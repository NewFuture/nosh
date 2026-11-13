use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use crate::terminal::{BackendSettings, PtyEvent, TerminalBackend};
use clap::Parser;
use eframe::egui::{self, Color32, RichText};

use crate::connections::{self, Connection, Settings};
use crate::suggestions::SuggestionJob;

const PURPLE: Color32 = Color32::from_rgb(117, 98, 206);
const GREEN: Color32 = Color32::from_rgb(56, 156, 125);
const TERMINAL_BG: Color32 = Color32::from_rgb(24, 24, 24);

#[derive(Parser)]
#[command(
    version,
    about = "Native Rust desktop for local nosh and OpenSSH terminals"
)]
pub struct Options {
    /// Path to the nosh CLI; otherwise use NOSH_BIN, a sibling executable, or PATH.
    #[arg(long)]
    nosh: Option<PathBuf>,
    /// Starting directory for new local terminals and independent AI suggestions.
    #[arg(long)]
    cwd: Option<PathBuf>,
}

struct Session {
    id: u64,
    name: String,
    target: String,
    remote: Option<Connection>,
    folder: PathBuf,
    backend: Option<TerminalBackend>,
    state: String,
    running: bool,
    title: String,
    draft: String,
    answer: String,
    answer_error: bool,
    job: Option<SuggestionJob>,
}

#[derive(Default)]
struct ConnectionForm {
    connection: Connection,
    port: String,
    error: String,
}

pub struct Desktop {
    settings: Settings,
    settings_path: Option<PathBuf>,
    settings_writable: bool,
    nosh: Result<PathBuf, String>,
    folder: PathBuf,
    sessions: Vec<Session>,
    active: Option<u64>,
    next_id: u64,
    events: Receiver<(u64, PtyEvent)>,
    sender: Sender<(u64, PtyEvent)>,
    show_assistant: bool,
    show_connections: bool,
    show_settings: bool,
    appearance_draft: Option<Settings>,
    connection_form: Option<ConnectionForm>,
    confirm_close: Option<u64>,
    confirm_exit: bool,
    allow_exit: bool,
    terminal_focus: bool,
    error: String,
}

impl Desktop {
    pub fn new(cc: &eframe::CreationContext<'_>, options: Options) -> Self {
        let (sender, events) = mpsc::channel();
        let settings_path = dirs::config_dir().map(|dir| dir.join("nosh").join("desktop.json"));
        let loaded = settings_path
            .as_ref()
            .ok_or_else(|| "No user configuration directory is available.".into())
            .and_then(|path| Settings::load(path));
        let (settings, settings_writable, error) = match loaded {
            Ok(settings) => (settings, true, String::new()),
            Err(error) => (
                Settings::default(),
                false,
                format!("{error}\nSettings will not be overwritten. Repair the file and restart."),
            ),
        };
        let folder = options
            .cwd
            .map_or_else(std::env::current_dir, std::fs::canonicalize);
        let mut desktop = Self {
            settings,
            settings_path,
            settings_writable,
            nosh: connections::find_nosh(options.nosh),
            folder: PathBuf::new(),
            sessions: Vec::new(),
            active: None,
            next_id: 1,
            events,
            sender,
            show_assistant: true,
            show_connections: true,
            show_settings: false,
            appearance_draft: None,
            connection_form: None,
            confirm_close: None,
            confirm_exit: false,
            allow_exit: false,
            terminal_focus: true,
            error,
        };
        desktop.apply_style(&cc.egui_ctx);
        match folder {
            Ok(folder) if folder.is_dir() => {
                desktop.folder = folder;
                desktop.new_local(&cc.egui_ctx);
            }
            Ok(folder) => desktop.report(format!("Not a directory: {}", folder.display())),
            Err(error) => desktop.report(format!("Cannot open the starting directory: {error}")),
        }
        desktop
    }

    fn report(&mut self, error: impl Into<String>) {
        let error = error.into();
        eprintln!("nosh-desktop: {error}");
        self.error = error;
    }

