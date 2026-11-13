use std::io;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use alacritty_terminal::event::{EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{
    self, Term, TermMode, cell::Flags, test::TermSize, viewport_to_point,
};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use eframe::egui::{self, Color32, FontId, Key, Rect, Stroke, Vec2};

pub use alacritty_terminal::event::Event as PtyEvent;

#[derive(Default)]
pub struct BackendSettings {
    pub shell: String,
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Clone)]
pub struct EventProxy {
    id: u64,
    sender: Sender<(u64, PtyEvent)>,
    wake: Arc<dyn Fn() + Send + Sync>,
    channel: Arc<OnceLock<EventLoopSender>>,
    size: Arc<FairMutex<WindowSize>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: PtyEvent) {
        if let Some(bytes) = TerminalBackend::protocol_response(&event, *self.size.lock()) {
            let result = self
                .channel
                .get()
                .ok_or_else(|| "PTY channel is not initialized".to_owned())
                .and_then(|channel| {
                    channel
                        .send(Msg::Input(bytes.into()))
                        .map_err(|error| error.to_string())
                });
            match result {
                Ok(()) => return,
                Err(error) => eprintln!(
                    "nosh-desktop: terminal {} protocol response: {error}",
                    self.id
                ),
            }
        }
        match event {
            PtyEvent::Wakeup | PtyEvent::MouseCursorDirty | PtyEvent::CursorBlinkingChange => {}
            PtyEvent::ClipboardLoad(..) | PtyEvent::ClipboardStore(..) | PtyEvent::Bell => return,
            event => {
                if self.sender.send((self.id, event)).is_err() {
                    eprintln!("nosh-desktop: terminal {} event receiver closed", self.id);
                    return;
                }
            }
        }
        (self.wake)();
    }
}

type PtyWorker = JoinHandle<(EventLoop<tty::Pty, EventProxy>, State)>;

pub struct TerminalBackend {
    id: u64,
    term: Arc<FairMutex<Term<EventProxy>>>,
    channel: EventLoopSender,
    worker: Option<PtyWorker>,
    size: WindowSize,
    reply_size: Arc<FairMutex<WindowSize>>,
}

impl TerminalBackend {
    pub fn new(
        id: u64,
        context: egui::Context,
        sender: Sender<(u64, PtyEvent)>,
        settings: BackendSettings,
    ) -> io::Result<Self> {
        Self::with_wake(
            id,
            Arc::new(move || context.request_repaint()),
            sender,
            settings,
        )
    }

    pub fn with_wake(
        id: u64,
        wake: Arc<dyn Fn() + Send + Sync>,
        sender: Sender<(u64, PtyEvent)>,
        settings: BackendSettings,
    ) -> io::Result<Self> {
        let size = WindowSize {
            num_lines: 24,
            num_cols: 80,
            cell_width: 8,
            cell_height: 18,
        };
        let reply_size = Arc::new(FairMutex::new(size));
        let reply_channel = Arc::new(OnceLock::new());
        let proxy = EventProxy {
            id,
            sender,
            wake,
            channel: reply_channel.clone(),
            size: reply_size.clone(),
        };
        // Alacritty's Windows backend joins its command line verbatim.
        #[cfg(windows)]
        let settings = BackendSettings {
            shell: quote_windows(&settings.shell),
            args: settings.args.iter().map(|arg| quote_windows(arg)).collect(),
            ..settings
        };
        let options = tty::Options {
            shell: Some(tty::Shell::new(settings.shell, settings.args)),
            working_directory: settings.working_directory,
            env: settings.env,
            ..Default::default()
        };
        let pty = tty::new(&options, size, id)?;
        let term = Arc::new(FairMutex::new(Term::new(
            term::Config {
                scrolling_history: 5000,
                ..Default::default()
            },
            &TermSize::new(80, 24),
            proxy.clone(),
        )));
        let event_loop = EventLoop::new(term.clone(), proxy, pty, true, false)?;
        let channel = event_loop.channel();
        reply_channel
            .set(channel.clone())
            .map_err(|_| io::Error::other("PTY channel initialized twice"))?;
        let worker = Some(event_loop.spawn());
        Ok(Self {
            id,
            term,
            channel,
            worker,
            size,
            reply_size,
        })
    }

