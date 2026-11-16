use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point as TermPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{TermMode, viewport_to_point};
use gpui::{
    Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, ScrollWheelEvent,
    ShapedLine, SharedString, TextRun, UTF16Selection, Window, canvas, div, fill, font, point,
    prelude::*, px, rgb, rgba, size,
};
use nosh_desktop::connections::{self, Connection, Settings};
use nosh_desktop::terminal::{BackendSettings, PtyEvent, TerminalBackend};

use crate::gpui_input::{Action, CloseTarget, InputState, Surface};
use crate::gpui_terminal;

const PAPER: u32 = 0xf3f3f3;
const INK: u32 = 0x202a35;
const MUTED: u32 = 0x6a7582;
const BORDER: u32 = 0xd6d6d6;
const GREEN: u32 = 0x14796b;
const BLUE: u32 = 0x0067c0;
const RED: u32 = 0xa44242;

#[derive(clap::Parser)]
#[command(about = "Native GPUI nosh workbench")]
pub struct Options {
    #[arg(long)]
    nosh: Option<PathBuf>,
    #[arg(long)]
    cwd: Option<PathBuf>,
}

enum SessionKind {
    Local,
    Ssh(String),
}

enum ProcessState {
    Running,
    Exited(i32),
    Closed,
}

struct Session {
    id: u64,
    title: String,
    kind: SessionKind,
    status: ProcessState,
    backend: TerminalBackend,
}

impl Session {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            SessionKind::Local => "LOCAL",
            SessionKind::Ssh(_) => "SSH",
        }
    }

    fn accent(&self) -> u32 {
        match self.kind {
            SessionKind::Local => GREEN,
            SessionKind::Ssh(_) => BLUE,
        }
    }

    fn identity(&self) -> String {
        match &self.kind {
            SessionKind::Local => "This computer".into(),
            SessionKind::Ssh(target) => target.clone(),
        }
    }

    fn status_label(&self) -> String {
        match (&self.status, &self.kind) {
            (ProcessState::Running, SessionKind::Local) => "Local process running".into(),
            (ProcessState::Running, SessionKind::Ssh(_)) => {
                "OpenSSH running / authentication unknown".into()
            }
            (ProcessState::Exited(code), _) => format!("Exited with code {code}"),
            (ProcessState::Closed, _) => "PTY closed".into(),
        }
    }

    fn status_color(&self) -> u32 {
        match self.status {
            ProcessState::Running => self.accent(),
            ProcessState::Exited(0) => MUTED,
            _ => RED,
        }
    }
}

pub struct Desktop {
    focus: FocusHandle,
    nosh: Result<PathBuf, String>,
    cwd: Result<PathBuf, String>,
    settings: Settings,
    settings_path: Option<PathBuf>,
    sessions: Vec<Session>,
    active: usize,
    next_id: u64,
    sender: Sender<(u64, PtyEvent)>,
    events: Receiver<(u64, PtyEvent)>,
    dirty: Arc<AtomicBool>,
    error: String,
    form_error: String,
    input: InputState,
    bounds: Bounds<Pixels>,
    cell: gpui::Size<Pixels>,
    selecting: bool,
    field_bounds: [Bounds<Pixels>; 6],
    field_layouts: [Option<ShapedLine>; 6],
    field_offsets: [Pixels; 6],
    connections_visible: bool,
}

