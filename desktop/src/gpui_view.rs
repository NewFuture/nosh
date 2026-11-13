use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point as TermPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{TermMode, cell::Flags, viewport_to_point};
use eframe::egui;
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, ScrollWheelEvent,
    SharedString, TextRun, UTF16Selection, Window, canvas, div, fill, font, point, prelude::*, px,
    rgb, size,
};
use nosh_desktop::connections::{self, Connection, Settings};
use nosh_desktop::terminal::{
    BackendSettings, PtyEvent, TerminalBackend, keyboard_input, terminal_color,
};

#[derive(clap::Parser)]
#[command(about = "Experimental native GPUI nosh terminal")]
pub struct Options {
    #[arg(long)]
    nosh: Option<PathBuf>,
    #[arg(long)]
    cwd: Option<PathBuf>,
}

struct Tab {
    id: u64,
    title: String,
    target: String,
    status: String,
    backend: TerminalBackend,
}

#[derive(Clone, Copy)]
enum Close {
    Tab(u64),
    Window,
}

pub struct Desktop {
    focus: FocusHandle,
    nosh: Result<PathBuf, String>,
    cwd: Result<PathBuf, String>,
    settings: Settings,
    settings_path: Option<PathBuf>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    sender: Sender<(u64, PtyEvent)>,
    events: Receiver<(u64, PtyEvent)>,
    dirty: Arc<AtomicBool>,
    error: String,
    form: bool,
    fields: [String; 6],
    field: Option<usize>,
    field_selection: bool,
    composition: String,
    closing: Option<Close>,
    bounds: Bounds<Pixels>,
    cell: gpui::Size<Pixels>,
    selecting: bool,
}

impl Desktop {
    pub fn new(options: Options, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (sender, events) = mpsc::channel();
        let settings_path = dirs::config_dir().map(|p| p.join("nosh").join("desktop-gpui.json"));
        let loaded = settings_path.as_ref().map_or_else(
            || Err("No system configuration directory; settings will not be saved.".into()),
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
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            sender,
            events,
            dirty: Arc::new(AtomicBool::new(true)),
            error,
            form: false,
            fields: Default::default(),
            field: None,
            field_selection: false,
            composition: String::new(),
            closing: None,
            bounds: Bounds::default(),
            cell: size(px(9.), px(20.)),
            selecting: false,
        };
        desktop.open_local(cx);
        window.focus(&desktop.focus);
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            view.update(cx, |this, cx| {
                this.closing = Some(Close::Window);
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
            let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
                continue;
            };
            match event {
                PtyEvent::ChildExit(code) => tab.status = format!("Exited with code {code}"),
                PtyEvent::Exit
                    if tab.status == "Process running (SSH authentication unknown)"
                        || tab.status == "Process running" =>
                {
                    tab.status = "PTY closed".into();
                }
                _ => {}
            }
        }
    }