    fn apply_style(&self, ctx: &egui::Context) {
        ctx.set_theme(if self.settings.dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });
        ctx.set_visuals(if self.settings.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        ctx.style_mut(|style| {
            style.spacing.item_spacing = egui::vec2(10.0, 9.0);
            style.spacing.button_padding = egui::vec2(12.0, 7.0);
            style.visuals.selection.bg_fill = PURPLE;
            style.visuals.selection.stroke.color = Color32::WHITE;
            style.visuals.hyperlink_color = PURPLE;
            style.visuals.window_corner_radius = egui::CornerRadius::same(10);
            for widget in [
                &mut style.visuals.widgets.inactive,
                &mut style.visuals.widgets.hovered,
                &mut style.visuals.widgets.active,
            ] {
                widget.corner_radius = egui::CornerRadius::same(5);
            }
            style.visuals.panel_fill = if self.settings.dark {
                Color32::from_rgb(32, 33, 39)
            } else {
                Color32::from_rgb(250, 249, 247)
            };
        });
    }

    fn persist(&mut self, settings: Settings) -> bool {
        if !self.settings_writable {
            self.report("Settings were not saved because the configuration could not be loaded.");
            return false;
        }
        let result = self
            .settings_path
            .as_ref()
            .ok_or("No settings directory.".to_string())
            .and_then(|path| settings.save(path));
        match result {
            Ok(()) => {
                self.settings = settings;
                true
            }
            Err(error) => {
                self.report(format!("Cannot save desktop settings: {error}"));
                false
            }
        }
    }

    fn spawn(
        &mut self,
        ctx: &egui::Context,
        name: String,
        remote: Option<Connection>,
        settings: BackendSettings,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        match TerminalBackend::new(id, ctx.clone(), self.sender.clone(), settings) {
            Ok(backend) => {
                let target = remote
                    .as_ref()
                    .map(Connection::target)
                    .unwrap_or_else(|| "This computer".into());
                self.sessions.push(Session {
                    id,
                    name,
                    target,
                    remote,
                    folder: self.folder.clone(),
                    backend: Some(backend),
                    state: "Process running".into(),
                    running: true,
                    title: String::new(),
                    draft: String::new(),
                    answer: String::new(),
                    answer_error: false,
                    job: None,
                });
                self.active = Some(id);
                self.terminal_focus = true;
            }
            Err(error) => self.report(format!("Cannot open terminal: {error}")),
        }
    }

    fn new_local(&mut self, ctx: &egui::Context) {
        match &self.nosh {
            Ok(nosh) => self.spawn(
                ctx,
                "nosh".into(),
                None,
                connections::local_backend(nosh, &self.folder),
            ),
            Err(error) => self.report(error.clone()),
        }
    }

    fn new_ssh(&mut self, ctx: &egui::Context, connection: Connection) {
        match connections::ssh_backend(&connection) {
            Ok(settings) => self.spawn(ctx, connection.name.clone(), Some(connection), settings),
            Err(error) => self.report(error),
        }
    }

    fn active_index(&self) -> Option<usize> {
        self.active
            .and_then(|id| self.sessions.iter().position(|session| session.id == id))
    }

