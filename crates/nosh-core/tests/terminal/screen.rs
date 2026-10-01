//! Bounded screen observations for real PTY redraws, including background state.

use super::support::style;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
struct Cell {
    text: String,
    background: Option<Color>,
    painted: bool,
}

#[derive(Debug)]
pub(super) struct Frame {
    pub columns: usize,
    pub cursor: (usize, usize),
    pub resizing: bool,
    pub lines: Vec<String>,
    pub painted: Vec<Vec<bool>>,
    pub backgrounds: Vec<Vec<Option<Color>>>,
}

#[derive(Debug)]
pub(super) struct Printed {
    pub character: char,
    pub background: Option<Color>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Color {
    Ansi(u16),
    Indexed(u16),
    Rgb([u16; 3]),
}

pub(super) struct Screen {
    cells: Vec<Vec<Cell>>,
    wrapped: Vec<bool>,
    row: usize,
    column: usize,
    columns: usize,
    saved: (usize, usize),
    background: Option<Color>,
    replies: Arc<Mutex<Vec<Vec<u8>>>>,
    acknowledged_columns: Arc<AtomicUsize>,
    pub frames: Vec<Frame>,
    pub printed: Vec<Printed>,
}

impl Screen {
    pub fn new(
        columns: usize,
        replies: Arc<Mutex<Vec<Vec<u8>>>>,
        acknowledged_columns: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            cells: vec![vec![Cell::default(); columns]; 24],
            wrapped: vec![false; 24],
            row: 0,
            column: 0,
            columns,
            saved: (0, 0),
            background: None,
            replies,
            acknowledged_columns,
            frames: Vec::new(),
            printed: Vec::new(),
        }
    }

    pub fn resize(&mut self, columns: usize) {
        let last = self
            .cells
            .iter()
            .rposition(|row| row.iter().any(|cell| cell.painted))
            .unwrap_or(0)
            .max(self.row)
            .max(self.saved.0);
        let mut rows = Vec::new();
        let mut wrapped = Vec::new();
        let mut cursor = None;
        let mut saved = None;
        let mut source_row = 0;
        while source_row <= last {
            let mut paragraph = Vec::new();
            let mut cursor_offset = None;
            let mut saved_offset = None;
            loop {
                let row = &self.cells[source_row];
                let length = if self.wrapped[source_row] {
                    self.columns
                } else {
                    row.iter()
                        .rposition(|cell| cell.painted)
                        .map_or(0, |last| last + 1)
                        .max(if source_row == self.row {
                            self.column
                        } else {
                            0
                        })
                        .max(if source_row == self.saved.0 {
                            self.saved.1
                        } else {
                            0
                        })
                };
                if source_row == self.row {
                    cursor_offset = Some(paragraph.len() + self.column);
                }
                if source_row == self.saved.0 {
                    saved_offset = Some(paragraph.len() + self.saved.1);
                }
                paragraph.extend_from_slice(&row[..length]);
                let continues = self.wrapped[source_row];
                source_row += 1;
                if !continues || source_row > last {
                    break;
                }
            }
            rows.push(vec![Cell::default(); columns]);
            wrapped.push(false);
            let mut column = 0;
            let mut index = 0;
            let mut positions = vec![(0, 0); paragraph.len() + 1];
            while index < paragraph.len() {
                let span = style::width(&paragraph[index].text).max(1);
                if column + span > columns {
                    *wrapped.last_mut().unwrap() = true;
                    rows.push(vec![Cell::default(); columns]);
                    wrapped.push(false);
                    column = 0;
                }
                let target_row = rows.len() - 1;
                for offset in 0..span {
                    positions[index + offset] = (target_row, column + offset);
                    rows[target_row][column + offset] = paragraph[index + offset].clone();
                }
                column += span;
                index += span;
            }
            positions[paragraph.len()] = (rows.len() - 1, column);
            if let Some(offset) = cursor_offset {
                cursor = Some(positions[offset]);
            }
            if let Some(offset) = saved_offset {
                saved = Some(positions[offset]);
            }
        }
        let scrolled = rows.len().saturating_sub(24);
        rows.drain(..scrolled);
        wrapped.drain(..scrolled);
        rows.resize_with(24, || vec![Cell::default(); columns]);
        wrapped.resize(24, false);
        let shift = |(row, column): (usize, usize)| (row.saturating_sub(scrolled), column);
        (self.row, self.column) = shift(cursor.expect("cursor paragraph exists"));
        self.saved = shift(saved.expect("saved cursor paragraph exists"));
        self.cells = rows;
        self.wrapped = wrapped;
        self.columns = columns;
    }