impl Desktop {
    pub fn new(options: Options, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (sender, events) = mpsc::channel();
        let settings_path = dirs::config_dir().map(|p| p.join("nosh").join("desktop-gpui.json"));
        let loaded = settings_path.as_ref().map_or_else(
            || Err("No system configuration directory. Saving is disabled.".into()),
            |p| Settings::load(p),
        );
        let (settings, error, settings_path) = match loaded {
            Ok(settings) => (settings, String::new(), settings_path),
            Err(error) => (Settings::default(), error, None),
        };
        let cwd = options
            .cwd
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)
            .and_then(|path| {
                if path.is_absolute() {
                    Ok(path)
                } else {
                    std::env::current_dir().map(|cwd| cwd.join(path))
                }
            })
            .and_then(|path| {
                if path.is_dir() {
                    Ok(path)
                } else {
                    Err(std::io::Error::other("startup path is not a directory"))
                }
            })
            .map_err(|error| format!("Cannot open startup directory: {error}"));
        let mut desktop = Self {
            focus: cx.focus_handle(),
            nosh: connections::find_nosh(options.nosh),
            cwd,
            settings,
            settings_path,
            sessions: Vec::new(),
            active: 0,
            next_id: 1,
            sender,
            events,
            dirty: Arc::new(AtomicBool::new(true)),
            error,
            form_error: String::new(),
            input: InputState::default(),
            bounds: Bounds::default(),
            cell: size(px(9.), px(20.)),
            selecting: false,
            field_bounds: Default::default(),
            field_layouts: Default::default(),
            field_offsets: [px(0.); 6],
            connections_visible: true,
        };
        desktop.open_local(cx);
        window.focus(&desktop.focus);
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            view.update(cx, |this, cx| {
                this.input.begin_close(CloseTarget::Window);
                this.selecting = false;
                window.focus(&this.focus);
                cx.notify();
            })
            .is_err()
        });
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                if view
                    .update(cx, |this, cx| {
                        if this.dirty.swap(false, Ordering::AcqRel) {
                            this.pump();
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        desktop
    }

    fn pump(&mut self) {
        while let Ok((id, event)) = self.events.try_recv() {
            let Some(session) = self.sessions.iter_mut().find(|t| t.id == id) else {
                continue;
            };
            match event {
                PtyEvent::ChildExit(code) => session.status = ProcessState::Exited(code),
                PtyEvent::Exit if matches!(session.status, ProcessState::Running) => {
                    session.status = ProcessState::Closed
                }
                _ => {}
            }
        }
    }

    fn open(
        &mut self,
        title: String,
        kind: SessionKind,
        settings: BackendSettings,
        cx: &mut Context<Self>,
    ) {
        if self.input.closing.is_some() {
            return;
        }
        let dirty = self.dirty.clone();
        match TerminalBackend::with_wake(
            self.next_id,
            Arc::new(move || {
                dirty.store(true, Ordering::Release);
            }),
            self.sender.clone(),
            settings,
        ) {
            Ok(backend) => {
                self.sessions.push(Session {
                    id: self.next_id,
                    title,
                    kind,
                    status: ProcessState::Running,
                    backend,
                });
                self.next_id += 1;
                self.active = self.sessions.len() - 1;
                self.input.set_surface(Surface::Terminal);
                self.selecting = false;
            }
            Err(error) => self.report_error(format!("Cannot start terminal: {error}")),
        }
        cx.notify();
    }

    fn open_local(&mut self, cx: &mut Context<Self>) {
        match (&self.nosh, &self.cwd) {
            (Ok(nosh), Ok(cwd)) => self.open(
                format!("Local {}", self.next_id),
                SessionKind::Local,
                connections::local_backend(nosh, cwd),
                cx,
            ),
            (Err(error), _) | (_, Err(error)) => {
                self.error = error.clone();
                cx.notify();
            }
        }
    }

    fn open_ssh(&mut self, connection: Connection, save: bool, cx: &mut Context<Self>) {
        if self.input.closing.is_some() {
            return;
        }
        match connections::ssh_backend(&connection) {
            Ok(backend) => {
                if save {
                    let mut settings = self.settings.clone();
                    settings.connections.push(connection.clone());
                    let result = self
                        .settings_path
                        .as_ref()
                        .ok_or_else(|| {
                            "Saving is disabled. Choose Connect without saving.".to_string()
                        })
                        .and_then(|path| settings.save(path));
                    if let Err(error) = result {
                        self.report_error(error);
                        cx.notify();
                        return;
                    }
                    self.settings = settings;
                }
                self.open(
                    connection.name.clone(),
                    SessionKind::Ssh(connection.target()),
                    backend,
                    cx,
                );
            }
            Err(error) => {
                self.report_error(error);
                cx.notify();
            }
        }
    }

    fn connect_form(&mut self, save: bool, cx: &mut Context<Self>) {
        if !self.input.field_enabled() {
            return;
        }
        self.form_error.clear();
        let fields = &self.input.fields;
        let port = if fields[3].text.is_empty() {
            None
        } else {
            match fields[3].text.parse::<u16>() {
                Ok(port) if port != 0 => Some(port),
                _ => {
                    self.form_error = "Port must be between 1 and 65535.".into();
                    self.input.field = 3;
                    cx.notify();
                    return;
                }
            }
        };
        let host = fields[1].text.trim().to_owned();
        if host.is_empty() {
            self.form_error = "Enter a host or an SSH config alias.".into();
            self.input.field = 1;
            cx.notify();
            return;
        }
        let connection = Connection {
            name: if fields[0].text.trim().is_empty() {
                host.clone()
            } else {
                fields[0].text.trim().to_owned()
            },
            host,
            user: fields[2].text.clone(),
            port,
            identity: fields[4].text.clone(),
            jump: fields[5].text.clone(),
        };
        self.open_ssh(connection, save, cx);
    }

    fn switch_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.closing.is_some() || index >= self.sessions.len() {
            return;
        }
        self.active = index;
        self.input.set_surface(Surface::Terminal);
        self.selecting = false;
        window.focus(&self.focus);
        cx.notify();
    }

    fn show_surface(&mut self, surface: Surface, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.closing.is_some() {
            return;
        }
        self.input.set_surface(surface);
        self.selecting = false;
        if surface == Surface::Connection {
            self.input.field = 1;
            self.form_error.clear();
        }
        window.focus(&self.focus);
        cx.notify();
    }

    fn request_close(&mut self, target: CloseTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.input.begin_close(target);
        self.selecting = false;
        window.focus(&self.focus);
        cx.notify();
    }

    fn confirm_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(prompt) = self.input.closing.take() {
            match prompt.target {
                CloseTarget::Window => {
                    self.sessions.clear();
                    window.remove_window();
                    cx.quit();
                }
                CloseTarget::Tab(id) => {
                    if let Some(index) = self.sessions.iter().position(|session| session.id == id) {
                        self.sessions.remove(index);
                        if index < self.active {
                            self.active -= 1;
                        }
                        self.active = self.active.min(self.sessions.len().saturating_sub(1));
                        if self.sessions.is_empty() {
                            self.input.set_surface(Surface::Board);
                        }
                    }
                    window.focus(&self.focus);
                }
            }
        }
        cx.notify();
    }

    fn cancel_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.closing = None;
        window.focus(&self.focus);
        cx.notify();
    }

    fn mode(&self) -> TermMode {
        self.sessions
            .get(self.active)
            .map(|s| *s.backend.model().lock().mode())
            .unwrap_or_default()
    }

    fn report_error(&mut self, error: String) {
        if self.input.surface == Surface::Connection {
            self.form_error = error;
        } else {
            self.error = error;
        }
    }

    fn write(&mut self, bytes: Vec<u8>) {
        // All keyboard, clipboard and native text paths share this final PTY gate.
        if !self.input.terminal_enabled() {
            return;
        }
        if let Some(session) = self.sessions.get(self.active) {
            if !matches!(session.status, ProcessState::Running) {
                self.error = "This session has ended. Open a new session to continue.".into();
            } else if let Err(error) = session.backend.write(bytes) {
                self.error = format!("Terminal input: {error}");
            } else {
                session
                    .backend
                    .model()
                    .lock()
                    .scroll_display(Scroll::Bottom);
            }
        }
    }

    fn action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Action::None => {}
            Action::Write(bytes) => self.write(bytes),
            Action::Copy(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            Action::CopyTerminal => {
                if self.input.terminal_enabled()
                    && let Some(text) = self
                        .sessions
                        .get(self.active)
                        .and_then(|s| s.backend.model().lock().selection_to_string())
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            Action::Paste => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    match self.input.paste(&text, self.mode()) {
                        Ok(action) => self.action(action, window, cx),
                        Err(error) => self.report_error(error),
                    }
                }
            }
            Action::NewLocal => self.open_local(cx),
            Action::NewConnection => self.show_surface(Surface::Connection, window, cx),
            Action::ToggleConnections => {
                self.connections_visible = !self.connections_visible;
                cx.notify();
            }
            Action::SwitchTab(reverse) if !self.sessions.is_empty() => {
                self.switch_session(
                    (self.active + if reverse { self.sessions.len() - 1 } else { 1 })
                        % self.sessions.len(),
                    window,
                    cx,
                );
            }
            Action::SwitchTab(_) => {}
            Action::RequestClose => {
                if let Some(session) = self.sessions.get(self.active) {
                    self.request_close(CloseTarget::Tab(session.id), window, cx);
                }
            }
            Action::ConfirmClose => self.confirm_close(window, cx),
            Action::CancelClose => self.cancel_close(window, cx),
            Action::Connect => self.connect_form(false, cx),
            Action::SaveAndConnect => self.connect_form(true, cx),
            Action::ReturnToTerminal => self.show_surface(Surface::Terminal, window, cx),
            Action::Zoom(delta) => {
                self.settings.font_size = if delta == 0 {
                    14.
                } else {
                    (self.settings.font_size + f32::from(delta)).clamp(10., 24.)
                };
                if let Some(path) = &self.settings_path
                    && let Err(error) = self.settings.save(path)
                {
                    self.report_error(error);
                }
            }
        }
        cx.notify();
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let action = self.input.key(event, self.mode());
        let handled = self.input.closing.is_some()
            || self.input.surface != Surface::Terminal
            || action != Action::None;
        self.action(action, window, cx);
        if handled {
            cx.stop_propagation();
        }
    }

    fn commit(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.input.commit(range, text) {
            Ok(action) => self.action(action, window, cx),
            Err(error) => {
                self.report_error(error);
                cx.notify();
            }
        }
    }

    fn mouse(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input.terminal_enabled() {
            return;
        }
        window.focus(&self.focus);
        self.selecting = true;
        self.select(event.position, true);
        cx.notify();
    }

    fn select(&mut self, pos: Point<Pixels>, start: bool) {
        if !self.input.terminal_enabled() {
            return;
        }
        if let Some(session) = self.sessions.get(self.active) {
            let mut term = session.backend.model().lock();
            let col = ((pos.x - self.bounds.left()) / self.cell.width).max(0.) as usize;
            let row = ((pos.y - self.bounds.top()) / self.cell.height).max(0.) as usize;
            let point = viewport_to_point(
                term.grid().display_offset(),
                TermPoint::new(
                    row.min(term.screen_lines() - 1),
                    Column(col.min(term.columns() - 1)),
                ),
            );
            if start {
                term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
            } else if let Some(selection) = &mut term.selection {
                selection.update(point, Side::Right);
            }
        }
    }

    fn prepare_terminal(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bounds = bounds;
        self.cell = gpui_terminal::cell_size(self.settings.font_size, window);
        if let Some(session) = self.sessions.get_mut(self.active)
            && let Err(error) = session.backend.resize(
                (bounds.size.width / self.cell.width) as u16,
                (bounds.size.height / self.cell.height) as u16,
                f32::from(self.cell.width) as u16,
                f32::from(self.cell.height) as u16,
            )
        {
            self.error = format!("Resize terminal: {error}");
            cx.notify();
        }
    }

    fn button(
        label: impl Into<SharedString>,
        id: impl Into<gpui::ElementId>,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .px_3()
            .h(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
            .border_1()
            .border_color(rgb(if primary { BLUE } else { BORDER }))
            .bg(rgb(if primary { BLUE } else { 0xffffff }))
            .text_color(rgb(if primary { 0xffffff } else { INK }))
            .hover(move |style| style.bg(rgb(if primary { 0x005aab } else { 0xe9e9e9 })))
            .active(move |style| style.bg(rgb(if primary { 0x004e92 } else { 0xdedede })))
            .cursor_pointer()
            .child(label.into())
    }

    fn tag(label: impl Into<SharedString>, color: u32) -> gpui::Div {
        div()
            .px_1()
            .text_size(px(10.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(rgb(color))
            .child(label.into())
    }

    fn header(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .h(px(38.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(0xe9e9e9))
            .child(
                Self::button(
                    if self.connections_visible {
                        "Hide connections"
                    } else {
                        "Connections"
                    },
                    "connections-toggle",
                    false,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.action(Action::ToggleConnections, window, cx)
                })),
            )
            .child(self.session_tabs(cx))
            .child(
                Self::button("+", "new-session", false)
                    .on_click(cx.listener(|this, _, _, cx| this.open_local(cx))),
            )
            .child(
                Self::button("SSH...", "new-ssh", false).on_click(cx.listener(
                    |this, _, window, cx| this.show_surface(Surface::Connection, window, cx),
                )),
            )
    }

    fn connection_list(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let folder = self
            .cwd
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "Startup directory unavailable".into());
        let mut saved = div().flex().flex_col().gap_1();
        if self.settings.connections.is_empty() {
            saved = saved.child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child("No saved connections"),
            );
        }
        for (index, connection) in self.settings.connections.iter().enumerate() {
            let connection = connection.clone();
            saved = saved.child(
                div()
                    .id(("saved", index))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .hover(|style| style.bg(rgb(0xe4e4e4)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_ssh(connection.clone(), false, cx);
                        window.focus(&this.focus);
                    }))
                    .child(Self::tag("SSH", BLUE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.settings.connections[index].name.clone())
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(MUTED))
                                    .truncate()
                                    .child(self.settings.connections[index].target()),
                            ),
                    ),
            );
        }
        div()
            .id("connection-list")
            .w(px(208.))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PAPER))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(36.))
                    .flex_shrink_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Connections"),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("LOCAL"),
            )
            .child(
                div()
                    .id("local-connection")
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0xe4e4e4)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_local(cx);
                        window.focus(&this.focus);
                    }))
                    .child(div().child("This computer"))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .truncate()
                            .child(folder),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pt_4()
                    .pb_1()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("SSH"),
            )
            .child(saved)
            .child(div().px_3().py_2().child(
                Self::button("New connection...", "list-new-ssh", false).on_click(cx.listener(
                    |this, _, window, cx| this.show_surface(Surface::Connection, window, cx),
                )),
            ))
            .child(div().flex_1())
            .child(
                div()
                    .p_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("Ctrl+Shift+T  Local terminal")
                    .child(div().child("Ctrl+Shift+N  SSH connection"))
                    .child(div().child("Ctrl+Shift+B  Connections")),
            )
    }

    fn stage(&self, cx: &mut Context<Self>) -> gpui::Div {
        let view = cx.entity();
        let paint_view = view.clone();
        let input_view = view.clone();
        let Some(session) = self.sessions.get(self.active) else {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(rgb(0x1c1e26))
                .text_color(rgb(0xc9ccd4))
                .child(div().text_size(px(18.)).child("No open sessions"))
                .child(div().text_color(rgb(MUTED)).child(
                    "Ctrl+Shift+T  New local terminal    /    Ctrl+Shift+N  Connect via SSH",
                ))
                .child(
                    Self::button("New local terminal", "empty-local", true)
                        .on_click(cx.listener(|this, _, _, cx| this.open_local(cx))),
                );
        };
        let identity = session.identity();
        let meta = match session.kind {
            SessionKind::Local => self
                .cwd
                .as_ref()
                .map(|cwd| cwd.display().to_string())
                .unwrap_or_default(),
            SessionKind::Ssh(_) => "OpenSSH".into(),
        };
        let status = session.status_label();
        let status_color = session.status_color();
        let is_exited = !matches!(session.status, ProcessState::Running);
        let terminal = div()
            .id("terminal")
            .flex_1()
            .min_h_0()
            .p_2()
            .overflow_hidden()
            .bg(rgb(0x1c1e26))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if this.selecting {
                    this.select(event.position, false);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                if this.input.terminal_enabled()
                    && let Some(session) = this.sessions.get(this.active)
                {
                    let lines = event.delta.pixel_delta(this.cell.height).y / this.cell.height;
                    session
                        .backend
                        .model()
                        .lock()
                        .scroll_display(Scroll::Delta(lines.round() as i32));
                    cx.notify();
                }
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        view.update(cx, |this, cx| this.prepare_terminal(bounds, window, cx))
                    },
                    move |bounds, (), window, cx| {
                        let focus = paint_view.read(cx).focus.clone();
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, input_view),
                            cx,
                        );
                        paint_view.update(cx, |this, cx| {
                            if let Some(session) = this.sessions.get(this.active)
                                && let Err(error) = gpui_terminal::paint(
                                    &session.backend,
                                    bounds,
                                    this.cell,
                                    this.settings.font_size,
                                    window,
                                    cx,
                                )
                            {
                                this.error = format!("Draw terminal: {error:#}");
                                cx.notify();
                            }
                        });
                    },
                )
                .size_full(),
            );
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(PAPER))
            .overflow_hidden()
            .child(
                div()
                    .h(px(36.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(Self::tag(session.kind_label(), session.accent()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .truncate()
                            .child(format!("{identity}  |  {meta}")),
                    )
                    .child(Self::button("Copy", "copy", false).on_click(cx.listener(
                        |this, _, window, cx| this.action(Action::CopyTerminal, window, cx),
                    )))
                    .child(
                        Self::button("Paste", "paste", false)
                            .when(is_exited, |button| {
                                button.opacity(0.45).cursor(gpui::CursorStyle::Arrow)
                            })
                            .when(!is_exited, |button| {
                                button.on_click(cx.listener(|this, _, window, cx| {
                                    this.action(Action::Paste, window, cx)
                                }))
                            }),
                    )
                    .child(Self::button("-", "zoom-out", false).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.action(Action::Zoom(-1), window, cx)
                        }),
                    ))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(format!("{} px", self.settings.font_size)),
                    )
                    .child(Self::button("+", "zoom-in", false).on_click(
                        cx.listener(|this, _, window, cx| this.action(Action::Zoom(1), window, cx)),
                    )),
            )
            .when(is_exited, |stage| {
                stage.child(
                    div()
                        .px_3()
                        .py_1()
                        .bg(rgb(0xfcf0ec))
                        .text_color(rgb(RED))
                        .text_size(px(12.))
                        .child("Session ended. Output preserved."),
                )
            })
            .child(terminal)
            .child(
                div()
                    .h(px(24.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px_3()
                    .gap_2()
                    .bg(rgb(PAPER))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(rgb(if status_color == RED {
                                0xe7a29a
                            } else {
                                0x8dd4c0
                            })),
                    )
                    .child(status)
                    .child(div().flex_1())
                    .child(if self.input.terminal_enabled() {
                        "GPUI"
                    } else {
                        "Input paused"
                    }),
            )
    }

    fn session_tabs(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let mut cards = div()
            .id("session-tabs")
            .flex()
            .flex_1()
            .min_w_0()
            .overflow_x_scroll()
            .h_full();
        for (index, session) in self.sessions.iter().enumerate() {
            let id = session.id;
            let selected = self.active == index && self.input.surface == Surface::Terminal;
            cards = cards.child(
                div()
                    .id(("session", id))
                    .w(px(180.))
                    .flex_shrink_0()
                    .px_2()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_b_2()
                    .border_color(rgb(if selected { BLUE } else { BORDER }))
                    .bg(rgb(if selected { PAPER } else { 0xe9e9e9 }))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0xffffff)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.switch_session(index, window, cx)
                    }))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .w_full()
                            .child(
                                div()
                                    .size(px(6.))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .bg(rgb(session.status_color())),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .truncate()
                                    .child(format!("{}  {}", session.kind_label(), session.title)),
                            )
                            .child(
                                div()
                                    .id(("close-session", id))
                                    .text_color(rgb(MUTED))
                                    .px_2()
                                    .rounded(px(2.))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(0xdcdcdc)).text_color(rgb(INK)))
                                    .child("x")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.request_close(CloseTarget::Tab(id), window, cx);
                                    })),
                            ),
                    ),
            );
        }
        cards
    }

    fn field(
        &self,
        index: usize,
        label: &'static str,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let view = cx.entity();
        let paint_view = view.clone();
        let input_view = view.clone();
        let focused = self.input.field_enabled() && self.input.field == index;
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(MUTED))
                    .child(label),
            )
            .child(
                div()
                    .id(("connection-field", index))
                    .h(px(30.))
                    .px_2()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(rgb(if focused { BLUE } else { BORDER }))
                    .bg(rgb(0xffffff))
                    .overflow_hidden()
                    .cursor(gpui::CursorStyle::IBeam)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            if !this.input.field_enabled() {
                                return;
                            }
                            this.input.field = index;
                            let offset = this.field_layouts[index]
                                .as_ref()
                                .map(|line| {
                                    line.closest_index_for_x(
                                        event.position.x - this.field_bounds[index].left()
                                            + this.field_offsets[index],
                                    )
                                })
                                .unwrap_or(0);
                            this.input.fields[index].move_cursor(offset, event.modifiers.shift);
                            window.focus(&this.focus);
                            cx.notify();
                        }),
                    )
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                view.update(cx, |this, _| {
                                    this.field_bounds[index] = bounds;
                                    let editor = &this.input.fields[index];
                                    let text: SharedString = if editor.text.is_empty() {
                                        placeholder.into()
                                    } else {
                                        editor.text.clone().into()
                                    };
                                    let line = window.text_system().shape_line(
                                        text.clone(),
                                        px(14.),
                                        &[TextRun {
                                            len: text.len(),
                                            font: font("Segoe UI"),
                                            color: rgb(if editor.text.is_empty() {
                                                0x8b949f
                                            } else {
                                                INK
                                            })
                                            .into(),
                                            background_color: None,
                                            underline: None,
                                            strikethrough: None,
                                        }],
                                        None,
                                    );
                                    this.field_offsets[index] = (line.x_for_index(editor.cursor)
                                        - bounds.size.width
                                        + px(4.))
                                    .max(px(0.));
                                    this.field_layouts[index] = Some(line.clone());
                                    line
                                })
                            },
                            move |bounds, line, window, cx| {
                                let focus = paint_view.read(cx).focus.clone();
                                if focused {
                                    window.handle_input(
                                        &focus,
                                        ElementInputHandler::new(bounds, input_view),
                                        cx,
                                    );
                                }
                                paint_view.update(cx, |this, cx| {
                                    let editor = &this.input.fields[index];
                                    let origin =
                                        bounds.origin + point(-this.field_offsets[index], px(3.));
                                    let selection = editor.selection();
                                    if focused && !selection.is_empty() {
                                        let start = line.x_for_index(selection.start);
                                        let end = line.x_for_index(selection.end);
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                origin + point(start, px(0.)),
                                                size(end - start, px(22.)),
                                            ),
                                            rgb(0xdde8fa),
                                        ));
                                    }
                                    if let Err(error) = line.paint(origin, px(22.), window, cx) {
                                        eprintln!("nosh-gpui: draw connection field: {error:#}");
                                    }
                                    if focused {
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                origin
                                                    + point(
                                                        line.x_for_index(editor.cursor),
                                                        px(0.),
                                                    ),
                                                size(px(1.5), px(22.)),
                                            ),
                                            rgb(BLUE),
                                        ));
                                        if let Some(marked) = &editor.marked {
                                            window.paint_quad(fill(
                                                Bounds::new(
                                                    origin
                                                        + point(
                                                            line.x_for_index(marked.start),
                                                            px(21.),
                                                        ),
                                                    size(
                                                        line.x_for_index(marked.end)
                                                            - line.x_for_index(marked.start),
                                                        px(1.),
                                                    ),
                                                ),
                                                rgb(BLUE),
                                            ));
                                        }
                                    }
                                });
                            },
                        )
                        .size_full(),
                    ),
            )
    }

    fn connection_drawer(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .absolute()
            .inset_0()
            .p_4()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .occlude()
            .child(
                div()
                    .w(px(480.))
                    .h_full()
                    .max_h(px(530.))
                    .rounded(px(5.))
                    .bg(rgb(PAPER))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(46.))
                            .flex_shrink_0()
                            .px_4()
                            .flex()
                            .items_center()
                            .gap_2()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(16.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("SSH connection"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(MUTED))
                                    .child("System OpenSSH"),
                            ),
                    )
                    .child(
                        div()
                            .id("connection-fields")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_4()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(self.field(1, "Host or SSH alias", "Required", cx))
                            .child(self.field(0, "Name", "Optional; defaults to host", cx))
                            .child(
                                div()
                                    .flex()
                                    .gap_3()
                                    .child(self.field(2, "User", "SSH config default", cx))
                                    .child(self.field(3, "Port", "SSH config default", cx)),
                            )
                            .child(self.field(4, "Identity file", "Optional private key path", cx))
                            .child(self.field(5, "Jump host", "Optional user@jump:port", cx))
                            .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(
                                "Host verification on. Passwords are entered in the terminal.",
                            ))
                            .when(!self.error.is_empty(), |drawer| {
                                drawer.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(RED))
                                        .child(self.error.clone()),
                                )
                            })
                            .when(!self.form_error.is_empty(), |dialog| {
                                dialog.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(RED))
                                        .child(self.form_error.clone()),
                                )
                            }),
                    )
                    .child(
                        div()
                            .p_3()
                            .border_t_1()
                            .border_color(rgb(BORDER))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .gap_2()
                                    .child(
                                        Self::button("Cancel", "cancel-connection", false)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.show_surface(Surface::Terminal, window, cx)
                                            })),
                                    )
                                    .child(
                                        Self::button("Save & connect", "save-connect", false)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.connect_form(true, cx)
                                            })),
                                    )
                                    .child(Self::button("Connect", "connect", true).on_click(
                                        cx.listener(|this, _, _, cx| this.connect_form(false, cx)),
                                    )),
                            )
                            .child(div().text_size(px(10.)).text_color(rgb(MUTED)).child(
                                "Enter: connect   Ctrl+Enter: save & connect   Esc: cancel",
                            )),
                    ),
            )
    }

    fn close_dialog(&self, cx: &mut Context<Self>) -> gpui::Div {
        let prompt = self
            .input
            .closing
            .as_ref()
            .expect("dialog requires close state");
        let (title, description, confirm) = match prompt.target {
            CloseTarget::Window => (
                "Close the workbench?",
                "Closing the workbench stops every local PTY process. Unsaved terminal work may be lost; remote programs may keep running.".to_owned(),
                "Close all",
            ),
            CloseTarget::Tab(id) => {
                let name = self.sessions.iter().find(|session| session.id == id)
                    .map(|s| s.title.as_str()).unwrap_or("this session");
                (
                    "Close this session?",
                    format!("Closing {name} stops its local PTY process. Unsaved terminal work may be lost; remote programs may keep running."),
                    "Close session",
                )
            }
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x15212c99))
            .occlude()
            .child(
                div()
                    .w(px(440.))
                    .p_5()
                    .rounded(px(5.))
                    .bg(rgb(PAPER))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(description)
                    .child(
                        div().text_size(px(11.)).text_color(rgb(MUTED)).child(
                            "Input paused. Enter / Esc: keep session. Tab: change selection.",
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Self::button(
                                    "Keep session",
                                    "keep-session",
                                    !prompt.confirm_selected,
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.cancel_close(window, cx)
                                    }),
                                ),
                            )
                            .child(
                                Self::button(confirm, "confirm-close", prompt.confirm_selected)
                                    .text_color(rgb(if prompt.confirm_selected {
                                        0xffffff
                                    } else {
                                        RED
                                    }))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_close(window, cx)
                                    })),
                            ),
                    ),
            )
    }
}