    pub fn write(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.channel
            .send(Msg::Input(bytes.into()))
            .map_err(io::Error::other)
    }

    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> io::Result<()> {
        let size = WindowSize {
            num_lines: rows.max(2),
            num_cols: cols.max(2),
            cell_width,
            cell_height,
        };
        if self.size.num_cols != size.num_cols
            || self.size.num_lines != size.num_lines
            || self.size.cell_width != cell_width
            || self.size.cell_height != cell_height
        {
            if self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
            {
                self.channel
                    .send(Msg::Resize(size))
                    .map_err(io::Error::other)?;
            }
            self.term.lock().resize(TermSize::new(
                size.num_cols as usize,
                size.num_lines as usize,
            ));
            self.size = size;
            *self.reply_size.lock() = size;
        }
        Ok(())
    }

    pub fn reply(&self, event: PtyEvent) -> io::Result<()> {
        Self::protocol_response(&event, self.size).map_or(Ok(()), |bytes| self.write(bytes))
    }

    #[cfg(feature = "gpui")]
    pub fn model(&self) -> &Arc<FairMutex<Term<EventProxy>>> {
        &self.term
    }

    fn protocol_response(event: &PtyEvent, size: WindowSize) -> Option<Vec<u8>> {
        match event {
            PtyEvent::PtyWrite(text) => Some(text.as_bytes().to_vec()),
            PtyEvent::TextAreaSizeRequest(formatter) => Some(formatter(size).into_bytes()),
            PtyEvent::ColorRequest(index, formatter) => {
                let color = palette(*index);
                Some(
                    formatter(Rgb {
                        r: color.r(),
                        g: color.g(),
                        b: color.b(),
                    })
                    .into_bytes(),
                )
            }
            _ => None,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, font_size: f32, focused: bool) -> io::Result<()> {
        let font = FontId::monospace(font_size);
        let cell = ui.fonts(|fonts| {
            Vec2::new(
                fonts.glyph_width(&font, 'M').ceil(),
                fonts.row_height(&font).ceil(),
            )
        });
        let size = ui.available_size().max(Vec2::splat(1.0));
        self.resize(
            (size.x / cell.x) as u16,
            (size.y / cell.y) as u16,
            cell.x as u16,
            cell.y as u16,
        )?;
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        if focused {
            response.request_focus();
        } else {
            response.surrender_focus();
        }
        if focused {
            let cursor = self.term.lock().grid().cursor.point;
            let cursor_pos = rect.min
                + egui::vec2(
                    cursor.column.0 as f32 * cell.x,
                    cursor.line.0 as f32 * cell.y,
                );
            ui.output_mut(|output| {
                output.ime = Some(egui::output::IMEOutput {
                    rect,
                    cursor_rect: Rect::from_min_size(cursor_pos, cell),
                });
            });
            let events = ui.input(|input| input.events.clone());
            let mode = *self.term.lock().mode();
            for event in &events {
                if let Some(bytes) = keyboard_input(event, mode, ui.input(|input| input.modifiers))
                {
                    self.write(bytes)?;
                    self.term.lock().scroll_display(Scroll::Bottom);
                }
            }
            let copy = ui.input(|input| {
                input.modifiers.shift || (cfg!(target_os = "macos") && input.modifiers.mac_cmd)
            });
            if copy
                && events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Copy))
                && let Some(text) = self.term.lock().selection_to_string()
            {
                ui.ctx().copy_text(text);
            }
        }
        let shift = ui.input(|input| input.modifiers.shift);
        let mouse_mode = self.term.lock().mode().intersects(TermMode::MOUSE_MODE);
        if response.hovered() {
            let delta = ui.input(|input| input.smooth_scroll_delta.y);
            if delta.abs() >= 1.0 && (!mouse_mode || shift) {
                self.term
                    .lock()
                    .scroll_display(Scroll::Delta((delta / cell.y).round() as i32));
            }
        }
        if let Some(pos) = response.interact_pointer_pos() {
            let x = ((pos.x - rect.min.x) / cell.x).floor().max(0.0) as usize;
            let y = ((pos.y - rect.min.y) / cell.y).floor().max(0.0) as usize;
            let mut term = self.term.lock();
            let point = viewport_to_point(
                term.grid().display_offset(),
                Point::new(
                    y.min(self.size.num_lines as usize - 1),
                    Column(x.min(self.size.num_cols as usize - 1)),
                ),
            );
            if !mouse_mode || shift {
                if response.drag_started() || response.clicked() {
                    term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
                }
                if response.dragged()
                    && let Some(selection) = &mut term.selection
                {
                    selection.update(point, Side::Right);
                }
            } else if focused && term.mode().contains(TermMode::SGR_MOUSE) {
                let press =
                    ui.input(|input| input.pointer.button_pressed(egui::PointerButton::Primary));
                let release =
                    ui.input(|input| input.pointer.button_released(egui::PointerButton::Primary));
                if press || release {
                    self.write(
                        format!(
                            "\x1b[<0;{};{}{}",
                            x + 1,
                            y + 1,
                            if press { 'M' } else { 'm' }
                        )
                        .into_bytes(),
                    )?;
                }
            }
        }
        self.paint(ui, rect, cell, &font, focused);
        Ok(())
    }

    fn paint(&self, ui: &egui::Ui, rect: Rect, cell_size: Vec2, font: &FontId, focused: bool) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0, palette(257));
        let term = self.term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let selection = content.selection;
        let cursor = content.cursor;
        let colors = content.colors;
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 {
                continue;
            }
            let origin = rect.min
                + egui::vec2(
                    indexed.point.column.0 as f32 * cell_size.x,
                    row as f32 * cell_size.y,
                );
            let bounds = Rect::from_min_size(origin, cell_size);
            let mut fg = terminal_color(indexed.fg, colors);
            let mut bg = terminal_color(indexed.bg, colors);
            if indexed.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if selection.is_some_and(|selection| selection.contains(indexed.point)) {
                bg = Color32::from_rgb(73, 65, 103);
            }
            painter.rect_filled(bounds, 0, bg);
            if !indexed.flags.intersects(
                Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER | Flags::HIDDEN,
            ) {
                let mut text = indexed.c.to_string();
                if let Some(zero_width) = indexed.zerowidth() {
                    text.extend(zero_width);
                }
                if indexed.flags.contains(Flags::DIM) {
                    fg = fg.gamma_multiply(0.65);
                }
                painter.text(origin, egui::Align2::LEFT_TOP, text, font.clone(), fg);
                if indexed.flags.contains(Flags::UNDERLINE) {
                    painter.line_segment(
                        [bounds.left_bottom(), bounds.right_bottom()],
                        Stroke::new(1.0_f32, fg),
                    );
                }
            }
        }
        if cursor.shape != alacritty_terminal::vte::ansi::CursorShape::Hidden {
            let row = cursor.point.line.0 + offset;
            if row >= 0 {
                let pos = rect.min
                    + egui::vec2(
                        cursor.point.column.0 as f32 * cell_size.x,
                        row as f32 * cell_size.y,
                    );
                painter.rect_stroke(
                    Rect::from_min_size(pos, cell_size),
                    0,
                    Stroke::new(if focused { 1.5_f32 } else { 0.5_f32 }, palette(256)),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }

    #[cfg(test)]
    pub fn screen_text(&self) -> String {
        self.term
            .lock()
            .grid()
            .display_iter()
            .map(|cell| cell.c)
            .collect()
    }
}

impl Drop for TerminalBackend {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            if !worker.is_finished()
                && let Err(error) = self.channel.send(Msg::Shutdown)
            {
                eprintln!("nosh-desktop: terminal {} shutdown: {error}", self.id);
            }
            if worker.join().is_err() {
                eprintln!("nosh-desktop: terminal {} I/O worker panicked", self.id);
            }
        }
    }
}