    fn newline(&mut self) {
        if self.row + 1 == self.cells.len() {
            self.cells.remove(0);
            self.cells.push(vec![Cell::default(); self.columns]);
            self.wrapped.remove(0);
            self.wrapped.push(false);
        } else {
            self.row += 1;
        }
    }

    fn frame(&mut self) {
        assert!(self.frames.len() < 4096, "screen observation limit");
        if self.row > 0
            && self.columns > 1
            && self.cells[self.row - 1][..self.columns - 1]
                .iter()
                .all(|cell| cell.painted)
            && !self.cells[self.row - 1][self.columns - 1].painted
        {
            self.acknowledged_columns
                .store(self.columns, Ordering::Release);
        }
        self.frames.push(Frame {
            columns: self.columns,
            cursor: (self.row, self.column),
            resizing: self.acknowledged_columns.load(Ordering::Acquire) != self.columns,
            lines: self
                .cells
                .iter()
                .map(|row| row.iter().map(|cell| cell.text.as_str()).collect())
                .collect(),
            painted: self
                .cells
                .iter()
                .map(|row| row.iter().map(|cell| cell.painted).collect())
                .collect(),
            backgrounds: self
                .cells
                .iter()
                .map(|row| row.iter().map(|cell| cell.background).collect())
                .collect(),
        });
    }
}

