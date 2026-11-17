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
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, ScrollHandle,
    ScrollWheelEvent, ShapedLine, SharedString, TextRun, UTF16Selection, Window, canvas, div, fill,
    font, point, prelude::*, px, rgb, rgba, size,
};
use nosh_desktop::connections::{self, Connection, Settings};
use nosh_desktop::terminal::{BackendSettings, PtyEvent, TerminalBackend};

use crate::gpui_input::{Action, CloseTarget, ConnectionButton, EditorTarget, InputState, Surface};
use crate::gpui_launcher::{LaunchAction, LaunchItem};
use crate::gpui_terminal;

const CHROME: u32 = 0x171b23;
const SURFACE: u32 = 0x222730;
const RAISED: u32 = 0x2a303b;
const INPUT: u32 = 0x1a1e27;
const HOVER: u32 = 0x303947;
const INK: u32 = 0xe8ecf1;
const MUTED: u32 = 0xa1adbc;
const BORDER: u32 = 0x353d49;
const GREEN: u32 = 0x84d8bd;
const BLUE: u32 = 0x91b8ee;
const RED: u32 = 0xffa4ae;
const ERROR_SURFACE: u32 = 0x3c2730;

enum ButtonStyle {
    Standard,
    Primary,
    Danger,
}

#[derive(clap::Parser)]
#[command(about = "Native GPUI nosh workbench")]
pub struct Options {
    #[arg(long)]
    nosh: Option<PathBuf>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long, help = "Use an isolated desktop settings file")]
    settings: Option<PathBuf>,
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
    field_bounds: [Bounds<Pixels>; 7],
    field_layouts: [Option<ShapedLine>; 7],
    field_offsets: [Pixels; 7],
    editing_connection: Option<usize>,
    launcher_query: String,
    launcher_selected: usize,
    launcher_scroll: ScrollHandle,
    reveal_launcher: bool,
    connections_visible: Option<bool>,
    tabs_scroll: ScrollHandle,
    reveal_active_tab: bool,
    viewport_width: Pixels,
    tabs_overflow: bool,
}