    fn process_events(&mut self) {
        for _ in 0..512 {
            let Ok((id, event)) = self.events.try_recv() else {
                break;
            };
            let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) else {
                continue;
            };
            match event {
                PtyEvent::Title(title) => {
                    session.title = title
                        .chars()
                        .filter(|ch| !ch.is_control())
                        .take(120)
                        .collect();
                }
                PtyEvent::ResetTitle => session.title.clear(),
                event @ (PtyEvent::PtyWrite(_)
                | PtyEvent::TextAreaSizeRequest(_)
                | PtyEvent::ColorRequest(..))
                    if session.running =>
                {
                    if let Some(backend) = &mut session.backend
                        && let Err(error) = backend.reply(event)
                    {
                        self.error = format!("Terminal protocol response failed: {error}");
                    }
                }
                PtyEvent::ChildExit(code) => {
                    session.running = false;
                    session.state = format!("Exited with code {code}");
                    if let Some(job) = &session.job {
                        job.cancel();
                    }
                }
                PtyEvent::Exit => {
                    session.running = false;
                    if session.state == "Process running" {
                        session.state = "Process exited".into();
                    }
                    if let Some(job) = &session.job {
                        job.cancel();
                    }
                }
                // OSC clipboard requests never read or replace the system clipboard.
                _ => {}
            }
        }
        for session in &mut self.sessions {
            if let Some(result) = session.job.as_ref().and_then(SuggestionJob::poll) {
                session.job = None;
                session.answer_error = result.is_err();
                session.answer = match result {
                    Ok(command) => command,
                    Err(error) => error,
                };
            }
        }
    }

    fn header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .exact_height(60.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new("nosh.").size(30.0).strong().color(PURPLE));
                    ui.label(
                        RichText::new("DESKTOP")
                            .size(10.0)
                            .color(ui.visuals().weak_text_color()),
                    );
                    ui.add_space(25.0);
                    ui.label(
                        RichText::new("Your shell. A little more capable.")
                            .size(12.0)
                            .weak(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(8.0);
                        if ui.button("Settings").clicked() {
                            self.show_settings = true;
                        }
                        if ui
                            .selectable_label(self.show_assistant, "AI suggestions")
                            .clicked()
                        {
                            self.show_assistant = !self.show_assistant;
                        }
                        if ui
                            .selectable_label(self.show_connections, "Connections")
                            .clicked()
                        {
                            self.show_connections = !self.show_connections;
                        }
                        ui.label(RichText::new("RUST NATIVE").size(10.0).color(GREEN));
                    });
                });
            });
        egui::TopBottomPanel::bottom("status")
            .exact_height(30.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("Local inference / offline")
                            .size(11.0)
                            .color(GREEN),
                    );
                    ui.separator();
                    if let Some(index) = self.active_index() {
                        let session = &self.sessions[index];
                        ui.label(
                            RichText::new(format!("{} / {}", session.target, session.state))
                                .size(11.0),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new("Suggestions never auto-execute")
                                .size(10.0)
                                .weak(),
                        );
                    });
                });
            });
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        if !self.show_connections {
            return;
        }
        egui::SidePanel::left("connections").default_width(215.0).width_range(170.0..=300.0).show(ctx, |ui| {
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("WORKSPACE").size(11.0).weak().strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("+ SSH").clicked() { self.connection_form = Some(ConnectionForm::default()); }
                });
            });
            ui.add_space(25.0);
            ui.label(RichText::new("ON THIS DEVICE").size(10.0).weak());
            ui.add_space(5.0);
            if ui.button("This computer    +").on_hover_text("Open another independent local nosh session").clicked() { self.new_local(ctx); }
            ui.label(RichText::new(self.folder.display().to_string()).size(10.0).weak());
            ui.add_space(28.0);
            ui.label(RichText::new("SSH CONNECTIONS").size(10.0).weak());
            ui.add_space(8.0);
            let mut connect = None;
            let mut remove = None;
            for (index, connection) in self.settings.connections.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        if ui.button(&connection.name).clicked() { connect = Some(connection.clone()); }
                        ui.label(RichText::new(connection.target()).size(10.0).weak());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        if ui.small_button("x").on_hover_text("Remove saved connection (does not close sessions)").clicked() { remove = Some(index); }
                    });
                });
                ui.add_space(7.0);
            }
            if let Some(connection) = connect { self.new_ssh(ctx, connection); }
            if let Some(index) = remove {
                let mut next = self.settings.clone();
                next.connections.remove(index);
                self.persist(next);
            }
            if self.settings.connections.is_empty() {
                ui.label(RichText::new("No saved hosts yet.\nUse a hostname or your\nOpenSSH config alias.").size(11.0).weak());
            }
            ui.add_space(10.0);
            if ui.button("+ New connection").clicked() { self.connection_form = Some(ConnectionForm::default()); }
            ui.add_space(30.0);
            ui.separator();
            ui.add_space(10.0);
            ui.label(RichText::new("Your machine. Your models.").size(11.0).color(GREEN));
            ui.label(RichText::new("Passwords and host verification\nstay in the SSH terminal.\nNothing is sent to a cloud API.").size(10.0).weak());
        });
    }

    fn assistant(&mut self, ctx: &egui::Context) {
        if !self.show_assistant {
            return;
        }
        egui::SidePanel::right("assistant").default_width(320.0).width_range(270.0..=500.0).show(ctx, |ui| {
            ui.add_space(20.0);
            ui.label(RichText::new("A second pair of eyes.").size(18.0).strong());
            ui.label(RichText::new("Real local AI. No automatic execution.").size(11.0).weak());
            ui.add_space(15.0);
            ui.separator();
            let Some(index) = self.active_index() else { ui.label("Open a local terminal to begin."); return; };
            let another_job = self.sessions.iter().any(|session| session.id != self.sessions[index].id && session.job.is_some());
            let session = &mut self.sessions[index];
            ui.add_space(9.0);
            ui.label(RichText::new(format!("Bound to {}", session.target)).size(11.0).color(PURPLE));
            if session.remote.is_some() {
                ui.add_space(15.0);
                ui.label("This is a basic OpenSSH session.");
                ui.label(RichText::new("Remote AI and remote Agent tools are not connected. No local project context will be mistaken for remote context.").size(12.0).weak());
                ui.add_space(10.0);
                ui.label("If nosh is installed remotely, run it directly in this terminal.");
                return;
            }
            ui.label(RichText::new(format!("Project folder: {}", session.folder.display())).size(10.0).weak());
            ui.label(RichText::new("Suggestions use a separate read-only shell in this folder, not this terminal's live cwd or variables.").size(10.0).weak());
            ui.add_space(15.0);
            let busy = session.job.is_some();
            ui.add_enabled(!busy && session.running, egui::TextEdit::multiline(&mut session.draft).hint_text("Describe the command you need...").desired_rows(4).desired_width(f32::INFINITY).char_limit(8192));
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy && !another_job && session.running && !session.draft.trim().is_empty(), egui::Button::new("Generate command").fill(PURPLE)).clicked() {
                    match &self.nosh {
                        Ok(nosh) => match SuggestionJob::start(nosh, &session.folder, &session.draft, ctx.clone()) {
                            Ok(job) => { session.job = Some(job); session.answer.clear(); session.answer_error = false; }
                            Err(error) => { session.answer = format!("Cannot start suggestion: {error}"); session.answer_error = true; }
                        },
                        Err(error) => { session.answer = error.clone(); session.answer_error = true; }
                    }
                }
                if let Some(job) = &session.job {
                    ui.spinner();
                    if ui.button("Stop").clicked() { job.cancel(); }
                }
            });
            if busy { ui.label(RichText::new("Loading model / generating locally...").size(11.0).weak()); }
            if another_job { ui.label(RichText::new("Another terminal is generating a suggestion.").size(11.0).weak()); }
            ui.add_space(15.0);
            egui::ScrollArea::vertical().id_salt(("answer", session.id)).max_height(ui.available_height() - 115.0).show(ui, |ui| {
                if !session.answer.is_empty() {
                    if session.answer_error {
                        ui.colored_label(ui.visuals().error_fg_color, &session.answer);
                    } else {
                        ui.label(RichText::new("DRAFT COMMAND").size(10.0).weak());
                        ui.add(egui::Label::new(RichText::new(&session.answer).monospace()).selectable(true).wrap());
                        if ui.button("Copy command").on_hover_text("Copies only. Paste into the appropriate shell after checking its prompt and input.").clicked() { ctx.copy_text(session.answer.clone()); }
                    }
                } else if !busy {
                    ui.label(RichText::new("Ask in plain language. Review the generated command, then copy and paste it yourself.").size(12.0).weak());
                }
            });
            ui.add_space(15.0);
            ui.separator();
            ui.label(RichText::new("Model required").strong().size(11.0));
            ui.label(RichText::new("Uses installed models only. To install, run `nosh model pull` in the terminal. Downloads require your explicit action.").size(11.0).weak());
            ui.add_space(8.0);
            ui.label(RichText::new("For a full Agent, type # followed by a space and your task in nosh. Its real approvals remain in the terminal.").size(11.0).weak());
        });
    }

    fn terminal(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(ctx.style().visuals.panel_fill)).show(ctx, |ui| {
            let mut select = None;
            let mut close = None;
            egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 4)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    egui::ScrollArea::horizontal().max_width((ui.available_width() - 65.0).max(10.0)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for session in &self.sessions {
                                let kind = if session.remote.is_some() { "SSH" } else { "LOCAL" };
                                if ui.selectable_label(self.active == Some(session.id), format!("{}  {kind}", session.name)).clicked() { select = Some(session.id); }
                                if ui.small_button("x").on_hover_text("Close terminal session").clicked() { close = Some(session.id); }
                            }
                        });
                    });
                    if ui.button("+").on_hover_text("New local terminal").clicked() { self.new_local(ctx); }
                });
            });
            if let Some(id) = select { self.active = Some(id); self.terminal_focus = true; }
            if let Some(id) = close { self.confirm_close = Some(id); }
            ui.separator();
            let Some(index) = self.active_index() else {
                ui.add_space(70.0);
                ui.vertical_centered(|ui| {
                    ui.heading("Your next workspace starts here.");
                    ui.label("Open a local nosh terminal or add an SSH connection.");
                    if ui.button("New local terminal").clicked() { self.new_local(ctx); }
                });
                return;
            };
            let modal_open = self.connection_form.is_some() || self.show_settings || self.confirm_close.is_some() || self.confirm_exit;
            let session = &mut self.sessions[index];
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(RichText::new(&session.target).strong().size(12.0));
                ui.label(RichText::new(&session.state).size(11.0).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if session.running && ui.small_button("Disconnect").clicked() { self.confirm_close = Some(session.id); }
                    if !session.running && ui.small_button("Reconnect / new shell").clicked() { select = Some(session.id); }
                });
            });
            if session.remote.is_some() {
                ui.label(RichText::new("  SSH authenticates in this terminal. Verify the real host fingerprint before accepting.").size(10.0).weak());
            } else {
                ui.label(RichText::new(format!("  Started in {} (not a live cwd indicator)", session.folder.display())).size(10.0).weak());
            }
            ui.add_space(4.0);
            if let Some(backend) = &mut session.backend {
                let rect = ui.available_rect_before_wrap();
                if ctx.input(|input| input.pointer.any_pressed())
                    && let Some(pos) = ctx.input(|input| input.pointer.interact_pos())
                {
                    self.terminal_focus = rect.contains(pos) && !modal_open;
                }
                egui::Frame::new().fill(TERMINAL_BG).inner_margin(8).show(ui, |ui| {
                    if let Err(error) = backend.show(ui, self.settings.font_size, self.terminal_focus && session.running && !modal_open) {
                        self.error = format!("Terminal I/O failed: {error}");
                    }
                });
            }
            if let Some(id) = select
                && let Some(old) = self.sessions.iter().find(|session| session.id == id && !session.running)
            {
                match old.remote.clone() {
                    Some(connection) => self.new_ssh(ctx, connection),
                    None => self.new_local(ctx),
                }
            }
        });
    }

    fn connection_window(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.connection_form.take() else {
            return;
        };
        let mut open = true;
        let mut action = None;
        egui::Window::new("New SSH connection").open(&mut open).collapsible(false).resizable(false).default_width(420.0).show(ctx, |ui| {
            ui.label(RichText::new("A real connection, using your system OpenSSH.").weak());
            ui.add_space(8.0);
            egui::Grid::new("ssh-fields").num_columns(2).spacing([12.0, 12.0]).show(ui, |ui| {
                for (label, text) in [
                    ("Name", &mut form.connection.name),
                    ("Host / config alias", &mut form.connection.host),
                    ("User (optional)", &mut form.connection.user),
                    ("Port (optional)", &mut form.port),
                    ("Identity file (optional)", &mut form.connection.identity),
                    ("Jump host (optional)", &mut form.connection.jump),
                ] {
                    ui.label(label);
                    ui.add(egui::TextEdit::singleline(text).desired_width(255.0).char_limit(4096));
                    ui.end_row();
                }
            });
            ui.add_space(12.0);
            ui.label(RichText::new("Blank user / port / identity / jump host use OpenSSH config defaults. Identity is a file path, never a private key.").size(11.0).weak());
            ui.label(RichText::new("Host verification and password prompts appear in the terminal. No credentials are stored. Agent forwarding and local config commands are disabled.").size(11.0).weak());
            if !form.error.is_empty() { ui.colored_label(ui.visuals().error_fg_color, &form.error); }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Connect once").clicked() { action = Some(false); }
                if ui.add_enabled(self.settings_writable, egui::Button::new("Save and connect").fill(PURPLE)).clicked() { action = Some(true); }
            });
        });
        if !open {
            return;
        }
        if let Some(save) = action {
            let result = if form.port.trim().is_empty() {
                form.connection.port = None;
                Ok(())
            } else {
                form.port
                    .trim()
                    .parse::<u16>()
                    .map(|port| form.connection.port = Some(port))
                    .map_err(|_| "Port must be between 1 and 65535.".to_string())
            }
            .and_then(|()| form.connection.validate());
            match result {
                Err(error) => form.error = error,
                Ok(()) => {
                    if save {
                        let mut next = self.settings.clone();
                        if next
                            .connections
                            .iter()
                            .any(|connection| connection.name == form.connection.name)
                        {
                            form.error = "A connection with this name already exists.".into();
                            self.connection_form = Some(form);
                            return;
                        }
                        next.connections.push(form.connection.clone());
                        if !self.persist(next) {
                            form.error = self.error.clone();
                            self.connection_form = Some(form);
                            return;
                        }
                    }
                    self.new_ssh(ctx, form.connection);
                    return;
                }
            }
        }
        self.connection_form = Some(form);
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let mut next = self
            .appearance_draft
            .take()
            .unwrap_or_else(|| self.settings.clone());
        egui::Window::new("Settings").open(&mut open).collapsible(false).default_width(450.0).show(ctx, |ui| {
            ui.heading("Small details. Better flow.");
            ui.add_space(15.0);
            ui.checkbox(&mut next.dark, "Dark appearance");
            ui.add(egui::Slider::new(&mut next.font_size, 10.0..=24.0).text("Terminal font size"));
            ui.separator();
            ui.label("nosh executable");
            match &self.nosh {
                Ok(path) => { ui.label(RichText::new(path.display().to_string()).monospace().size(11.0)); }
                Err(error) => { ui.colored_label(ui.visuals().error_fg_color, error); }
            }
            ui.label(RichText::new("Override with --nosh PATH or NOSH_BIN at launch.").weak().size(11.0));
            ui.add_space(10.0);
            ui.label("Desktop settings");
            if let Some(path) = &self.settings_path { ui.label(RichText::new(path.display().to_string()).monospace().size(11.0)); }
            ui.label(RichText::new("Only connection metadata and appearance are saved. No terminal output, passwords, tasks or model responses are persisted.").weak().size(11.0));
            if ui.add_enabled(self.settings_writable, egui::Button::new("Save appearance")).clicked() && self.persist(next.clone()) { self.apply_style(ctx); }
        });
        self.show_settings = open;
        self.appearance_draft = open.then_some(next);
    }

    fn confirmations(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.confirm_close {
            let mut close = false;
            let mut cancel = false;
            egui::Window::new("Close session?").collapsible(false).resizable(false).show(ctx, |ui| {
                let name = self.sessions.iter().find(|session| session.id == id).map(|session| session.target.as_str()).unwrap_or("session");
                ui.label(format!("Close {name}?"));
                ui.label("This closes its PTY and cancels its AI request.");
                ui.label("Remote commands may continue on the server. Nothing is rolled back or replayed.");
                ui.horizontal(|ui| {
                    cancel = ui.button("Keep session").clicked();
                    close = ui.button("Close session").clicked();
                });
            });
            if close {
                self.sessions.retain(|session| session.id != id);
                if self.active == Some(id) {
                    self.active = self.sessions.last().map(|session| session.id);
                }
            }
            if close || cancel {
                self.confirm_close = None;
            }
        }
        if self.confirm_exit {
            egui::Window::new("Exit nosh desktop?")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Close all terminal sessions and cancel local AI requests?");
                    ui.label("Closing an SSH transport does not guarantee remote commands stop.");
                    ui.horizontal(|ui| {
                        if ui.button("Keep working").clicked() {
                            self.confirm_exit = false;
                        }
                        if ui.button("Close all and exit").clicked() {
                            self.sessions.clear();
                            self.allow_exit = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
        }
    }
}

impl eframe::App for Desktop {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_events();
        if ctx.input(|input| input.viewport().close_requested())
            && !self.allow_exit
            && self
                .sessions
                .iter()
                .any(|session| session.running || session.job.is_some())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_exit = true;
        }
        self.header(ctx);
        if !self.error.is_empty() {
            egui::TopBottomPanel::top("error").show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, &self.error);
                    if ui.button("Dismiss").clicked() {
                        self.error.clear();
                    }
                });
            });
        }
        self.sidebar(ctx);
        self.assistant(ctx);
        self.terminal(ctx);
        self.connection_window(ctx);
        self.settings_window(ctx);
        self.confirmations(ctx);
        if self.sessions.iter().any(|session| session.job.is_some()) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}