impl vte::Perform for Screen {
    fn print(&mut self, character: char) {
        assert!(self.printed.len() < 1024 * 1024, "screen output limit");
        self.printed.push(Printed {
            character,
            background: self.background,
        });
        let width = style::width(&character.to_string());
        if width == 0 {
            if self.column > 0 {
                self.cells[self.row][self.column.saturating_sub(1)]
                    .text
                    .push(character);
            }
            return;
        }
        if self.column + width > self.columns {
            self.wrapped[self.row] = true;
            self.column = 0;
            self.newline();
        }
        for offset in 0..width {
            self.cells[self.row][self.column + offset] = Cell {
                text: if offset == 0 {
                    character.to_string()
                } else {
                    String::new()
                },
                background: self.background,
                painted: true,
            };
        }
        self.column += width;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.column = 0,
            b'\n' => {
                self.wrapped[self.row] = false;
                self.newline();
            }
            b'\x08' => self.column = self.column.saturating_sub(1),
            b'\t' => self.column = ((self.column / 8 + 1) * 8).min(self.columns.saturating_sub(1)),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _: &[u8], _: bool, byte: u8) {
        match byte {
            b'7' => self.saved = (self.row, self.column),
            b'8' => (self.row, self.column) = self.saved,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _: bool, action: char) {
        let values: Vec<_> = params.iter().map(|value| value[0]).collect();
        let first = values.first().copied().unwrap_or(0);
        let distance = usize::from(first.max(1));
        match action {
            'm' => {
                let mut values = values.into_iter();
                while let Some(value) = values.next() {
                    match value {
                        0 | 49 => self.background = None,
                        40..=47 | 100..=107 => self.background = Some(Color::Ansi(value)),
                        38 | 48 => {
                            let extended = match values.next() {
                                Some(5) => values.next().map(Color::Indexed),
                                Some(2) => {
                                    let rgb: Vec<_> = values.by_ref().take(3).collect();
                                    (rgb.len() == 3).then(|| Color::Rgb([rgb[0], rgb[1], rgb[2]]))
                                }
                                _ => None,
                            };
                            if value == 48 {
                                self.background = extended;
                            }
                        }
                        _ => {}
                    }
                }
            }
            'H' | 'f' => {
                self.row = (distance - 1).min(self.cells.len() - 1);
                self.column = usize::from(values.get(1).copied().unwrap_or(1).max(1) - 1)
                    .min(self.columns.saturating_sub(1));
            }
            'A' => self.row = self.row.saturating_sub(distance),
            'B' => self.row = (self.row + distance).min(self.cells.len() - 1),
            'C' => self.column = (self.column + distance).min(self.columns.saturating_sub(1)),
            'D' => self.column = self.column.saturating_sub(distance),
            'G' | '`' => self.column = (distance - 1).min(self.columns.saturating_sub(1)),
            'd' => self.row = (distance - 1).min(self.cells.len() - 1),
            'J' => {
                if matches!(first, 2 | 3) {
                    for row in &mut self.cells {
                        row.fill(Cell::default());
                    }
                    self.wrapped.fill(false);
                } else if first == 0 {
                    self.cells[self.row][self.column.min(self.columns)..].fill(Cell::default());
                    for row in &mut self.cells[self.row + 1..] {
                        row.fill(Cell::default());
                    }
                    self.wrapped[self.row..].fill(false);
                }
            }
            'K' => {
                let start = if matches!(first, 1 | 2) {
                    0
                } else {
                    self.column.min(self.columns)
                };
                let end = if first == 1 {
                    (self.column + 1).min(self.columns)
                } else {
                    self.columns
                };
                self.cells[self.row][start..end].fill(Cell::default());
                self.wrapped[self.row] = false;
            }
            's' => self.saved = (self.row, self.column),
            'u' => (self.row, self.column) = self.saved,
            'n' if first == 6 => self.replies.lock().unwrap().push(
                format!(
                    "\x1b[{};{}R",
                    self.row + 1,
                    (self.column + 1).min(self.columns)
                )
                .into_bytes(),
            ),
            'h' if first == 25 && intermediates == b"?" => self.frame(),
            _ => {}
        }
    }
}

#[test]
fn screen_resize_reflows_colored_spaces_without_turning_hard_newlines_into_soft_wraps() {
    let replies = Arc::new(Mutex::new(Vec::new()));
    let mut screen = Screen::new(120, replies.clone(), Arc::new(AtomicUsize::new(0)));
    let mut parser = vte::Parser::<0>::new_with_size();
    let text = format!(
        "header\r\n\x1b[0;97;100m{}\x1b[0m\r\n> echo 中e\u{301}\x1b7",
        " ".repeat(119)
    );
    parser.advance(&mut screen, text.as_bytes());
    assert_eq!((screen.row, screen.column), (2, 10));
    screen.resize(48);
    assert_eq!((screen.row, screen.column), (4, 10));
    assert!(screen.wrapped[1] && screen.wrapped[2] && !screen.wrapped[3]);
    assert!(
        screen.cells[1]
            .iter()
            .all(|cell| cell.background == Some(Color::Ansi(100)))
    );
    parser.advance(&mut screen, b"\x1b[6n");
    assert_eq!(*replies.lock().unwrap(), [b"\x1b[5;11R".to_vec()]);
    screen.resize(160);
    assert_eq!((screen.row, screen.column), (2, 10));
    assert_eq!(screen.saved, (2, 10));
    assert!(!screen.wrapped.iter().any(|wrapped| *wrapped));
}

#[test]
fn screen_consumes_rgb_and_indexed_sgr_as_colors_not_independent_ansi_parameters() {
    let mut screen = Screen::new(
        80,
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(AtomicUsize::new(0)),
    );
    let mut parser = vte::Parser::<0>::new_with_size();
    parser.advance(
        &mut screen,
        b"\x1b[0;38;2;238;243;248;48;2;100;54;59mR\x1b[0mN",
    );
    assert_eq!(
        screen.printed[0].background,
        Some(Color::Rgb([100, 54, 59]))
    );
    assert_eq!(screen.printed[1].background, None);
    parser.advance(&mut screen, b"\x1b[0;38;5;231;48;5;23mI\x1b[0mN");
    assert_eq!(screen.printed[2].background, Some(Color::Indexed(23)));
    assert_eq!(screen.printed[3].background, None);
    parser.advance(&mut screen, b"\x1b[38;2;40;41;42mF");
    assert_eq!(
        screen.printed[4].background, None,
        "foreground RGB components must not set ANSI backgrounds"
    );
}