impl Desktop {
    pub fn new(options: Options, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (sender, events) = mpsc::channel();
        let settings_path = options
            .settings
            .or_else(|| dirs::config_dir().map(|p| p.join("nosh").join("desktop-gpui.json")));
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
            field_offsets: [px(0.); 7],
            editing_connection: None,
            launcher_query: String::new(),
            launcher_selected: 0,
            launcher_scroll: ScrollHandle::new(),
            reveal_launcher: false,
            connections_visible: None,
            tabs_scroll: ScrollHandle::new(),
            reveal_active_tab: true,
            viewport_width: px(0.),
            tabs_overflow: false,
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
                self.reveal_active_tab = true;
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
                    if let Some(index) = self.editing_connection {
                        settings.connections[index] = connection.clone();
                    } else {
                        settings.connections.push(connection.clone());
                    }
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
        if !self.input.connection_enabled() {
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
                    self.input.form_button = None;
                    cx.notify();
                    return;
                }
            }
        };
        let host = fields[1].text.trim().to_owned();
        if host.is_empty() {
            self.form_error = "Enter a host or an SSH config alias.".into();
            self.input.field = 1;
            self.input.form_button = None;
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
        self.reveal_active_tab = true;
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
            self.editing_connection = None;
            self.input.fields = Default::default();
            self.input.field = 1;
            self.form_error.clear();
        } else if surface == Surface::Launcher {
            self.launcher_query.clear();
            self.launcher_selected = 0;
            self.reveal_launcher = true;
        }
        window.focus(&self.focus);
        cx.notify();
    }

    fn edit_connection(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input.background_enabled() {
            return;
        }
        let connection = self.settings.connections[index].clone();
        self.show_surface(Surface::Connection, window, cx);
        self.editing_connection = Some(index);
        let values = [
            connection.name,
            connection.host,
            connection.user,
            connection
                .port
                .map_or_else(String::new, |port| port.to_string()),
            connection.identity,
            connection.jump,
        ];
        for (editor, value) in self.input.fields.iter_mut().zip(values) {
            if let Err(error) = editor.replace(None, &value) {
                self.form_error = error;
                break;
            }
        }
        cx.notify();
    }

    fn launcher_items(&self) -> Vec<LaunchItem> {
        let mut items = vec![
            LaunchItem::new(
                "New local terminal",
                "Start nosh on this computer",
                "Create",
                ">_",
                "Ctrl+Shift+T",
                LaunchAction::Local,
            ),
            LaunchItem::new(
                "New SSH connection",
                "Review a target before connecting",
                "Create",
                "SSH",
                "Ctrl+Shift+N",
                LaunchAction::Connection,
            ),
        ];
        for session in &self.sessions {
            items.push(LaunchItem::new(
                session.title.clone(),
                format!("{} / {}", session.identity(), session.status_label()),
                "Switch session",
                session.kind_label(),
                "",
                LaunchAction::Session(session.id),
            ));
        }
        for (index, connection) in self.settings.connections.iter().enumerate() {
            items.push(LaunchItem::new(
                connection.name.clone(),
                connection.target(),
                "Saved connections",
                "SSH",
                "",
                LaunchAction::Saved(index),
            ));
        }
        if let Some(session) = self.sessions.get(self.active) {
            items.push(LaunchItem::new(
                "Copy terminal selection",
                "Copy only the selected terminal text",
                "Terminal",
                "C",
                "Ctrl+Shift+C",
                LaunchAction::Copy,
            ));
            if matches!(session.status, ProcessState::Running) {
                items.push(LaunchItem::new(
                    "Paste into terminal",
                    "Uses the terminal's bracketed-paste mode",
                    "Terminal",
                    "V",
                    "Ctrl+Shift+V",
                    LaunchAction::Paste,
                ));
            }
            items.push(LaunchItem::new(
                "Close current session",
                session.title.clone(),
                "Terminal",
                "\u{00d7}",
                "Ctrl+Shift+W",
                LaunchAction::Close(session.id),
            ));
        }
        for (label, shortcut, delta) in [
            ("Increase text size", "Ctrl++", 1),
            ("Decrease text size", "Ctrl+-", -1),
            ("Reset text size", "Ctrl+0", 0),
        ] {
            items.push(LaunchItem::new(
                label,
                "Zoom / terminal text size",
                "View",
                "Aa",
                shortcut,
                LaunchAction::Zoom(delta),
            ));
        }
        items.push(LaunchItem::new(
            "Toggle session navigator",
            "Show or hide the sidebar with sessions and saved connections",
            "View",
            "\u{2261}",
            "Ctrl+Shift+B",
            LaunchAction::Sidebar,
        ));
        items.retain(|item| item.matches(&self.input.launcher.text));
        items
    }

    fn activate_launcher(
        &mut self,
        action: LaunchAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.input.surface != Surface::Launcher || self.input.closing.is_some() {
            return;
        }
        self.input.set_surface(if self.sessions.is_empty() {
            Surface::Board
        } else {
            Surface::Terminal
        });
        window.focus(&self.focus);
        match action {
            LaunchAction::Local => self.open_local(cx),
            LaunchAction::Connection => self.show_surface(Surface::Connection, window, cx),
            LaunchAction::Saved(index) => self.edit_connection(index, window, cx),
            LaunchAction::Session(id) => {
                if let Some(index) = self.sessions.iter().position(|session| session.id == id) {
                    self.switch_session(index, window, cx);
                }
            }
            LaunchAction::Copy => self.action(Action::CopyTerminal, window, cx),
            LaunchAction::Paste => self.action(Action::Paste, window, cx),
            LaunchAction::Close(id) => self.request_close(CloseTarget::Tab(id), window, cx),
            LaunchAction::Zoom(delta) => self.action(Action::Zoom(delta), window, cx),
            LaunchAction::Sidebar => self.action(Action::ToggleConnections, window, cx),
        }
        cx.notify();
    }

    fn sync_launcher(&mut self) {
        if self.input.surface != Surface::Launcher {
            return;
        }
        if self.launcher_query != self.input.launcher.text {
            self.launcher_query.clone_from(&self.input.launcher.text);
            self.launcher_selected = 0;
            self.reveal_launcher = true;
        }
        let selected = self
            .launcher_selected
            .min(self.launcher_items().len().saturating_sub(1));
        if selected != self.launcher_selected {
            self.launcher_selected = selected;
            self.reveal_launcher = true;
        }
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
                        self.reveal_active_tab = true;
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
        self.sync_launcher();
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
            Action::OpenLauncher => self.show_surface(Surface::Launcher, window, cx),
            Action::MoveLauncher(reverse) => {
                let count = self.launcher_items().len();
                if count != 0 {
                    self.launcher_selected =
                        (self.launcher_selected + if reverse { count - 1 } else { 1 }) % count;
                    self.reveal_launcher = true;
                }
            }
            Action::ActivateLauncher => {
                if let Some(item) = self.launcher_items().get(self.launcher_selected) {
                    self.activate_launcher(item.action, window, cx);
                }
            }
            Action::ToggleConnections => {
                self.connections_visible = Some(!self.connections_visible(window));
                self.reveal_active_tab = true;
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
            Action::ReturnToTerminal => self.show_surface(
                if self.sessions.is_empty() {
                    Surface::Board
                } else {
                    Surface::Terminal
                },
                window,
                cx,
            ),
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
        Self::styled_button(
            label,
            id,
            if primary {
                ButtonStyle::Primary
            } else {
                ButtonStyle::Standard
            },
        )
    }

    fn styled_button(
        label: impl Into<SharedString>,
        id: impl Into<gpui::ElementId>,
        style: ButtonStyle,
    ) -> gpui::Stateful<gpui::Div> {
        let (background, border, foreground, hover, active) = match style {
            ButtonStyle::Standard => (RAISED, BORDER, INK, HOVER, 0x394655),
            ButtonStyle::Primary => (GREEN, GREEN, CHROME, 0x9be4ce, 0x6cc5a9),
            ButtonStyle::Danger => (0xb94858, 0xb94858, 0xffffff, 0xce5b6b, 0xa63a49),
        };
        div()
            .id(id)
            .px_3()
            .h(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(border))
            .bg(rgb(background))
            .text_color(rgb(foreground))
            .hover(move |style| style.bg(rgb(hover)))
            .active(move |style| style.bg(rgb(active)))
            .cursor_pointer()
            .child(label.into())
    }

    fn tag(label: impl Into<SharedString>, color: u32) -> gpui::Div {
        div()
            .px_1()
            .h(px(18.))
            .flex()
            .items_center()
            .rounded(px(4.))
            .bg(rgba((color << 8) | 0x18))
            .text_size(px(10.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(rgb(color))
            .child(label.into())
    }

    fn toolbar_button(
        label: impl Into<SharedString>,
        id: impl Into<gpui::ElementId>,
    ) -> gpui::Stateful<gpui::Div> {
        Self::button(label, id, false)
            .h(px(26.))
            .px_2()
            .text_size(px(12.))
            .text_color(rgb(MUTED))
            .border_color(rgba(0))
            .bg(rgba(0))
    }

    fn shortcut(label: &'static str, keys: &'static str) -> gpui::Div {
        div()
            .h(px(24.))
            .flex()
            .items_center()
            .justify_between()
            .text_size(px(11.))
            .text_color(rgb(MUTED))
            .child(label)
            .child(
                div()
                    .px_1()
                    .rounded(px(3.))
                    .bg(rgb(INPUT))
                    .font_family("Consolas")
                    .text_size(px(10.))
                    .child(keys),
            )
    }

    fn connections_visible(&self, window: &Window) -> bool {
        self.connections_visible
            .unwrap_or(window.viewport_size().width >= px(1000.))
    }

    #[cfg(target_os = "windows")]
    fn caption_buttons(&self, window: &Window) -> gpui::Div {
        let enabled = self.input.background_enabled();
        let mut buttons = div().flex().h_full().flex_shrink_0();
        for (area, glyph) in [
            (gpui::WindowControlArea::Min, "\u{e921}"),
            (
                gpui::WindowControlArea::Max,
                if window.is_maximized() {
                    "\u{e923}"
                } else {
                    "\u{e922}"
                },
            ),
            (gpui::WindowControlArea::Close, "\u{e8bb}"),
        ] {
            let close = area == gpui::WindowControlArea::Close;
            buttons = buttons.child(
                div()
                    .w(px(46.))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family("Segoe MDL2 Assets")
                    .text_size(px(10.))
                    .when(enabled, |button| {
                        button.window_control_area(area).hover(move |style| {
                            style
                                .bg(rgb(if close { 0xc42b1c } else { HOVER }))
                                .text_color(rgb(if close { 0xffffff } else { INK }))
                        })
                    })
                    .child(glyph),
            );
        }
        buttons
    }

    fn header(&self, _window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .h(px(40.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .pl_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(CHROME))
            .when(cfg!(target_os = "windows"), |header| {
                header.child(
                    div()
                        .w(px(32.))
                        .h_full()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(self.input.background_enabled(), |logo| {
                            logo.window_control_area(gpui::WindowControlArea::Drag)
                        })
                        .child(
                            div()
                                .size(px(26.))
                                .rounded(px(6.))
                                .bg(rgb(GREEN))
                                .text_color(rgb(CHROME))
                                .font_family("Consolas")
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_size(px(14.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child("n_"),
                        ),
                )
            })
            .child(
                Self::toolbar_button("\u{2630}", "connections-toggle")
                    .w(px(32.))
                    .px_0()
                    .text_size(px(16.))
                    .when(self.connections_visible(_window), |button| {
                        button.text_color(rgb(GREEN)).bg(rgba((GREEN << 8) | 0x18))
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.action(Action::ToggleConnections, window, cx)
                    })),
            )
            .child(self.session_tabs(cx))
            .when(self.tabs_scroll.max_offset().width > px(0.), |header| {
                header.children([("<", "previous-tab", true), (">", "next-tab", false)].map(
                    |(label, id, reverse)| {
                        Self::toolbar_button(label, id)
                            .w(px(24.))
                            .px_0()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.action(Action::SwitchTab(reverse), window, cx)
                            }))
                    },
                ))
            })
            .child(
                Self::button("+ New", "new-session", false)
                    .text_color(rgb(GREEN))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_surface(Surface::Launcher, window, cx)
                    })),
            )
            .when(cfg!(target_os = "windows"), |header| {
                header.child(
                    div()
                        .w(px(32.))
                        .h_full()
                        .flex_shrink_0()
                        .when(self.input.background_enabled(), |space| {
                            space.window_control_area(gpui::WindowControlArea::Drag)
                        }),
                )
            })
            .map(|header| {
                #[cfg(target_os = "windows")]
                {
                    header.child(self.caption_buttons(_window))
                }
                #[cfg(not(target_os = "windows"))]
                {
                    header.pr_2()
                }
            })
    }

    fn connection_list(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let mut saved = div().flex().flex_col().gap_1();
        if self.settings.connections.is_empty() {
            saved = saved.child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child("Saved SSH targets appear here"),
            );
        }
        for (index, connection) in self.settings.connections.iter().enumerate() {
            let selected = matches!(
                self.sessions.get(self.active).map(|session| &session.kind),
                Some(SessionKind::Ssh(target)) if target == &connection.target()
            );
            saved = saved.child(
                div()
                    .id(("saved", index))
                    .flex()
                    .items_center()
                    .gap_2()
                    .mx_2()
                    .px_2()
                    .py_2()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if selected { rgb(BORDER) } else { rgba(0) })
                    .bg(if selected { rgb(RAISED) } else { rgba(0) })
                    .hover(|style| style.bg(rgb(HOVER)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.edit_connection(index, window, cx);
                    }))
                    .child(Self::tag("SSH", BLUE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .child(self.settings.connections[index].name.clone()),
                            )
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
        let mut sessions = div().flex().flex_col().gap_1();
        for (index, session) in self.sessions.iter().enumerate() {
            let id = session.id;
            let active = index == self.active;
            sessions = sessions.child(
                div()
                    .id(("navigator-session", id))
                    .mx_2()
                    .px_2()
                    .py_2()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if active { rgb(0x3c5a50) } else { rgba(0) })
                    .bg(if active { rgb(0x293d39) } else { rgba(0) })
                    .hover(|style| style.bg(rgb(HOVER)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.switch_session(index, window, cx);
                    }))
                    .child(Self::tag(
                        if matches!(session.kind, SessionKind::Local) {
                            ">_"
                        } else {
                            "SSH"
                        },
                        session.accent(),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(session.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(MUTED))
                                    .truncate()
                                    .child(session.identity()),
                            ),
                    )
                    .child(
                        div()
                            .size(px(5.))
                            .rounded_full()
                            .bg(rgb(session.status_color())),
                    )
                    .child(
                        Self::toolbar_button("\u{00d7}", ("navigator-close", id))
                            .px_1()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.request_close(CloseTarget::Tab(id), window, cx);
                            })),
                    ),
            );
        }
        div()
            .id("connection-list")
            .w(px(232.))
            .flex_shrink_0()
            .h_full()
            .border_r_1()
            .border_color(rgb(BORDER))
            .bg(rgb(SURFACE))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(48.))
                    .flex_shrink_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(px(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Workspace")
                    .child(Self::tag(format!("{}", self.sessions.len()), MUTED)),
            )
            .child(
                div()
                    .px_3()
                    .pt_3()
                    .pb_2()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("SESSIONS"),
            )
            .child(
                div()
                    .id("navigator-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(sessions)
                    .child(
                        div().px_3().py_2().child(
                            Self::toolbar_button("+ New session", "navigator-new")
                                .text_color(rgb(GREEN))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.show_surface(Surface::Launcher, window, cx);
                                })),
                        ),
                    )
                    .child(
                        div()
                            .px_3()
                            .pt_4()
                            .pb_2()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child("SAVED CONNECTIONS")
                            .child(Self::toolbar_button("+", "add-saved").on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.show_surface(Surface::Connection, window, cx);
                                },
                            ))),
                    )
                    .child(saved),
            )
            .child(
                div()
                    .p_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(
                        Self::toolbar_button("Quick open...", "navigator-launcher")
                            .w_full()
                            .justify_between()
                            .child(Self::tag("Ctrl+Shift+P", MUTED))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_surface(Surface::Launcher, window, cx);
                            })),
                    )
                    .child(Self::shortcut("New local", "Ctrl+Shift+T"))
                    .child(Self::shortcut("New SSH", "Ctrl+Shift+N")),
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
                    Self::button("New session...", "empty-local", true).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.show_surface(Surface::Launcher, window, cx);
                        },
                    )),
                );
        };
        let identity = session.identity();
        let meta = match session.kind {
            SessionKind::Local => self
                .cwd
                .as_ref()
                .map(|cwd| format!("Startup: {}", cwd.display()))
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
            .bg(rgb(SURFACE))
            .overflow_hidden()
            .child(
                div()
                    .h(px(34.))
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
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(12.))
                            .child(
                                div()
                                    .max_w(px(220.))
                                    .truncate()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(identity),
                            )
                            .child(div().text_color(rgb(MUTED)).child("/"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(rgb(MUTED))
                                    .child(meta),
                            ),
                    )
                    .child(
                        Self::toolbar_button("Actions...", "session-actions").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.show_surface(Surface::Launcher, window, cx);
                            }),
                        ),
                    ),
            )
            .when(is_exited, |stage| {
                stage.child(
                    div()
                        .px_3()
                        .py_1()
                        .flex()
                        .items_center()
                        .justify_between()
                        .bg(rgb(ERROR_SURFACE))
                        .text_color(rgb(RED))
                        .text_size(px(12.))
                        .child("Session ended. Output preserved.")
                        .child(
                            Self::toolbar_button("New session...", "exited-new").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.show_surface(Surface::Launcher, window, cx);
                                }),
                            ),
                        ),
                )
            })
            .child(terminal)
            .child(
                div()
                    .h(px(30.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px_3()
                    .gap_2()
                    .bg(rgb(CHROME))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(div().size(px(6.)).rounded_full().bg(rgb(status_color)))
                    .child(status)
                    .child(div().flex_1())
                    .when(!self.input.terminal_enabled(), |bar| {
                        bar.child("Input paused")
                    })
                    .child(
                        Self::toolbar_button("-", "zoom-out")
                            .px_1()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.action(Action::Zoom(-1), window, cx);
                            })),
                    )
                    .child(format!("{} px", self.settings.font_size))
                    .child(
                        Self::toolbar_button("+", "zoom-in")
                            .px_1()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.action(Action::Zoom(1), window, cx);
                            })),
                    ),
            )
    }

    fn session_tabs(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let mut cards = div()
            .id("session-tabs")
            .flex()
            .flex_1()
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&self.tabs_scroll)
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
                    .mt(px(4.))
                    .rounded_t(px(6.))
                    .flex()
                    .items_center()
                    .border_b_2()
                    .border_color(if selected { rgb(GREEN) } else { rgba(0) })
                    .bg(rgb(if selected { 0x2b343e } else { CHROME }))
                    .text_color(rgb(if selected { INK } else { MUTED }))
                    .cursor_pointer()
                    .hover(move |s| s.bg(rgb(if selected { 0x2b343e } else { HOVER })))
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
                            .child(Self::tag(
                                match session.kind {
                                    SessionKind::Local => ">_",
                                    SessionKind::Ssh(_) => "SSH",
                                },
                                session.accent(),
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .truncate()
                                    .child(session.title.clone()),
                            )
                            .child(
                                div()
                                    .id(("close-session", id))
                                    .text_color(rgb(MUTED))
                                    .px_2()
                                    .rounded(px(4.))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(HOVER)).text_color(rgb(INK)))
                                    .child("\u{00d7}")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.request_close(CloseTarget::Tab(id), window, cx);
                                    })),
                            ),
                    ),
            );
        }
        cards.child(div().min_w(px(32.)).flex_1().h_full().when(
            cfg!(target_os = "windows") && self.input.background_enabled(),
            |space| space.window_control_area(gpui::WindowControlArea::Drag),
        ))
    }

    fn field(
        &self,
        index: usize,
        label: &'static str,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.text_field(EditorTarget::Connection(index), label, placeholder, cx)
    }

    fn text_field(
        &self,
        target: EditorTarget,
        label: &'static str,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let index = target.slot();
        let view = cx.entity();
        let paint_view = view.clone();
        let input_view = view.clone();
        let focused = self.input.active_editor() == Some(target);
        div()
            .w_full()
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
                    .h(px(if target == EditorTarget::Launcher {
                        38.
                    } else {
                        32.
                    }))
                    .px_2()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(if focused { GREEN } else { BORDER }))
                    .bg(rgb(INPUT))
                    .overflow_hidden()
                    .cursor(gpui::CursorStyle::IBeam)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            match target {
                                EditorTarget::Connection(index)
                                    if this.input.connection_enabled() =>
                                {
                                    this.input.field = index;
                                    this.input.form_button = None;
                                }
                                EditorTarget::Launcher
                                    if this.input.active_editor() == Some(target) => {}
                                _ => return,
                            }
                            let offset = this.field_layouts[index]
                                .as_ref()
                                .map(|line| {
                                    line.closest_index_for_x(
                                        event.position.x - this.field_bounds[index].left()
                                            + this.field_offsets[index],
                                    )
                                })
                                .unwrap_or(0);
                            this.input
                                .editor_mut(target)
                                .move_cursor(offset, event.modifiers.shift);
                            window.focus(&this.focus);
                            cx.notify();
                        }),
                    )
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                view.update(cx, |this, _| {
                                    this.field_bounds[index] = bounds;
                                    let editor = this.input.editor(target);
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
                                                MUTED
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
                                    let editor = this.input.editor(target);
                                    let origin = bounds.origin
                                        + point(
                                            -this.field_offsets[index],
                                            (bounds.size.height - px(22.)) / 2.,
                                        );
                                    let selection = editor.selection();
                                    if focused && !selection.is_empty() {
                                        let start = line.x_for_index(selection.start);
                                        let end = line.x_for_index(selection.end);
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                origin + point(start, px(0.)),
                                                size(end - start, px(22.)),
                                            ),
                                            rgb(0x35564f),
                                        ));
                                    }
                                    if let Err(error) = line.paint(origin, px(22.), window, cx) {
                                        eprintln!("nosh-gpui: draw text field: {error:#}");
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
                                            rgb(GREEN),
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
                                                rgb(GREEN),
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

    fn connection_button(
        &self,
        label: &'static str,
        id: &'static str,
        button: ConnectionButton,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        Self::button(label, id, button == ConnectionButton::Connect)
            .when(self.input.form_button == Some(button), |control| {
                control.border_2().border_color(rgb(INK))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.input.cancel_composition();
                    this.input.form_button = Some(button);
                    window.focus(&this.focus);
                    cx.notify();
                }),
            )
            .on_click(
                cx.listener(move |this, _, window, cx| this.action(button.action(), window, cx)),
            )
    }

    fn launcher(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let items = self.launcher_items();
        let groups = items
            .iter()
            .enumerate()
            .filter(|(index, item)| *index == 0 || items[*index - 1].group != item.group)
            .count();
        let content_height = if items.is_empty() {
            px(252.)
        } else {
            px(172. + items.len() as f32 * 52. + groups as f32 * 28.)
        };
        let mut rows = div()
            .id("launcher-results")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.launcher_scroll)
            .pb_2();
        let mut group = "";
        for (index, item) in items.iter().enumerate() {
            let heading = group != item.group;
            group = item.group;
            let selected = self.launcher_selected == index;
            let action = item.action;
            rows = rows.child(
                div()
                    .when(heading, |row| {
                        row.child(
                            div()
                                .px_4()
                                .pt_3()
                                .pb_1()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(item.group.to_uppercase()),
                        )
                    })
                    .child(
                        div()
                            .id(("launcher-item", index))
                            .mx_2()
                            .px_2()
                            .h(px(52.))
                            .rounded(px(5.))
                            .border_l_2()
                            .border_color(if selected { rgb(GREEN) } else { rgba(0) })
                            .bg(if selected { rgb(HOVER) } else { rgba(0) })
                            .hover(|style| style.bg(rgb(HOVER)))
                            .flex()
                            .items_center()
                            .gap_3()
                            .cursor_pointer()
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                if this.launcher_selected != index {
                                    this.launcher_selected = index;
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.activate_launcher(action, window, cx);
                            }))
                            .child(div().w(px(40.)).flex_shrink_0().child(Self::tag(
                                item.glyph,
                                if item.glyph == "SSH" { BLUE } else { GREEN },
                            )))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_size(px(13.))
                                            .truncate()
                                            .child(item.title.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(MUTED))
                                            .truncate()
                                            .child(item.detail.clone()),
                                    ),
                            )
                            .when(!item.shortcut.is_empty(), |row| {
                                row.child(Self::tag(item.shortcut, MUTED))
                            }),
                    ),
            );
        }
        if items.is_empty() {
            rows = rows.child(
                div()
                    .px_4()
                    .py_5()
                    .text_color(rgb(MUTED))
                    .child("No matches. Try a session name, host, or action."),
            );
        }
        div()
            .absolute()
            .inset_0()
            .p_4()
            .pt(px(72.))
            .flex()
            .flex_col()
            .items_center()
            .bg(rgba(0x0b0f18bb))
            .occlude()
            .child(
                div()
                    .w_full()
                    .max_w(px(640.))
                    .h(content_height
                        .min(window.viewport_size().height - px(120.))
                        .min(px(540.)))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(SURFACE))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .p_4()
                            .flex_shrink_0()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div()
                                    .mb_3()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("New or switch")
                                    .child(Self::toolbar_button("Esc", "close-launcher").on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.action(Action::ReturnToTerminal, window, cx);
                                        }),
                                    )),
                            )
                            .child(self.text_field(
                                EditorTarget::Launcher,
                                "QUICK OPEN",
                                "Search sessions, saved hosts, or actions...",
                                cx,
                            ))
                            .when(!self.error.is_empty(), |header| {
                                header.child(
                                    div()
                                        .mt_2()
                                        .text_size(px(12.))
                                        .text_color(rgb(RED))
                                        .child(self.error.clone()),
                                )
                            }),
                    )
                    .child(rows)
                    .child(
                        div()
                            .h(px(32.))
                            .flex_shrink_0()
                            .px_4()
                            .flex()
                            .items_center()
                            .justify_between()
                            .border_t_1()
                            .border_color(rgb(BORDER))
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child("Up / Down: select     Enter: open     Esc: dismiss")
                            .child(format!(
                                "{} result{}",
                                items.len(),
                                if items.len() == 1 { "" } else { "s" }
                            )),
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
            .bg(rgba(0x0b0f18bb))
            .occlude()
            .child(
                div()
                    .w(px(680.))
                    .h_full()
                    .max_h(px(430.))
                    .rounded(px(8.))
                    .bg(rgb(SURFACE))
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
                                    .child(if self.editing_connection.is_some() {
                                        "Review saved connection"
                                    } else {
                                        "New SSH connection"
                                    }),
                            )
                            .child(Self::tag("OpenSSH", BLUE)),
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
                            .child(
                                div()
                                    .flex()
                                    .gap_5()
                                    .child(
                                        div().flex_1().min_w_0().flex().flex_col().gap_3()
                                            .child(div().font_weight(gpui::FontWeight::SEMIBOLD)
                                                .child("Destination"))
                                            .child(self.field(1, "Host or SSH alias", "Required", cx))
                                            .child(div().flex().gap_3()
                                                .child(self.field(2, "User", "SSH config", cx))
                                                .child(self.field(3, "Port", "SSH config", cx))),
                                    )
                                    .child(
                                        div().flex_1().min_w_0().flex().flex_col().gap_3()
                                            .child(div().font_weight(gpui::FontWeight::SEMIBOLD)
                                                .child("Profile & access"))
                                            .child(self.field(0, "Display name", "Defaults to host", cx))
                                            .child(self.field(4, "Identity file", "Optional private key path", cx))
                                            .child(self.field(5, "Jump host", "Optional user@jump:port", cx)),
                                    ),
                            )
                            .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(
                                "Opens a separate SSH terminal. Host verification stays on; passwords are entered there.",
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
                                    .child(self.connection_button(
                                        "Cancel",
                                        "cancel-connection",
                                        ConnectionButton::Cancel,
                                        cx,
                                    ))
                                    .child(self.connection_button(
                                        "Save & connect",
                                        "save-connect",
                                        ConnectionButton::SaveAndConnect,
                                        cx,
                                    ))
                                    .child(self.connection_button(
                                        "Connect",
                                        "connect",
                                        ConnectionButton::Connect,
                                        cx,
                                    )),
                            )
                            .child(
                                div().text_size(px(10.)).text_color(rgb(MUTED)).child(
                                    if self.input.form_button.is_some() {
                                        "Enter / Space: selected button   Ctrl+Enter: save & connect   Esc: cancel"
                                    } else {
                                        "Enter: connect   Ctrl+Enter: save & connect   Esc: cancel"
                                    },
                                ),
                            ),
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
            .bg(rgba(0x0b0f18bb))
            .occlude()
            .child(
                div()
                    .w(px(440.))
                    .p_5()
                    .rounded(px(8.))
                    .bg(rgb(SURFACE))
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
                    .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(
                        "Enter: activate selected button. Esc: keep session. Tab / arrows: select.",
                    ))
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
                                Self::styled_button(
                                    confirm,
                                    "confirm-close",
                                    if prompt.confirm_selected {
                                        ButtonStyle::Danger
                                    } else {
                                        ButtonStyle::Standard
                                    },
                                )
                                .text_color(rgb(if prompt.confirm_selected {
                                    0xffffff
                                } else {
                                    RED
                                }))
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.confirm_close(window, cx)
                                    }),
                                ),
                            ),
                    ),
            )
    }
}

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_launcher();
        if self.input.surface == Surface::Launcher && std::mem::take(&mut self.reveal_launcher) {
            let view = cx.entity();
            window.on_next_frame(move |_, cx| {
                view.update(cx, |this, cx| {
                    this.launcher_scroll.scroll_to_item(this.launcher_selected);
                    cx.notify();
                });
            });
        }
        let width = window.viewport_size().width;
        let overflow = self.tabs_scroll.max_offset().width > px(0.);
        if self.viewport_width != width || self.tabs_overflow != overflow {
            self.viewport_width = width;
            self.tabs_overflow = overflow;
            self.reveal_active_tab = true;
        }
        if std::mem::take(&mut self.reveal_active_tab) {
            let view = cx.entity();
            // Scroll after layout, when new or resized tab bounds are available.
            window.on_next_frame(move |_, cx| {
                view.update(cx, |this, cx| {
                    this.tabs_scroll.scroll_to_item(this.active);
                    cx.notify();
                });
            });
        }
        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(SURFACE))
            .text_color(rgb(INK))
            .font_family("Segoe UI")
            .text_size(px(13.))
            .on_key_down(cx.listener(Self::key))
            .child(self.header(window, cx));
        if !self.error.is_empty() && self.input.surface != Surface::Connection {
            root =
                root.child(
                    div()
                        .px_3()
                        .py_1()
                        .flex()
                        .gap_3()
                        .items_center()
                        .bg(rgb(ERROR_SURFACE))
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
                // Root focus handling would prevent Windows' default caption actions.
                .track_focus(&self.focus)
                .when(self.connections_visible(window), |body| {
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
        if self.input.surface == Surface::Launcher {
            root = root.child(self.launcher(window, cx));
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
        let editor = self.input.editor(self.input.active_editor()?);
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
        if let Some(target) = self.input.active_editor() {
            let editor = self.input.editor(target);
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
        if let Some(target) = self.input.active_editor() {
            let editor = self.input.editor(target);
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
        if let Some(target) = self.input.active_editor() {
            self.input.editor_mut(target).marked = None;
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
        if let Some(target) = self.input.active_editor()
            && let Some(selected) = selected
        {
            let editor = self.input.editor_mut(target);
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
        if let Some(target) = self.input.active_editor() {
            let index = target.slot();
            let line = self.field_layouts[index].as_ref()?;
            let range = self.input.editor(target).byte_range(range);
            let origin = self.field_bounds[index].origin
                + point(
                    line.x_for_index(range.start) - self.field_offsets[index],
                    (self.field_bounds[index].size.height - px(22.)) / 2.,
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
        let target = self.input.active_editor()?;
        let index = target.slot();
        let byte = self.field_layouts[index].as_ref()?.closest_index_for_x(
            pos.x - self.field_bounds[index].left() + self.field_offsets[index],
        );
        Some(
            self.input.editor(target).text[..byte.min(self.input.editor(target).text.len())]
                .encode_utf16()
                .count(),
        )
    }
}