impl Render for Desktop {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(PAPER))
            .text_color(rgb(INK))
            .font_family("Segoe UI")
            .text_size(px(13.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .child(self.header(cx));
        if !self.error.is_empty() && self.input.surface != Surface::Connection {
            root =
                root.child(
                    div()
                        .px_3()
                        .py_1()
                        .flex()
                        .gap_3()
                        .items_center()
                        .bg(rgb(0xf8e8e3))
                        .text_color(rgb(RED))
                        .child(div().flex_1().child(self.error.clone()))
                        .child(Self::button("Dismiss", "dismiss-error", false).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.error.clear();
                                cx.notify();
                            }),
                        )),
                );
        }
        root = root.child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .when(self.connections_visible, |body| {
                    body.child(self.connection_list(cx))
                })
                .child(self.stage(cx)),
        );
        // Empty workspaces also route late native text commits through the input policy.
        if self.input.surface == Surface::Board {
            let view = cx.entity();
            root = root.child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, cx| {
                        let focus = view.read(cx).focus.clone();
                        window.handle_input(&focus, ElementInputHandler::new(bounds, view), cx);
                    },
                )
                .absolute()
                .size(px(0.)),
            );
        }
        if self.input.surface == Surface::Connection {
            root = root.child(self.connection_drawer(cx));
        }
        if self.input.closing.is_some() {
            root = root.child(self.close_dialog(cx));
        }
        root
    }
}