pub fn keyboard_input(
    event: &egui::Event,
    mode: TermMode,
    modifiers: egui::Modifiers,
) -> Option<Vec<u8>> {
    match event {
        egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
            let prefix = if modifiers.alt && !modifiers.ctrl {
                "\x1b"
            } else {
                ""
            };
            Some(format!("{prefix}{text}").into_bytes())
        }
        egui::Event::Paste(text) => {
            let text = text.replace('\x1b', "");
            Some(
                if mode.contains(TermMode::BRACKETED_PASTE) {
                    format!("\x1b[200~{text}\x1b[201~")
                } else {
                    text.replace("\r\n", "\r").replace('\n', "\r")
                }
                .into_bytes(),
            )
        }
        egui::Event::Copy if !modifiers.shift && !cfg!(target_os = "macos") => Some(vec![3]),
        egui::Event::Cut if !cfg!(target_os = "macos") => Some(vec![24]),
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            if modifiers.ctrl && !modifiers.alt && !modifiers.shift {
                let name = key.name();
                if name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic() {
                    return Some(vec![name.as_bytes()[0].to_ascii_uppercase() & 0x1f]);
                }
            }
            let bytes = match key {
                Key::Enter => "\r",
                Key::Backspace if modifiers.ctrl => "\x17",
                Key::Backspace => "\x7f",
                Key::Tab if modifiers.shift => "\x1b[Z",
                Key::Tab => "\t",
                Key::Escape => "\x1b",
                Key::ArrowUp
                | Key::ArrowDown
                | Key::ArrowLeft
                | Key::ArrowRight
                | Key::Home
                | Key::End => {
                    let code = match key {
                        Key::ArrowUp => 'A',
                        Key::ArrowDown => 'B',
                        Key::ArrowRight => 'C',
                        Key::ArrowLeft => 'D',
                        Key::Home => 'H',
                        _ => 'F',
                    };
                    let modifier = 1
                        + u8::from(modifiers.shift)
                        + 2 * u8::from(modifiers.alt)
                        + 4 * u8::from(modifiers.ctrl);
                    return Some(
                        if modifier > 1 {
                            format!("\x1b[1;{modifier}{code}")
                        } else if mode.contains(TermMode::APP_CURSOR) {
                            format!("\x1bO{code}")
                        } else {
                            format!("\x1b[{code}")
                        }
                        .into_bytes(),
                    );
                }
                Key::Insert => "\x1b[2~",
                Key::Delete => "\x1b[3~",
                Key::PageUp => "\x1b[5~",
                Key::PageDown => "\x1b[6~",
                Key::F1 => "\x1bOP",
                Key::F2 => "\x1bOQ",
                Key::F3 => "\x1bOR",
                Key::F4 => "\x1bOS",
                Key::F5 => "\x1b[15~",
                Key::F6 => "\x1b[17~",
                Key::F7 => "\x1b[18~",
                Key::F8 => "\x1b[19~",
                Key::F9 => "\x1b[20~",
                Key::F10 => "\x1b[21~",
                Key::F11 => "\x1b[23~",
                Key::F12 => "\x1b[24~",
                _ => return None,
            };
            Some(bytes.as_bytes().to_vec())
        }
        _ => None,
    }
}