    fn open(
        &mut self,
        title: String,
        target: String,
        settings: BackendSettings,
        ssh: bool,
        cx: &mut Context<Self>,
    ) {
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
                self.tabs.push(Tab {
                    id: self.next_id,
                    title,
                    target,
                    status: if ssh {
                        "Process running (SSH authentication unknown)"
                    } else {
                        "Process running"
                    }
                    .into(),
                    backend,
                });
                self.next_id += 1;
                self.active = self.tabs.len() - 1;
                self.field = None;
                self.composition.clear();
                self.selecting = false;
            }
            Err(error) => self.error = format!("Cannot start terminal: {error}"),
        }
        cx.notify();
    }

    fn open_local(&mut self, cx: &mut Context<Self>) {
        match (&self.nosh, &self.cwd) {
            (Ok(nosh), Ok(cwd)) => self.open(
                format!("Local {}", self.next_id),
                format!("LOCAL | Startup directory: {}", cwd.display()),
                connections::local_backend(nosh, cwd),
                false,
                cx,
            ),
            (Err(error), _) | (_, Err(error)) => {
                self.error = error.clone();
                cx.notify();
            }
        }
    }

    fn open_ssh(&mut self, connection: Connection, save: bool, cx: &mut Context<Self>) {
        match connections::ssh_backend(&connection) {
            Ok(backend) => {
                if save {
                    let mut settings = self.settings.clone();
                    settings.connections.push(connection.clone());
                    let result = self
                        .settings_path
                        .as_ref()
                        .ok_or_else(|| "Settings saving is disabled; use Connect once.".to_string())
                        .and_then(|path| settings.save(path));
                    if let Err(error) = result {
                        self.error = error;
                        cx.notify();
                        return;
                    }
                    self.settings = settings;
                }
                self.form = false;
                self.open(
                    connection.name.clone(),
                    format!("SSH | {}", connection.target()),
                    backend,
                    true,
                    cx,
                );
            }
            Err(error) => {
                self.error = error;
                cx.notify();
            }
        }
    }

    fn connect_form(&mut self, save: bool, cx: &mut Context<Self>) {
        let port = if self.fields[3].is_empty() {
            None
        } else {
            match self.fields[3].parse::<u16>() {
                Ok(port) if port != 0 => Some(port),
                _ => {
                    self.error = "Port must be between 1 and 65535.".into();
                    cx.notify();
                    return;
                }
            }
        };
        self.open_ssh(
            Connection {
                name: self.fields[0].clone(),
                host: self.fields[1].clone(),
                user: self.fields[2].clone(),
                port,
                identity: self.fields[4].clone(),
                jump: self.fields[5].clone(),
            },
            save,
            cx,
        );
    }

    fn write(&mut self, bytes: Vec<u8>) {
        if let Some(tab) = self.tabs.get(self.active) {
            if let Err(error) = tab.backend.write(bytes) {
                self.error = format!("Terminal input: {error}");
            } else {
                tab.backend.model().lock().scroll_display(Scroll::Bottom);
            }
        }
    }

    fn paste(&mut self, text: &str) {
        if self.field.is_some() {
            self.insert(text);
        } else if let Some(tab) = self.tabs.get(self.active) {
            let mode = *tab.backend.model().lock().mode();
            if let Some(bytes) = keyboard_input(
                &egui::Event::Paste(text.into()),
                mode,
                egui::Modifiers::NONE,
            ) {
                self.write(bytes);
            }
        }
    }

    fn copy_selection(&self, cx: &mut App) {
        if let Some(text) = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.backend.model().lock().selection_to_string())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn set_font_size(&mut self, font_size: f32, cx: &mut Context<Self>) {
        self.settings.font_size = font_size.clamp(10., 24.);
        if let Some(path) = &self.settings_path
            && let Err(error) = self.settings.save(path)
        {
            self.error = error;
        }
        cx.notify();
    }

    fn insert(&mut self, text: &str) {
        if let Some(index) = self.field {
            let value = &mut self.fields[index];
            if self.field_selection {
                value.clear();
                self.field_selection = false;
            }
            if value.len() + text.len() <= 4096 {
                value.extend(text.chars().filter(|ch| !ch.is_control()));
            } else {
                self.error = "Field input exceeds 4096 bytes.".into();
            }
        } else {
            self.write(text.as_bytes().to_vec());
        }
    }

    fn key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if modifiers.control && modifiers.shift && matches!(key, "t" | "w" | "c" | "v") {
            match key {
                "t" => self.open_local(cx),
                "w" => self.closing = self.tabs.get(self.active).map(|tab| Close::Tab(tab.id)),
                "c" => {
                    self.copy_selection(cx);
                }
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.paste(&text);
                    }
                }
                _ => return,
            }
        } else if modifiers.control && matches!(key, "+" | "=" | "-" | "0") {
            let font_size = match key {
                "-" => self.settings.font_size - 1.,
                "0" => 14.,
                _ => self.settings.font_size + 1.,
            };
            self.set_font_size(font_size, cx);
        } else if modifiers.control && key == "tab" && !self.tabs.is_empty() {
            self.active = (self.active
                + if modifiers.shift {
                    self.tabs.len() - 1
                } else {
                    1
                })
                % self.tabs.len();
            self.field = None;
            self.selecting = false;
        } else if self.field.is_some() {
            let index = self.field.unwrap();
            match key {
                "backspace" => {
                    if self.field_selection {
                        self.fields[index].clear();
                    } else {
                        self.fields[index].pop();
                    }
                    self.field_selection = false;
                }
                "tab" => {
                    self.field = Some((index + if modifiers.shift { 5 } else { 1 }) % 6);
                    self.field_selection = false;
                }
                "escape" => {
                    self.form = false;
                    self.field = None;
                }
                "enter" => self.connect_form(false, cx),
                "a" if modifiers.control => self.field_selection = true,
                "v" if modifiers.control => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.paste(&text);
                    }
                }
                "c" if modifiers.control && self.field_selection => {
                    cx.write_to_clipboard(ClipboardItem::new_string(self.fields[index].clone()))
                }
                _ => return,
            }
        } else if let Some(tab) = self.tabs.get(self.active) {
            let mode = *tab.backend.model().lock().mode();
            if let Some(bytes) = terminal_key(event, mode) {
                self.write(bytes);
            } else {
                return;
            }
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn mouse(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.field = None;
        self.composition.clear();
        window.focus(&self.focus);
        self.selecting = true;
        self.select(event.position, true);
        cx.notify();
    }

    fn select(&mut self, pos: Point<Pixels>, start: bool) {
        if let Some(tab) = self.tabs.get(self.active) {
            let mut term = tab.backend.model().lock();
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

    fn paint_terminal(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bounds = bounds;
        let font = font(if cfg!(windows) {
            "Consolas"
        } else {
            "monospace"
        });
        let font_size = px(self.settings.font_size);
        let run = TextRun {
            len: 1,
            font: font.clone(),
            color: rgb(0xdadce8).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let width = window
            .text_system()
            .shape_line("M".into(), font_size, &[run], None)
            .width
            .ceil();
        self.cell = size(width, px((self.settings.font_size * 1.4).ceil()));
        if let Some(tab) = self.tabs.get_mut(self.active)
            && let Err(error) = tab.backend.resize(
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

    fn draw(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        window.paint_quad(fill(bounds, rgb(0x1c1e26)));
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let term = tab.backend.model().lock();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let font_size = px(self.settings.font_size);
        let font = font(if cfg!(windows) {
            "Consolas"
        } else {
            "monospace"
        });
        // Paint every background before glyphs so wide glyphs are not covered by spacer cells.
        for cell in term.renderable_content().display_iter {
            let row = cell.point.line.0 + offset;
            if row < 0 {
                continue;
            }
            let origin = bounds.origin
                + point(
                    self.cell.width * cell.point.column.0 as f32,
                    self.cell.height * row as f32,
                );
            let bg = if content.selection.is_some_and(|s| s.contains(cell.point)) {
                egui::Color32::from_rgb(73, 65, 103)
            } else {
                terminal_color(
                    if cell.flags.contains(Flags::INVERSE) {
                        cell.fg
                    } else {
                        cell.bg
                    },
                    content.colors,
                )
            };
            window.paint_quad(fill(Bounds::new(origin, self.cell), gpui_color(bg)));
        }
        for cell in content.display_iter {
            let row = cell.point.line.0 + offset;
            if row < 0 {
                continue;
            }
            let origin = bounds.origin
                + point(
                    self.cell.width * cell.point.column.0 as f32,
                    self.cell.height * row as f32,
                );
            let mut fg = terminal_color(
                if cell.flags.contains(Flags::INVERSE) {
                    cell.bg
                } else {
                    cell.fg
                },
                content.colors,
            );
            if cell.c == ' '
                || cell.flags.intersects(
                    Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER | Flags::HIDDEN,
                )
            {
                continue;
            }
            let mut text = cell.c.to_string();
            if let Some(zero_width) = cell.zerowidth() {
                text.extend(zero_width);
            }
            if cell.flags.contains(Flags::DIM) {
                fg = fg.gamma_multiply(0.65);
            }
            let mut cell_font = font.clone();
            if cell.flags.contains(Flags::BOLD) {
                cell_font.weight = gpui::FontWeight::BOLD;
            }
            let run = TextRun {
                len: text.len(),
                font: cell_font,
                color: gpui_color(fg),
                background_color: None,
                underline: cell
                    .flags
                    .contains(Flags::UNDERLINE)
                    .then_some(gpui::UnderlineStyle {
                        thickness: px(1.),
                        color: None,
                        wavy: false,
                    }),
                strikethrough: None,
            };
            let line = window.text_system().shape_line(
                text.into(),
                font_size,
                &[run],
                Some(self.cell.width),
            );
            if let Err(error) = line.paint(origin, self.cell.height, window, cx) {
                eprintln!("nosh-gpui: paint terminal text: {error:#}");
            }
        }
        let cursor = content.cursor;
        let row = cursor.point.line.0 + offset;
        if cursor.shape != alacritty_terminal::vte::ansi::CursorShape::Hidden && row >= 0 {
            let origin = bounds.origin
                + point(
                    self.cell.width * cursor.point.column.0 as f32,
                    self.cell.height * row as f32,
                );
            window.paint_quad(fill(
                Bounds::new(
                    origin + point(px(0.), self.cell.height - px(2.)),
                    size(self.cell.width, px(2.)),
                ),
                rgb(0xb59ae0),
            ));
        }
    }

    fn button(
        label: impl Into<SharedString>,
        id: impl Into<gpui::ElementId>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(0x343543))
            .hover(|s| s.bg(rgb(0x4a4661)))
            .cursor_pointer()
            .child(label.into())
    }
}

fn gpui_color(color: egui::Color32) -> gpui::Hsla {
    rgb((u32::from(color.r()) << 16) | (u32::from(color.g()) << 8) | u32::from(color.b())).into()
}

fn terminal_key(event: &KeyDownEvent, mode: TermMode) -> Option<Vec<u8>> {
    let m = event.keystroke.modifiers;
    let name = match event.keystroke.key.as_str() {
        "enter" => "Enter",
        "escape" => "Escape",
        "backspace" => "Backspace",
        "tab" => "Tab",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "delete" => "Delete",
        "insert" => "Insert",
        other => other,
    };
    let modifiers = egui::Modifiers {
        alt: m.alt,
        ctrl: m.control,
        shift: m.shift,
        mac_cmd: m.platform,
        command: m.platform || m.control,
    };
    if let Some(key) =
        egui::Key::from_name(name).or_else(|| egui::Key::from_name(&name.to_uppercase()))
        && (name.len() != 1 || m.control)
    {
        return keyboard_input(
            &egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: event.is_held,
                modifiers,
            },
            mode,
            modifiers,
        );
    }
    if m.alt
        && !m.control
        && let Some(text) = &event.keystroke.key_char
    {
        return Some(format!("\x1b{text}").into_bytes());
    }
    None
}

impl Render for Desktop {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let paint_view = view.clone();
        let input_view = view.clone();
        let mut header = div()
            .flex()
            .gap_2()
            .items_center()
            .p_3()
            .bg(rgb(0x252631))
            .child(
                div()
                    .text_lg()
                    .text_color(rgb(0xcab1f3))
                    .child("nosh / GPUI"),
            )
            .child(
                Self::button("+ Local", "local")
                    .on_click(cx.listener(|this, _, _, cx| this.open_local(cx))),
            )
            .child(
                Self::button("+ SSH", "ssh").on_click(cx.listener(|this, _, _, cx| {
                    this.form = !this.form;
                    this.field = this.form.then_some(0);
                    this.field_selection = false;
                    cx.notify();
                })),
            )
            .child(Self::button("Close tab", "close-tab").on_click(cx.listener(
                |this, _, _, cx| {
                    this.closing = this.tabs.get(this.active).map(|tab| Close::Tab(tab.id));
                    cx.notify();
                },
            )))
            .child(
                Self::button("Copy selection", "copy")
                    .on_click(cx.listener(|this, _, _, cx| this.copy_selection(cx))),
            )
            .child(
                Self::button("Paste", "paste").on_click(cx.listener(|this, _, _, cx| {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        this.paste(&text);
                        cx.notify();
                    }
                })),
            );
        header = header
            .child(div().flex_1())
            .child(
                Self::button("A-", "zoom-out").on_click(cx.listener(|this, _, _, cx| {
                    this.set_font_size(this.settings.font_size - 1., cx);
                })),
            )
            .child(
                Self::button("A+", "zoom-in").on_click(cx.listener(|this, _, _, cx| {
                    this.set_font_size(this.settings.font_size + 1., cx);
                })),
            )
            .child(format!("{} px  |  Ctrl +/- zoom", self.settings.font_size));
        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1c1e26))
            .text_color(rgb(0xdadce8))
            .text_size(px(13.))
            .font_family("Segoe UI")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .child(header);
        if !self.error.is_empty() {
            root = root.child(
                div()
                    .flex()
                    .p_2()
                    .gap_2()
                    .bg(rgb(0x542f3a))
                    .child(self.error.clone())
                    .child(Self::button("Dismiss", "dismiss").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.error.clear();
                            cx.notify();
                        },
                    ))),
            );
        }
        if self.closing.is_some() {
            root = root.child(
                div()
                    .flex()
                    .p_2()
                    .gap_2()
                    .bg(rgb(0x53432e))
                    .child("Close PTY and terminate its local process? Remote jobs may survive.")
                    .child(
                        Self::button("Confirm close", "confirm").on_click(cx.listener(
                            |this, _, window, cx| {
                                match this.closing.take() {
                                    Some(Close::Window) => {
                                        this.tabs.clear();
                                        window.remove_window();
                                        cx.quit();
                                    }
                                    Some(Close::Tab(id)) => {
                                        if let Some(index) =
                                            this.tabs.iter().position(|tab| tab.id == id)
                                        {
                                            this.tabs.remove(index);
                                            if index < this.active {
                                                this.active -= 1;
                                            }
                                            this.active =
                                                this.active.min(this.tabs.len().saturating_sub(1));
                                            this.selecting = false;
                                        }
                                    }
                                    _ => {}
                                }
                                cx.notify();
                            },
                        )),
                    )
                    .child(Self::button("Cancel", "cancel-close").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.closing = None;
                            cx.notify();
                        },
                    ))),
            );
        }
        if self.form {
            let labels = [
                "Name",
                "Host / SSH alias",
                "User (optional)",
                "Port (optional)",
                "Identity file (optional)",
                "Jump host (optional)",
            ];
            let mut form = div().flex().flex_col().gap_2().p_3().bg(rgb(0x292a36));
            for row in 0..2 {
                let mut fields = div().flex().gap_3();
                for (i, label) in labels.iter().enumerate().skip(row * 3).take(3) {
                    let focused = self.field == Some(i);
                    fields = fields.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(*label)
                            .child(
                                div()
                                    .id(("field", i))
                                    .px_2()
                                    .py_2()
                                    .h(px(36.))
                                    .overflow_hidden()
                                    .border_1()
                                    .border_color(rgb(if focused { 0xb59ae0 } else { 0x454655 }))
                                    .bg(rgb(if focused && self.field_selection {
                                        0x494167
                                    } else {
                                        0x1c1e26
                                    }))
                                    .child(format!(
                                        "{}{}{}",
                                        self.fields[i],
                                        if focused { &self.composition } else { "" },
                                        if focused { "|" } else { "" }
                                    ))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            this.field = Some(i);
                                            this.field_selection = false;
                                            this.composition.clear();
                                            window.focus(&this.focus);
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    );
                }
                form = form.child(fields);
            }
            root =
                root.child(
                    form.child(
                        div()
                            .flex()
                            .gap_2()
                            .child(Self::button("Connect once", "connect").on_click(
                                cx.listener(|this, _, _, cx| this.connect_form(false, cx)),
                            ))
                            .child(Self::button("Save and connect", "save-connect").on_click(
                                cx.listener(|this, _, _, cx| this.connect_form(true, cx)),
                            ))
                            .child(Self::button("Cancel", "cancel-form").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.form = false;
                                    this.field = None;
                                    cx.notify();
                                },
                            ))),
                    ),
                );
        }
        let mut sidebar = div()
            .w(px(190.))
            .flex_shrink_0()
            .p_3()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgb(0x22232d))
            .child(div().text_color(rgb(0xb59ae0)).child("CONNECTIONS"))
            .child("Real local PTY / OpenSSH");
        for (i, connection) in self.settings.connections.iter().enumerate() {
            let connection = connection.clone();
            let label = connection.name.clone();
            sidebar = sidebar.child(Self::button(label, ("saved", i)).on_click(
                cx.listener(move |this, _, _, cx| this.open_ssh(connection.clone(), false, cx)),
            ));
        }
        sidebar = sidebar
            .child(div().flex_1())
            .child("Ctrl+Shift+T  New local")
            .child("Ctrl+Tab  Switch tab")
            .child("Drag  Select text")
            .child("Ctrl+Shift+C / V")
            .child("AI: use nosh in the terminal. No remote/local context sharing.");
        let mut tabs = div()
            .flex()
            .gap_1()
            .p_2()
            .bg(rgb(0x252631))
            .overflow_hidden();
        for (i, tab) in self.tabs.iter().enumerate() {
            tabs = tabs.child(
                Self::button(tab.title.clone(), ("tab", tab.id))
                    .bg(rgb(if self.active == i { 0x514561 } else { 0x343543 }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active = i;
                        this.field = None;
                        this.composition.clear();
                        this.selecting = false;
                        cx.notify();
                    })),
            );
        }
        let target = self
            .tabs
            .get(self.active)
            .map(|t| t.target.clone())
            .unwrap_or_else(|| "No terminal. Open Local or SSH.".into());
        let status = self
            .tabs
            .get(self.active)
            .map(|t| t.status.clone())
            .unwrap_or_default();
        let terminal = div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .id("terminal")
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
                if let Some(tab) = this.tabs.get(this.active) {
                    let lines = event.delta.pixel_delta(this.cell.height).y / this.cell.height;
                    tab.backend
                        .model()
                        .lock()
                        .scroll_display(Scroll::Delta(lines.round() as i32));
                    cx.notify();
                }
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        view.update(cx, |this, cx| this.paint_terminal(bounds, window, cx))
                    },
                    move |bounds, (), window, cx| {
                        let this = paint_view.read(cx);
                        window.handle_input(
                            &this.focus,
                            ElementInputHandler::new(bounds, input_view),
                            cx,
                        );
                        paint_view.update(cx, |this, cx| this.draw(bounds, window, cx));
                    },
                )
                .size_full(),
            );
        root.child(
            div().flex().flex_1().min_h_0().child(sidebar).child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(tabs)
                    .child(div().px_3().py_2().child(target))
                    .child(terminal)
                    .child(div().p_2().bg(rgb(0x252631)).child(status)),
            ),
        )
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
        let field = &self.fields[self.field?];
        let text: Vec<u16> = field.encode_utf16().collect();
        let range = range.start.min(text.len())..range.end.min(text.len());
        *actual = Some(range.clone());
        Some(String::from_utf16_lossy(&text[range]))
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let len = self
            .field
            .map(|i| self.fields[i].encode_utf16().count())
            .unwrap_or(0);
        Some(UTF16Selection {
            range: if self.field_selection {
                0..len
            } else {
                len..len
            },
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        if self.composition.is_empty() {
            None
        } else {
            Some(0..self.composition.encode_utf16().count())
        }
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.composition.clear();
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composition.clear();
        self.insert(text);
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composition = text.to_owned();
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(self.bounds)
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_keys_preserve_interrupt_application_cursor_and_unicode_input_path() {
        let key = |key: &str| KeyDownEvent {
            keystroke: gpui::Keystroke::parse(key).unwrap(),
            is_held: false,
        };
        assert_eq!(
            terminal_key(&key("ctrl-c"), TermMode::empty()).unwrap(),
            [3]
        );
        assert_eq!(
            terminal_key(&key("up"), TermMode::APP_CURSOR).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            terminal_key(&key("shift-up"), TermMode::empty()).unwrap(),
            b"\x1b[1;2A"
        );
        assert_eq!(
            terminal_key(&key("enter"), TermMode::empty()).unwrap(),
            b"\r"
        );
        assert!(
            terminal_key(&key("a"), TermMode::empty()).is_none(),
            "ordinary text must be delivered once by the native input handler"
        );
    }
}