impl EntityInputHandler for Desktop {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        if !self.input.field_enabled() {
            return None;
        }
        let editor = &self.input.fields[self.input.field];
        let range = editor.byte_range(range);
        *actual = Some(
            editor.text[..range.start].encode_utf16().count()
                ..editor.text[..range.end].encode_utf16().count(),
        );
        Some(editor.text[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if self.input.field_enabled() {
            let editor = &self.input.fields[self.input.field];
            Some(UTF16Selection {
                range: editor.utf16_selection(),
                reversed: editor.cursor < editor.anchor,
            })
        } else if self.input.terminal_enabled() {
            Some(UTF16Selection {
                range: 0..0,
                reversed: false,
            })
        } else {
            None
        }
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        if self.input.field_enabled() {
            let editor = &self.input.fields[self.input.field];
            editor.marked.as_ref().map(|range| {
                editor.text[..range.start].encode_utf16().count()
                    ..editor.text[..range.end].encode_utf16().count()
            })
        } else if self.input.terminal_enabled() && !self.input.terminal_composition.is_empty() {
            Some(0..self.input.terminal_composition.encode_utf16().count())
        } else {
            None
        }
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.input.field_enabled() {
            self.input.fields[self.input.field].marked = None;
        }
        self.input.terminal_composition.clear();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit(range, text, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.input.mark(range, text) {
            self.report_error(error);
        }
        if self.input.field_enabled()
            && let Some(selected) = selected
        {
            let editor = &mut self.input.fields[self.input.field];
            if let Some(marked) = &editor.marked {
                let prefix = editor.text[..marked.start].encode_utf16().count();
                let range = editor.byte_range(prefix + selected.start..prefix + selected.end);
                editor.anchor = range.start;
                editor.cursor = range.end;
            }
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if self.input.field_enabled() {
            let index = self.input.field;
            let line = self.field_layouts[index].as_ref()?;
            let range = self.input.fields[index].byte_range(range);
            let origin = self.field_bounds[index].origin
                + point(
                    line.x_for_index(range.start) - self.field_offsets[index],
                    px(3.),
                );
            Some(Bounds::new(
                origin,
                size(
                    (line.x_for_index(range.end) - line.x_for_index(range.start)).max(px(1.)),
                    px(22.),
                ),
            ))
        } else if self.input.terminal_enabled() {
            let session = self.sessions.get(self.active)?;
            let cursor = session.backend.model().lock().grid().cursor.point;
            Some(Bounds::new(
                self.bounds.origin
                    + point(
                        self.cell.width * cursor.column.0 as f32,
                        self.cell.height * cursor.line.0.max(0) as f32,
                    ),
                self.cell,
            ))
        } else {
            None
        }
    }

    fn character_index_for_point(
        &mut self,
        pos: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        if !self.input.field_enabled() {
            return None;
        }
        let index = self.input.field;
        let byte = self.field_layouts[index].as_ref()?.closest_index_for_x(
            pos.x - self.field_bounds[index].left() + self.field_offsets[index],
        );
        Some(
            self.input.fields[index].text[..byte.min(self.input.fields[index].text.len())]
                .encode_utf16()
                .count(),
        )
    }
}