pub fn terminal_color(color: Color, colors: &alacritty_terminal::term::color::Colors) -> Color32 {
    let index = match color {
        Color::Spec(rgb) => return Color32::from_rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => index as usize,
        Color::Named(name) => name as usize,
    };
    if let Some(rgb) = colors[index] {
        Color32::from_rgb(rgb.r, rgb.g, rgb.b)
    } else {
        palette(index)
    }
}

fn palette(index: usize) -> Color32 {
    const ANSI: [[u8; 3]; 16] = [
        [32, 34, 42],
        [224, 141, 137],
        [133, 190, 156],
        [218, 191, 131],
        [142, 171, 224],
        [181, 154, 224],
        [129, 188, 195],
        [210, 212, 224],
        [104, 110, 132],
        [242, 165, 160],
        [155, 213, 176],
        [235, 208, 154],
        [162, 191, 245],
        [202, 177, 243],
        [152, 214, 219],
        [240, 240, 247],
    ];
    let [r, g, b] = match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let index = index - 16;
            let component = |value| {
                if value == 0 {
                    0
                } else {
                    (55 + value * 40) as u8
                }
            };
            [
                component(index / 36),
                component(index / 6 % 6),
                component(index % 6),
            ]
        }
        232..=255 => [8 + (index as u8 - 232) * 10; 3],
        index if index == NamedColor::Background as usize => [28, 30, 38],
        _ => [218, 220, 232],
    };
    Color32::from_rgb(r, g, b)
}

