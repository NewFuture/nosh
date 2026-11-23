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
use nosh_desktop::local_shell::LocalShell;
use nosh_desktop::terminal::{BackendSettings, PtyEvent, TerminalBackend};

use crate::gpui_input::{
    Action, CloseTarget, ConnectionButton, EditorTarget, InputState, PasteReview, Surface,
};
use crate::gpui_launcher::{LaunchAction, LaunchItem};
use crate::gpui_search::OutputSearch;
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
    cwd: Option<PathBuf>,
    #[arg(long, help = "Use an isolated desktop settings file")]
    settings: Option<PathBuf>,
}

enum SessionKind {
    Local,
    Ssh(Connection),
}

enum ProcessState {
    Running,
    Exited(i32),
    Closed,
}

impl ProcessState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    fn is_failure(&self) -> bool {
        matches!(self, Self::Exited(code) if *code != 0) || matches!(self, Self::Closed)
    }
}

struct Session {
    id: u64,
    title: String,
    kind: SessionKind,
    status: ProcessState,
    backend: TerminalBackend,
    error: String,
    search: OutputSearch,
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
            SessionKind::Ssh(connection) => connection.target(),
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
    nosh: Result<LocalShell, String>,
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
    invalid_field: Option<usize>,
    notice: String,
    input: InputState,
    bounds: Bounds<Pixels>,
    cell: gpui::Size<Pixels>,
    selecting: bool,
    field_bounds: [Bounds<Pixels>; 9],
    field_layouts: [Option<ShapedLine>; 9],
    field_offsets: [Pixels; 9],
    editing_connection: Option<usize>,
    connection_revision: u64,
    rename_target: Option<u64>,
    rename_error: String,
    menu_selected: usize,
    actions_bounds: Bounds<Pixels>,
    menu_target: Option<u64>,
    menu_position: Option<Point<Pixels>>,
    menu_is_tab: bool,
    library_selected: usize,
    library_scroll: ScrollHandle,
    reveal_library: bool,
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
            nosh: LocalShell::resolve(),
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
            invalid_field: None,
            notice: String::new(),
            input: InputState::default(),
            bounds: Bounds::default(),
            cell: size(px(9.), px(20.)),
            selecting: false,
            field_bounds: Default::default(),
            field_layouts: Default::default(),
            field_offsets: [px(0.); 9],
            editing_connection: None,
            connection_revision: 0,
            rename_target: None,
            rename_error: String::new(),
            menu_selected: 0,
            actions_bounds: Bounds::default(),
            menu_target: None,
            menu_position: None,
            menu_is_tab: false,
            library_selected: 0,
            library_scroll: ScrollHandle::new(),
            reveal_library: false,
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
                this.request_close(CloseTarget::Window, window, cx);
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
        if self.input.pasting.as_ref().is_some_and(|paste| {
            !self
                .sessions
                .iter()
                .any(|s| s.id == paste.target && s.status.is_running())
        }) {
            self.input.pasting = None;
            self.notice = "Paste cancelled: its target session ended. Nothing was sent.".into();
        }
    }

    fn open(
        &mut self,
        title: String,
        kind: SessionKind,
        settings: BackendSettings,
        cx: &mut Context<Self>,
    ) {
        if self.input.closing.is_some() || self.input.pasting.is_some() {
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
                self.notice.clear();
                self.sessions.push(Session {
                    id: self.next_id,
                    title,
                    kind,
                    status: ProcessState::Running,
                    backend,
                    error: String::new(),
                    search: OutputSearch::default(),
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
            (Ok(nosh), Ok(cwd)) => match nosh.backend(cwd) {
                Ok(backend) => self.open(
                    format!("Local {}", self.next_id),
                    SessionKind::Local,
                    backend,
                    cx,
                ),
                Err(error) => {
                    self.report_error(error);
                    cx.notify();
                }
            },
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
                if save && let Err(error) = self.save_connection(&connection) {
                    self.report_error(error);
                    cx.notify();
                    return;
                }
                self.open(
                    connection.name.clone(),
                    SessionKind::Ssh(connection),
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

    fn persist_settings(&mut self, settings: Settings) -> Result<(), String> {
        let path = self.settings_path.as_ref().ok_or(
            "Saving is disabled because settings could not be loaded. Connect without saving is still available.",
        )?;
        settings.save(path)?;
        self.settings = settings;
        Ok(())
    }

    fn save_connection(&mut self, connection: &Connection) -> Result<(), String> {
        connection.validate()?;
        let mut settings = self.settings.clone();
        if let Some(index) = self.editing_connection {
            settings.connections[index] = connection.clone();
        } else {
            settings.connections.push(connection.clone());
        }
        self.persist_settings(settings)
    }

    fn form_connection(&mut self) -> Result<Connection, String> {
        self.form_error.clear();
        self.invalid_field = None;
        let fields = &self.input.fields;
        let port = if fields[3].text.is_empty() {
            None
        } else {
            match fields[3].text.parse::<u16>() {
                Ok(port) if port != 0 => Some(port),
                _ => {
                    self.invalid_field = Some(3);
                    self.input.field = 3;
                    self.input.form_button = None;
                    return Err("Port must be between 1 and 65535.".into());
                }
            }
        };
        let host = fields[1].text.trim().to_owned();
        if host.is_empty() {
            self.invalid_field = Some(1);
            self.input.field = 1;
            self.input.form_button = None;
            return Err("Enter a host or an SSH config alias.".into());
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
        connection.validate()?;
        Ok(connection)
    }

    fn submit_connection(&mut self, connect: bool, save: bool, cx: &mut Context<Self>) {
        if !self.input.connection_enabled() {
            return;
        }
        match self.form_connection() {
            Ok(connection) if connect => self.open_ssh(connection, save, cx),
            Ok(connection) => match self.save_connection(&connection) {
                Ok(()) => {
                    self.notice = format!("Saved {}. No connection was started.", connection.name);
                    self.input.set_surface(if self.sessions.is_empty() {
                        Surface::Board
                    } else {
                        Surface::Terminal
                    });
                }
                Err(error) => self.form_error = error,
            },
            Err(error) => self.form_error = error,
        }
        cx.notify();
    }

    fn switch_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.closing.is_some()
            || self.input.pasting.is_some()
            || index >= self.sessions.len()
        {
            return;
        }
        self.active = index;
        self.notice.clear();
        self.reveal_active_tab = true;
        self.input.set_surface(Surface::Terminal);
        if self.sessions[index].search.open {
            self.focus_find();
        }
        self.selecting = false;
        window.focus(&self.focus);
        cx.notify();
    }

    fn show_surface(&mut self, surface: Surface, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.closing.is_some() || self.input.pasting.is_some() {
            return;
        }
        self.input.set_surface(surface);
        self.notice.clear();
        self.selecting = false;
        if surface == Surface::Connection {
            self.editing_connection = None;
            self.input.fields = Default::default();
            self.input.field = 1;
            self.form_error.clear();
            self.invalid_field = None;
            self.input.advanced = false;
            self.input.editing = false;
            self.connection_revision += 1;
        } else if surface == Surface::Launcher {
            self.launcher_query.clear();
            self.launcher_selected = 0;
            self.reveal_launcher = true;
        } else if surface == Surface::SessionMenu {
            self.menu_selected = 0;
            self.menu_target = self.sessions.get(self.active).map(|s| s.id);
            self.menu_position = None;
            self.menu_is_tab = false;
        }
        window.focus(&self.focus);
        cx.notify();
    }

    fn edit_connection(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input.background_enabled() {
            return;
        }
        let Some(connection) = self.settings.connections.get(index).cloned() else {
            return;
        };
        self.show_surface(Surface::Connection, window, cx);
        self.editing_connection = Some(index);
        self.input.editing = true;
        self.input.advanced = !connection.identity.is_empty() || !connection.jump.is_empty();
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

    fn connect_saved(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(connection) = self.settings.connections.get(index).cloned() {
            self.open_ssh(connection, false, cx);
        }
    }

    fn begin_rename(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let title = session.title.clone();
        self.input.set_surface(Surface::Rename);
        self.rename_target = Some(id);
        self.rename_error.clear();
        self.input.rename = Default::default();
        if let Err(error) = self.input.rename.replace(None, &title) {
            self.rename_error = error;
        }
        self.input.rename.anchor = 0;
        window.focus(&self.focus);
        cx.notify();
    }

    fn apply_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.input.rename.text.trim();
        if title.is_empty() || title.len() > 80 {
            self.rename_error = "Use a session name containing 1-80 UTF-8 bytes.".into();
            self.input.rename_focus = 0;
        } else {
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|s| Some(s.id) == self.rename_target)
            {
                session.title = title.to_owned();
            }
            self.action(Action::ReturnToTerminal, window, cx);
        }
        cx.notify();
    }

    fn browse_identity(&mut self, cx: &mut Context<Self>) {
        let revision = self.connection_revision;
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select SSH identity file".into()),
        });
        cx.spawn(async move |view, cx| {
            let result = prompt.await;
            let _ = view.update(cx, |this, cx| {
                if this.connection_revision != revision || !this.input.connection_enabled() {
                    return;
                }
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.first() {
                            let field = &mut this.input.fields[4];
                            field.anchor = 0;
                            field.cursor = field.text.len();
                            if let Err(error) = field.replace(None, &path.to_string_lossy()) {
                                this.form_error = error;
                            }
                            this.input.field = 4;
                            this.input.form_button = None;
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => this.form_error = format!("Select identity file: {error}"),
                    Err(error) => this.form_error = format!("File picker did not respond: {error}"),
                }
                cx.notify();
            });
        })
        .detach();
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
                "Find in terminal output",
                "Literal search in this session's screen and recent scrollback",
                "Terminal",
                "F",
                "Ctrl+Shift+F",
                LaunchAction::Find,
            ));
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
                "Rename current session",
                session.title.clone(),
                "Terminal",
                "Aa",
                "F2",
                LaunchAction::Rename(session.id),
            ));
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
            "Toggle connection library",
            "Show or hide saved SSH targets",
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
            LaunchAction::Saved(index) => self.connect_saved(index, cx),
            LaunchAction::Rename(id) => self.begin_rename(id, window, cx),
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
            LaunchAction::Find => self.action(Action::OpenFind, window, cx),
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
        let needs_confirmation = match target {
            CloseTarget::Tab(id) => self
                .sessions
                .iter()
                .any(|s| s.id == id && s.status.is_running()),
            CloseTarget::Window => self.sessions.iter().any(|s| s.status.is_running()),
            CloseTarget::Connection(_) => true,
        };
        if !needs_confirmation {
            self.finish_close(target, window, cx);
            return;
        }
        self.input.begin_close(target);
        self.selecting = false;
        window.focus(&self.focus);
        cx.notify();
    }

    fn confirm_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(prompt) = self.input.closing.take() {
            self.finish_close(prompt.target, window, cx);
        }
    }

    fn finish_close(&mut self, target: CloseTarget, window: &mut Window, cx: &mut Context<Self>) {
        match target {
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
                    } else {
                        self.input.set_surface(Surface::Terminal);
                        if self.sessions[self.active].search.open {
                            self.focus_find();
                        }
                    }
                }
                window.focus(&self.focus);
            }
            CloseTarget::Connection(index) => {
                if let Some(connection) = self.settings.connections.get(index) {
                    let name = connection.name.clone();
                    let mut settings = self.settings.clone();
                    settings.connections.remove(index);
                    match self.persist_settings(settings) {
                        Ok(()) => {
                            self.library_selected = self
                                .library_selected
                                .min(self.settings.connections.len().saturating_sub(1));
                            self.notice = format!(
                                "Removed {name} from the library. Open sessions were not changed."
                            );
                            self.action(Action::ReturnToTerminal, window, cx);
                        }
                        Err(error) => self.report_error(error),
                    }
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

    fn focus_find(&mut self) {
        let Some(session) = self.sessions.get_mut(self.active) else {
            return;
        };
        session.search.open = true;
        let query = session.search.query.clone();
        self.input.set_surface(Surface::Find);
        self.input.find = Default::default();
        if let Err(error) = self.input.find.replace(None, &query) {
            session.search.error = error;
        }
        self.input.find.anchor = 0;
    }

    fn sync_find(&mut self) {
        if self.input.surface == Surface::Find
            && self.input.find.marked.is_none()
            && let Some(session) = self.sessions.get_mut(self.active)
        {
            session.search.set_query(&self.input.find.text);
        }
    }

    fn review_paste(&mut self, text: String) {
        if !self.input.terminal_enabled() {
            return;
        }
        let Some(session) = self.sessions.get_mut(self.active) else {
            return;
        };
        if !session.status.is_running() {
            session.error = "This session has ended. Nothing was pasted.".into();
            return;
        }
        match PasteReview::new(session.id, text) {
            Err(error) => session.error = error,
            Ok(review) if review.needs_confirmation() => {
                self.input.cancel_composition();
                self.input.pasting = Some(review);
                self.selecting = false;
            }
            Ok(review) => self.send_paste(review),
        }
    }

    fn send_paste(&mut self, review: PasteReview) {
        if !self.input.terminal_enabled() {
            return;
        }
        let Some(session) = self.sessions.get(self.active) else {
            return;
        };
        if !review.allows_target(session.id, session.status.is_running()) {
            self.notice =
                "Paste cancelled: its target is no longer active and running. Nothing was sent."
                    .into();
            return;
        }
        let mode = *session.backend.model().lock().mode();
        if let Some(bytes) = nosh_desktop::terminal::keyboard_input(
            &eframe::egui::Event::Paste(review.text),
            mode,
            eframe::egui::Modifiers::NONE,
        ) {
            self.write(bytes);
        }
    }

    fn open_context_menu(
        &mut self,
        id: u64,
        position: Point<Pixels>,
        tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input.background_enabled() {
            return;
        }
        self.show_surface(Surface::SessionMenu, window, cx);
        self.menu_target = Some(id);
        self.menu_position = Some(position);
        self.menu_is_tab = tab;
    }

    fn report_error(&mut self, error: String) {
        if self.input.surface == Surface::Connection {
            self.form_error = error;
        } else if self.input.surface == Surface::Rename {
            self.rename_error = error;
        } else {
            self.error = error;
        }
    }

    fn write(&mut self, bytes: Vec<u8>) {
        // All keyboard, clipboard and native text paths share this final PTY gate.
        if !self.input.terminal_enabled() {
            return;
        }
        if let Some(session) = self.sessions.get_mut(self.active) {
            if !matches!(session.status, ProcessState::Running) {
                session.error = "This session has ended. Open a new session to continue.".into();
            } else if let Err(error) = session.backend.write(bytes) {
                session.error = format!("Terminal input: {error}");
            } else {
                session.error.clear();
                session
                    .backend
                    .model()
                    .lock()
                    .scroll_display(Scroll::Bottom);
            }
        }
    }

    fn session_actions(&self) -> Vec<(&'static str, &'static str, Option<Action>)> {
        let Some(session) = self
            .sessions
            .iter()
            .find(|s| Some(s.id) == self.menu_target)
        else {
            return Vec::new();
        };
        if self.menu_is_tab {
            return vec![
                (
                    "Activate session",
                    "",
                    Some(Action::ActivateTab(session.id)),
                ),
                ("Rename session", "F2", Some(Action::RenameTab(session.id))),
                (
                    "Close session",
                    "Ctrl+Shift+W",
                    Some(Action::CloseTab(session.id)),
                ),
            ];
        }
        let has_selection = session
            .backend
            .model()
            .lock()
            .selection_to_string()
            .is_some();
        vec![
            ("Find in terminal", "Ctrl+Shift+F", Some(Action::OpenFind)),
            ("Session details", "", Some(Action::SessionDetails)),
            ("Rename session", "F2", Some(Action::RenameTab(session.id))),
            (
                "Copy selection",
                "Ctrl+Shift+C",
                has_selection.then_some(Action::CopyTerminal),
            ),
            (
                "Paste",
                "Ctrl+Shift+V",
                session.status.is_running().then_some(Action::Paste),
            ),
            (
                "Close session",
                "Ctrl+Shift+W",
                Some(Action::CloseTab(session.id)),
            ),
        ]
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
            Action::PasteText(text) => self.review_paste(text),
            Action::ConfirmPaste => {
                if let Some(review) = self.input.pasting.take() {
                    self.send_paste(review);
                    window.focus(&self.focus);
                }
            }
            Action::CancelPaste => {
                self.input.pasting = None;
                window.focus(&self.focus);
            }
            Action::OpenFind => {
                self.focus_find();
                window.focus(&self.focus);
            }
            Action::CloseFind => {
                if let Some(session) = self.sessions.get_mut(self.active) {
                    session.search.open = false;
                }
                self.input.set_surface(Surface::Terminal);
                window.focus(&self.focus);
            }
            Action::MoveFind(reverse) => {
                self.sync_find();
                if let Some(session) = self.sessions.get_mut(self.active) {
                    session.search.navigate(
                        &mut session.backend.model().lock(),
                        session.backend.content_revision(),
                        reverse,
                    );
                }
            }
            Action::SessionDetails => self.show_surface(Surface::Details, window, cx),
            Action::RenameTab(id) => self.begin_rename(id, window, cx),
            Action::CloseTab(id) => self.request_close(CloseTarget::Tab(id), window, cx),
            Action::ActivateTab(id) => {
                if let Some(index) = self.sessions.iter().position(|s| s.id == id) {
                    self.switch_session(index, window, cx);
                }
            }
            Action::NewLocal => self.open_local(cx),
            Action::NewConnection => self.show_surface(Surface::Connection, window, cx),
            Action::OpenSessionMenu => self.show_surface(Surface::SessionMenu, window, cx),
            Action::MoveMenu(reverse) => {
                let items = self.session_actions();
                if !items.is_empty() {
                    for step in 1..=items.len() {
                        let index = (self.menu_selected
                            + if reverse { items.len() - step } else { step })
                            % items.len();
                        if items[index].2.is_some() {
                            self.menu_selected = index;
                            break;
                        }
                    }
                }
            }
            Action::ActivateMenu => {
                if let Some((_, _, Some(action))) =
                    self.session_actions().into_iter().nth(self.menu_selected)
                {
                    self.input.set_surface(Surface::Terminal);
                    self.action(action, window, cx);
                }
            }
            Action::RenameSession => {
                if let Some(session) = self.sessions.get(self.active) {
                    self.begin_rename(session.id, window, cx);
                }
            }
            Action::ApplyRename => self.apply_rename(window, cx),
            Action::ToggleLibraryFocus => {
                if self.input.surface == Surface::Library {
                    self.action(Action::ReturnToTerminal, window, cx);
                } else {
                    self.connections_visible = Some(true);
                    self.input.set_surface(Surface::Library);
                    self.reveal_library = true;
                }
            }
            Action::MoveLibrary(reverse) => {
                let count = self.settings.connections.len();
                if count != 0 {
                    self.library_selected =
                        (self.library_selected + if reverse { count - 1 } else { 1 }) % count;
                    self.reveal_library = true;
                }
            }
            Action::ConnectLibrary => self.connect_saved(self.library_selected, cx),
            Action::EditLibrary => self.edit_connection(self.library_selected, window, cx),
            Action::ToggleAdvanced => self.input.advanced = !self.input.advanced,
            Action::BrowseIdentity => self.browse_identity(cx),
            Action::DeleteConnection => {
                let index = if self.input.surface == Surface::Connection {
                    self.editing_connection
                } else {
                    (!self.settings.connections.is_empty()).then_some(self.library_selected)
                };
                if let Some(index) = index {
                    self.request_close(CloseTarget::Connection(index), window, cx);
                }
            }
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
                if self.input.surface == Surface::Library {
                    self.input.set_surface(if self.sessions.is_empty() {
                        Surface::Board
                    } else {
                        Surface::Terminal
                    });
                }
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
            Action::Connect => self.submit_connection(true, false, cx),
            Action::SaveAndConnect => self.submit_connection(true, true, cx),
            Action::SaveConnection => self.submit_connection(false, true, cx),
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
        self.sync_find();
        cx.notify();
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let action = self.input.key(event, self.mode());
        let handled = self.input.closing.is_some()
            || self.input.pasting.is_some()
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
        if self.input.surface == Surface::Find && self.input.background_enabled() {
            self.input.set_surface(Surface::Terminal);
        }
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
            session.error = format!("Resize terminal: {error}");
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
            .h(px(30.))
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
            .text_size(px(11.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(rgb(color))
            .child(label.into())
    }

    fn toolbar_button(
        label: impl Into<SharedString>,
        id: impl Into<gpui::ElementId>,
    ) -> gpui::Stateful<gpui::Div> {
        Self::button(label, id, false)
            .h(px(28.))
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
        self.connections_visible.unwrap_or(
            window.viewport_size().width >= px(1000.) && !self.settings.connections.is_empty(),
        )
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
                Self::button("+ Local", "new-session", false)
                    .text_color(rgb(GREEN))
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.action(Action::NewLocal, window, cx)
                        }),
                    ),
            )
            .child(
                Self::toolbar_button("SSH...", "new-ssh")
                    .text_color(rgb(BLUE))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_surface(Surface::Connection, window, cx);
                    })),
            )
            .child(div().flex_1().min_w(px(32.)).h_full().when(
                cfg!(target_os = "windows") && self.input.background_enabled(),
                |space| space.window_control_area(gpui::WindowControlArea::Drag),
            ))
            .child(
                Self::toolbar_button("Quick open", "command-search")
                    .flex_shrink_0()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_surface(Surface::Launcher, window, cx);
                    })),
            )
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
        let mut saved = div()
            .id("saved-targets")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.library_scroll)
            .flex()
            .flex_col()
            .gap_1();
        if self.settings.connections.is_empty() {
            saved = saved.child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child("Save an SSH target to reconnect here in one click. Use Edit to change its settings."),
            );
        }
        for (index, connection) in self.settings.connections.iter().enumerate() {
            let selected = self.input.surface == Surface::Library && self.library_selected == index;
            saved = saved.child(
                div()
                    .id(("saved", index))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .mx_2()
                    .px_2()
                    .py_2()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if selected { rgb(GREEN) } else { rgba(0) })
                    .bg(if selected { rgb(RAISED) } else { rgba(0) })
                    .hover(|style| style.bg(rgb(HOVER)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.connect_saved(index, cx);
                        window.focus(&this.focus);
                    }))
                    .child(Self::tag("SSH", BLUE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(connection.name.clone()))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(MUTED))
                                    .truncate()
                                    .child(connection.target()),
                            ),
                    )
                    .child(
                        Self::toolbar_button("Edit", ("edit-saved", index))
                            .px_1()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.edit_connection(index, window, cx);
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
                    .child("Connections")
                    .child(Self::tag(format!("{}", self.settings.connections.len()), MUTED)),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .child(
                        Self::button("+ Add SSH target", "add-saved", false)
                            .w_full()
                            .text_color(rgb(BLUE))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_surface(Surface::Connection, window, cx);
                            })),
                    ),
            )
            .child(saved)
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
                    .child(Self::shortcut("Focus connections", "F6"))
                    .when(self.input.surface == Surface::Library, |footer| {
                        footer.child(
                            div().text_size(px(12.)).text_color(rgb(GREEN))
                                .child("Up / Down: select\nEnter: connect  F2: edit\nDelete: remove  Esc: terminal"),
                        )
                    })
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
                    Self::button("New local terminal", "empty-local", true).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.action(Action::NewLocal, window, cx);
                        },
                    )),
                );
        };
        let identity = session.identity();
        let status = session.status_label();
        let status_color = session.status_color();
        let is_exited = !matches!(session.status, ProcessState::Running);
        let terminal = div()
            .id("terminal")
            .relative()
            .flex_1()
            .min_h_0()
            .p_2()
            .overflow_hidden()
            .bg(rgb(0x1c1e26))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if let Some(session) = this.sessions.get(this.active) {
                        this.open_context_menu(session.id, event.position, false, window, cx);
                        cx.stop_propagation();
                    }
                }),
            )
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
                            if let Some(session) = this.sessions.get_mut(this.active) {
                                session.search.editing = this.input.surface == Surface::Find
                                    && this.input.find_focus == 0;
                                match gpui_terminal::paint(
                                    &session.backend,
                                    bounds,
                                    this.cell,
                                    this.settings.font_size,
                                    &mut session.search,
                                    window,
                                    cx,
                                ) {
                                    Ok(true) => window.request_animation_frame(),
                                    Ok(false) => {}
                                    Err(error) => {
                                        session.error = format!("Draw terminal: {error:#}");
                                        cx.notify();
                                    }
                                }
                            }
                        });
                    },
                )
                .size_full(),
            )
            .when(session.search.open, |terminal| {
                terminal.child(self.find_bar(cx))
            });
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
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(identity),
                            ),
                    )
                    .child(Self::toolbar_button("Details", "session-details").on_click(
                        cx.listener(|this, _, window, cx| {
                            this.action(Action::SessionDetails, window, cx)
                        }),
                    ))
                    .child(
                        Self::toolbar_button("Find output", "terminal-find").on_click(cx.listener(
                            |this, _, window, cx| this.action(Action::OpenFind, window, cx),
                        )),
                    )
                    .child(
                        Self::toolbar_button("Actions...", "session-actions")
                            .relative()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_surface(Surface::SessionMenu, window, cx);
                            }))
                            .child(
                                canvas(
                                    {
                                        let view = cx.entity();
                                        move |bounds, _, cx| {
                                            view.update(cx, |this, _| this.actions_bounds = bounds)
                                        }
                                    },
                                    |_, (), _, _| {},
                                )
                                .absolute()
                                .size_full(),
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
                        .bg(rgb(if session.status.is_failure() {
                            ERROR_SURFACE
                        } else {
                            RAISED
                        }))
                        .text_color(rgb(if session.status.is_failure() {
                            RED
                        } else {
                            MUTED
                        }))
                        .text_size(px(12.))
                        .child(format!("{status}. Output preserved."))
                        .child(Self::toolbar_button("New local", "exited-new").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.action(Action::NewLocal, window, cx);
                            }),
                        )),
                )
            })
            .when(!session.error.is_empty(), |stage| {
                stage.child(
                    div()
                        .px_3()
                        .py_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .bg(rgb(ERROR_SURFACE))
                        .text_color(rgb(RED))
                        .child(
                            div()
                                .flex_1()
                                .child(format!("{}: {}", session.title, session.error)),
                        )
                        .child(
                            Self::toolbar_button("Dismiss", "dismiss-session-error").on_click(
                                cx.listener(|this, _, _, cx| {
                                    if let Some(session) = this.sessions.get_mut(this.active) {
                                        session.error.clear();
                                    }
                                    cx.notify();
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
            .w(px(self.sessions.len() as f32 * 180.))
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&self.tabs_scroll)
            .h_full();
        for (index, session) in self.sessions.iter().enumerate() {
            let id = session.id;
            let selected = self.active == index;
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
                        this.action(Action::ActivateTab(id), window, cx)
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.open_context_menu(id, event.position, true, window, cx);
                            cx.stop_propagation();
                        }),
                    )
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
                                    .size(px(28.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
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
        cards
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
        let invalid =
            matches!(target, EditorTarget::Connection(field) if self.invalid_field == Some(field));
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .when(target != EditorTarget::Find, |field| {
                field.child(
                    div()
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(MUTED))
                        .child(label),
                )
            })
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
                    .border_color(rgb(if invalid {
                        RED
                    } else if focused {
                        GREEN
                    } else {
                        BORDER
                    }))
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
                                EditorTarget::Rename
                                    if this.input.surface == Surface::Rename
                                        && this.input.closing.is_none() =>
                                {
                                    this.input.rename_focus = 0;
                                }
                                EditorTarget::Find
                                    if this
                                        .sessions
                                        .get(this.active)
                                        .is_some_and(|s| s.search.open)
                                        && this.input.closing.is_none()
                                        && this.input.pasting.is_none() =>
                                {
                                    this.input.set_surface(Surface::Find);
                                    this.input.find_focus = 0;
                                }
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
            .when(invalid && !self.form_error.is_empty(), |field| {
                field.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(RED))
                        .child(self.form_error.clone()),
                )
            })
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
                                    .child("Quick open")
                                    .child(Self::toolbar_button("Esc", "close-launcher").on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.action(Action::ReturnToTerminal, window, cx);
                                        }),
                                    )),
                            )
                            .child(self.text_field(
                                EditorTarget::Launcher,
                                "SEARCH",
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
                            .text_size(px(12.))
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

    fn find_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let search = &self.sessions[self.active].search;
        div()
            .absolute()
            .when(search.bar_at_bottom, |bar| bar.bottom_0())
            .when(!search.bar_at_bottom, |bar| bar.top_0())
            .right_0()
            .w_full()
            .max_w(px(520.))
            .p_2()
            .rounded_bl(px(6.))
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(SURFACE))
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(self.text_field(
                        EditorTarget::Find,
                        "",
                        "Find output / screen + history",
                        cx,
                    )))
                    .children(
                        [
                            ("Prev", "find-prev", Action::MoveFind(true), 1),
                            ("Next", "find-next", Action::MoveFind(false), 2),
                            ("Esc", "find-close", Action::CloseFind, 3),
                        ]
                        .into_iter()
                        .map(|(label, id, action, focus)| {
                            Self::toolbar_button(label, id)
                                .border_color(rgb(
                                    if self.input.surface == Surface::Find
                                        && self.input.find_focus == focus
                                    {
                                        GREEN
                                    } else {
                                        BORDER
                                    },
                                ))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.action(action.clone(), window, cx);
                                }))
                        }),
                    ),
            )
            .child(
                div()
                    .mt_1()
                    .flex()
                    .gap_2()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(rgb(if search.error.is_empty() { MUTED } else { RED }))
                            .child(search.label()),
                    )
                    .child(if search.limited {
                        format!("Recent {} rows / bounded", search.rows)
                    } else {
                        "Screen + history / literal".into()
                    }),
            )
    }

    fn paste_dialog(&self, cx: &mut Context<Self>) -> gpui::Div {
        let review = self
            .input
            .pasting
            .as_ref()
            .expect("paste dialog requires a pending paste");
        let target = self.sessions.iter().find(|s| s.id == review.target);
        let identity = target.map_or_else(
            || "Session no longer available".into(),
            |s| format!("{} / {} / {}", s.title, s.kind_label(), s.identity()),
        );
        div().absolute().inset_0().p_4().flex().items_center().justify_center()
            .bg(rgba(0x0b0f18bb)).occlude()
            .child(div().w_full().max_w(px(640.)).p_4().rounded(px(8.))
                .border_1().border_color(rgb(BORDER)).bg(rgb(SURFACE)).shadow_lg()
                .flex().flex_col().gap_3()
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Review terminal paste"))
                .child(div().h(px(22.)).flex_shrink_0().text_color(rgb(BLUE)).truncate().child(identity))
                .child(div().flex_shrink_0().text_size(px(12.)).text_color(rgb(MUTED))
                    .child(format!("{} lines / {} UTF-8 bytes / processed only on this computer", review.lines, review.text.len())))
                .child(div().id("paste-preview").h(px(140.)).flex_shrink_0().overflow_y_scroll()
                    .p_3().rounded(px(4.)).bg(rgb(INPUT)).font_family("Consolas")
                    .text_size(px(12.)).child(review.preview()))
                .child(div().flex_shrink_0().text_size(px(12.)).text_color(rgb(MUTED))
                    .child("Newlines may execute commands in the receiving program, even with bracketed paste. No extra Enter is added."))
                .child(div().flex_shrink_0().text_size(px(12.)).text_color(rgb(MUTED))
                    .child("Enter: selected button / Escape: cancel"))
                .child(div().flex_shrink_0().flex().justify_end().gap_2()
                    .child(Self::button("Cancel", "paste-cancel", false)
                        .border_color(rgb(if !review.confirm_selected { GREEN } else { BORDER }))
                        .on_click(cx.listener(|this, _, window, cx| this.action(Action::CancelPaste, window, cx))))
                    .child(Self::styled_button("Paste to this session", "paste-confirm",
                        if review.confirm_selected { ButtonStyle::Danger } else { ButtonStyle::Standard })
                        .on_click(cx.listener(|this, _, window, cx| this.action(Action::ConfirmPaste, window, cx))))))
    }

    fn details_dialog(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(session) = self.sessions.get(self.active) else {
            return div();
        };
        let mut details = vec![
            ("Session", session.title.clone()),
            (
                "Target",
                format!("{} / {}", session.kind_label(), session.identity()),
            ),
            ("Process", session.status_label()),
        ];
        match &session.kind {
            SessionKind::Local => details.push((
                "Startup directory (not live cwd)",
                self.cwd
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            )),
            SessionKind::Ssh(connection) => {
                details.push((
                    "Identity file",
                    if connection.identity.is_empty() {
                        "OpenSSH configuration".into()
                    } else {
                        connection.identity.clone()
                    },
                ));
                details.push((
                    "Jump host",
                    if connection.jump.is_empty() {
                        "OpenSSH configuration".into()
                    } else {
                        connection.jump.clone()
                    },
                ));
                details.push((
                    "Remote directory",
                    "Unknown; raw terminal output is not treated as metadata.".into(),
                ));
            }
        }
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
                    .w_full()
                    .max_w(px(600.))
                    .p_4()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(SURFACE))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Session details"),
                    )
                    .children(details.into_iter().map(|(label, value)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child(label))
                            .child(div().child(value))
                    }))
                    .child(div().flex().justify_end().child(
                        Self::button("Done / Esc", "details-close", true).on_click(cx.listener(
                            |this, _, window, cx| this.action(Action::ReturnToTerminal, window, cx),
                        )),
                    )),
            )
    }

    fn session_menu(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let items = self.session_actions();
        let position = self.menu_position.unwrap_or(point(
            self.actions_bounds.right() - px(300.),
            self.actions_bounds.bottom() + px(2.),
        ));
        let position = point(
            position
                .x
                .min(window.viewport_size().width - px(300.))
                .max(px(0.)),
            position
                .y
                .min(window.viewport_size().height - px(40. + 36. * items.len() as f32))
                .max(px(0.)),
        );
        let mut menu = div()
            .absolute()
            .top(position.y)
            .left(position.x)
            .w(px(300.))
            .p_1()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(SURFACE))
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(30.))
                    .px_3()
                    .flex()
                    .items_center()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .truncate()
                    .child(
                        self.sessions
                            .iter()
                            .find(|s| Some(s.id) == self.menu_target)
                            .map(|s| s.title.clone())
                            .unwrap_or_default(),
                    ),
            );
        for (index, (label, shortcut, action)) in items.into_iter().enumerate() {
            let enabled = action.is_some();
            menu = menu.child(
                div()
                    .id(("session-action", index))
                    .h(px(36.))
                    .px_3()
                    .rounded(px(5.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_color(rgb(if enabled { INK } else { MUTED }))
                    .bg(if self.menu_selected == index {
                        rgb(HOVER)
                    } else {
                        rgba(0)
                    })
                    .when_some(action, |row, action| {
                        row.cursor_pointer()
                            .hover(|style| style.bg(rgb(HOVER)))
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                this.menu_selected = index;
                                cx.notify();
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.input.set_surface(Surface::Terminal);
                                this.action(action.clone(), window, cx);
                            }))
                    })
                    .child(label)
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child(shortcut),
                    ),
            );
        }
        div()
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.action(Action::ReturnToTerminal, window, cx);
                }),
            )
            .child(menu)
    }

    fn rename_dialog(&self, cx: &mut Context<Self>) -> gpui::Div {
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
                    .w(px(440.))
                    .p_5()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(SURFACE))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Rename session"),
                    )
                    .child(self.text_field(
                        EditorTarget::Rename,
                        "Session name",
                        "Example: Build / logs",
                        cx,
                    ))
                    .when(!self.rename_error.is_empty(), |dialog| {
                        dialog.child(div().text_color(rgb(RED)).child(self.rename_error.clone()))
                    })
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("Changes this tab only, not a saved connection."),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Self::button("Cancel", "cancel-rename", false)
                                    .when(self.input.rename_focus == 1, |b| {
                                        b.border_2().border_color(rgb(INK))
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.action(Action::ReturnToTerminal, window, cx);
                                    })),
                            )
                            .child(
                                Self::button("Rename", "apply-rename", true)
                                    .when(self.input.rename_focus == 2, |b| {
                                        b.border_2().border_color(rgb(INK))
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.action(Action::ApplyRename, window, cx);
                                    })),
                            ),
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
                    .max_h(px(if self.input.advanced { 530. } else { 410. }))
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
                                        "Edit SSH target"
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
                                                .child("Library"))
                                            .child(self.field(0, "Display name", "Defaults to host", cx))
                                            .child(self.connection_button(
                                                if self.input.advanced { "Hide advanced access" } else { "Advanced access..." },
                                                "advanced-access", ConnectionButton::Advanced, cx,
                                            ))
                                            .when(!self.input.advanced && (!self.input.fields[4].text.is_empty() || !self.input.fields[5].text.is_empty()), |column| {
                                                column.child(div().text_size(px(12.)).text_color(rgb(BLUE))
                                                    .child(format!("Identity: {} / Jump: {}",
                                                        if self.input.fields[4].text.is_empty() { "SSH config" } else { &self.input.fields[4].text },
                                                        if self.input.fields[5].text.is_empty() { "SSH config" } else { &self.input.fields[5].text })))
                                            }),
                                    ),
                            )
                            .when(self.input.advanced, |fields| {
                                fields.child(
                                    div().pt_3().border_t_1().border_color(rgb(BORDER)).flex().flex_col().gap_3()
                                        .child(div().flex().items_end().gap_2()
                                            .child(self.field(4, "Identity file", "Optional private key path", cx))
                                            .child(self.connection_button("Browse...", "browse-identity", ConnectionButton::Browse, cx).flex_shrink_0()))
                                        .child(self.field(5, "Jump host", "Optional user@jump:port", cx)),
                                )
                            })
                            .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child(
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
                            .when(!self.form_error.is_empty() && self.invalid_field.is_none(), |dialog| {
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
                                    .items_center()
                                    .gap_2()
                                    .when(self.editing_connection.is_some(), |buttons| {
                                        buttons.child(self.connection_button(
                                            "Remove...", "delete-connection", ConnectionButton::Delete, cx,
                                        ).text_color(rgb(RED)))
                                    })
                                    .child(div().flex_1())
                                    .child(self.connection_button(
                                        "Cancel",
                                        "cancel-connection",
                                        ConnectionButton::Cancel,
                                        cx,
                                    ))
                                    .child(self.connection_button(
                                        "Save",
                                        "save-connection",
                                        ConnectionButton::Save,
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
                                div().text_size(px(12.)).text_color(rgb(MUTED)).child(
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
            CloseTarget::Connection(index) => (
                "Remove saved target?",
                format!("Remove {} from the connection library? Open sessions and remote programs are not affected.",
                    self.settings.connections.get(index).map(|c| c.name.as_str()).unwrap_or("this target")),
                "Remove target",
            ),
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
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child("Enter: selected button. Esc: cancel. Tab / arrows: select."),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Self::button(
                                    if matches!(prompt.target, CloseTarget::Connection(_)) {
                                        "Keep target"
                                    } else {
                                        "Keep session"
                                    },
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
        if self.input.surface == Surface::Library && std::mem::take(&mut self.reveal_library) {
            let view = cx.entity();
            window.on_next_frame(move |_, cx| {
                view.update(cx, |this, cx| {
                    if !this.settings.connections.is_empty() {
                        this.library_scroll.scroll_to_item(this.library_selected);
                        cx.notify();
                    }
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
        if !self.notice.is_empty() {
            root = root.child(
                div()
                    .absolute()
                    .bottom(px(42.))
                    .right_3()
                    .max_w(px(540.))
                    .px_3()
                    .py_2()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg()
                    .flex()
                    .gap_3()
                    .items_center()
                    .bg(rgb(RAISED))
                    .text_color(rgb(INK))
                    .child(div().flex_1().child(self.notice.clone()))
                    .child(Self::toolbar_button("Dismiss", "dismiss-notice").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.notice.clear();
                            cx.notify();
                        }),
                    )),
            );
        }
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
        if self.input.surface == Surface::SessionMenu {
            root = root.child(self.session_menu(window, cx));
        }
        if self.input.surface == Surface::Rename {
            root = root.child(self.rename_dialog(cx));
        }
        if self.input.surface == Surface::Details {
            root = root.child(self.details_dialog(cx));
        }
        if self.input.pasting.is_some() {
            root = root.child(self.paste_dialog(cx));
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