#[cfg(windows)]
fn quote_windows(value: &str) -> String {
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        quoted.extend(std::iter::repeat_n(
            '\\',
            if ch == '"' { slashes * 2 + 1 } else { slashes },
        ));
        quoted.push(ch);
        slashes = 0;
    }
    quoted.extend(std::iter::repeat_n('\\', slashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    fn wait_for(terminal: &TerminalBackend, _events: &Receiver<(u64, PtyEvent)>, needle: &str) {
        // Do not pump UI events: terminal queries must also work in background tabs.
        let start = Instant::now();
        loop {
            if terminal.screen_text().contains(needle) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "missing {needle:?}; screen:\n{}",
                terminal.screen_text()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn native_pty_roundtrip_resize_and_real_exit_status() {
        let (sender, events) = mpsc::channel();
        let settings = BackendSettings {
            shell: if cfg!(windows) {
                "cmd.exe".into()
            } else {
                "/bin/sh".into()
            },
            args: if cfg!(windows) {
                vec!["/d".into(), "/q".into()]
            } else {
                vec!["-i".into()]
            },
            working_directory: None,
            ..Default::default()
        };
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let notified = wakes.clone();
        let mut terminal = TerminalBackend::with_wake(
            1,
            Arc::new(move || {
                notified.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }),
            sender,
            settings,
        )
        .unwrap();
        terminal.resize(100, 30, 8, 18).unwrap();
        let command = if cfg!(windows) {
            "@echo off\rset FIRST=DESKTOP_\recho %FIRST%ROUNDTRIP\r"
        } else {
            "printf 'DESKTOP_%s\\n' ROUNDTRIP\r"
        };
        terminal.write(command.as_bytes().to_vec()).unwrap();
        wait_for(&terminal, &events, "DESKTOP_ROUNDTRIP");
        assert!(wakes.load(std::sync::atomic::Ordering::Relaxed) > 0);
        terminal.write(b"exit 7\r".to_vec()).unwrap();
        let start = Instant::now();
        loop {
            if let Ok((_, event)) = events.recv_timeout(Duration::from_millis(100)) {
                if matches!(event, PtyEvent::ChildExit(7)) {
                    break;
                }
                terminal.reply(event).unwrap();
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "child exit status not delivered"
            );
        }
        let start = Instant::now();
        while !terminal.worker.as_ref().unwrap().is_finished() {
            assert!(start.elapsed() < Duration::from_secs(3), "PTY did not stop");
            std::thread::sleep(Duration::from_millis(10));
        }
        terminal.resize(120, 35, 9, 20).unwrap();
        let start = Instant::now();
        drop(terminal);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "PTY cleanup blocked"
        );
    }

    #[test]
    fn ssh_failure_is_real_visible_and_exits_nonzero() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let connection = crate::connections::Connection {
            name: "closed-loopback".into(),
            host: "127.0.0.1".into(),
            port: Some(port),
            ..Default::default()
        };
        let mut settings = crate::connections::ssh_backend(&connection).unwrap();
        settings.args.splice(
            0..0,
            [
                "-F".into(),
                "none".into(),
                "-o".into(),
                "ConnectTimeout=2".into(),
            ],
        );
        let (sender, events) = mpsc::channel();
        let terminal = TerminalBackend::new(2, egui::Context::default(), sender, settings).unwrap();
        let start = Instant::now();
        let mut exit = None;
        while start.elapsed() < Duration::from_secs(10) {
            if let Ok((_, event)) = events.recv_timeout(Duration::from_millis(100)) {
                if let PtyEvent::ChildExit(code) = event {
                    exit = Some(code);
                    break;
                }
                terminal.reply(event).unwrap();
            }
        }
        assert_eq!(
            exit,
            Some(255),
            "SSH must report failure; screen: {}",
            terminal.screen_text()
        );
        assert!(
            terminal.screen_text().contains("ssh:"),
            "SSH diagnostic missing"
        );
    }

    #[test]
    #[ignore = "requires a built nosh CLI; set NOSH_TEST_BINARY and run with --ignored"]
    fn real_nosh_keeps_shell_state_and_answers_terminal_queries() {
        let path = std::env::var_os("NOSH_TEST_BINARY")
            .expect("set NOSH_TEST_BINARY to the built nosh executable");
        let dir = tempfile::tempdir().unwrap();
        let (sender, events) = mpsc::channel();
        let settings = BackendSettings {
            shell: PathBuf::from(path).display().to_string(),
            args: vec!["--safe".into(), "-i".into()],
            working_directory: Some(dir.path().to_owned()),
            env: [("PS1".into(), "DESKTOP> ".into())].into_iter().collect(),
        };
        let terminal = TerminalBackend::new(3, egui::Context::default(), sender, settings).unwrap();
        wait_for(&terminal, &events, ">");
        terminal
            .write(b"DESKTOP_VAR=kept\rprintf 'STATE_%s\\n' \"$DESKTOP_VAR\"\r".to_vec())
            .unwrap();
        wait_for(&terminal, &events, "STATE_kept");
        terminal
            .write(b"printf '\\033[31mANSI_%s\\033[0m\\n' works\r".to_vec())
            .unwrap();
        wait_for(&terminal, &events, "ANSI_works");
    }

    #[test]
    #[cfg(windows)]
    fn closing_a_terminal_reclaims_its_console_child() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        let settings = BackendSettings {
            shell: "powershell.exe".into(),
            args: vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-Command".into(),
                "Write-Output ('CHILD:' + $PID); Start-Sleep -Seconds 30".into(),
            ],
            working_directory: None,
            ..Default::default()
        };
        let (sender, events) = mpsc::channel();
        let terminal = TerminalBackend::new(4, egui::Context::default(), sender, settings).unwrap();
        wait_for(&terminal, &events, "CHILD:");
        let screen = terminal.screen_text();
        let pid: u32 = screen
            .split("CHILD:")
            .nth(1)
            .unwrap()
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap();
        // SAFETY: the PID is returned by our own child; this handle grants only waiting.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(!handle.is_null());
        let start = Instant::now();
        drop(terminal);
        let result = unsafe { WaitForSingleObject(handle, 3000) };
        unsafe {
            CloseHandle(handle);
        }
        assert_eq!(result, WAIT_OBJECT_0, "terminal child survived PTY close");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn terminal_keys_preserve_interrupt_cursor_modes_and_bracketed_paste() {
        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let plain = egui::Modifiers::NONE;
        assert_eq!(
            keyboard_input(
                &key(Key::C, egui::Modifiers::CTRL),
                TermMode::empty(),
                plain
            )
            .unwrap(),
            [3]
        );
        assert_eq!(
            keyboard_input(&key(Key::ArrowUp, plain), TermMode::APP_CURSOR, plain).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            keyboard_input(&key(Key::ArrowUp, plain), TermMode::empty(), plain).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            keyboard_input(
                &egui::Event::Paste("echo one\necho two".into()),
                TermMode::BRACKETED_PASTE,
                plain
            )
            .unwrap(),
            b"\x1b[200~echo one\necho two\x1b[201~"
        );
        assert_eq!(
            keyboard_input(&egui::Event::Text("中文".into()), TermMode::empty(), plain).unwrap(),
            "中文".as_bytes()
        );
    }

    #[test]
    #[cfg(windows)]
    fn quotes_windows_programs_spaces_quotes_and_trailing_backslashes() {
        assert_eq!(
            quote_windows(r"C:\Program Files\nosh.exe"),
            r#""C:\Program Files\nosh.exe""#
        );
        assert_eq!(quote_windows(r#"a"b\"#), r#""a\"b\\""#);
        assert_eq!(quote_windows(""), "\"\"");
    }
}
