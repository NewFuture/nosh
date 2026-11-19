use crate::gpui_search::OutputSearch;
use alacritty_terminal::term::cell::Flags;
use eframe::egui;
use gpui::{App, Bounds, Hsla, Pixels, TextRun, Window, fill, font, point, px, rgb, size};
use nosh_desktop::terminal::{TerminalBackend, terminal_color};

pub fn color(color: egui::Color32) -> Hsla {
    rgb((u32::from(color.r()) << 16) | (u32::from(color.g()) << 8) | u32::from(color.b())).into()
}

fn terminal_font() -> gpui::Font {
    font(if cfg!(windows) {
        "Consolas"
    } else {
        "monospace"
    })
}

pub fn cell_size(font_size: f32, window: &Window) -> gpui::Size<Pixels> {
    let run = TextRun {
        len: 1,
        font: terminal_font(),
        color: rgb(0xdadce8).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let width = window
        .text_system()
        .shape_line("M".into(), px(font_size), &[run], None)
        .width
        .ceil();
    size(width, px((font_size * 1.4).ceil()))
}

pub fn paint(
    backend: &TerminalBackend,
    bounds: Bounds<Pixels>,
    cell_size: gpui::Size<Pixels>,
    font_size: f32,
    search: &mut OutputSearch,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Result<bool> {
    window.paint_quad(fill(bounds, rgb(0x1c1e26)));
    let mut term = backend.model().lock();
    let search_changed = search.open && search.refresh(&mut term);
    let content = term.renderable_content();
    let offset = content.display_offset as i32;
    // Backgrounds precede glyphs so wide characters are not covered by spacer cells.
    for cell in term.renderable_content().display_iter {
        let row = cell.point.line.0 + offset;
        if row < 0 {
            continue;
        }
        let origin = bounds.origin
            + point(
                cell_size.width * cell.point.column.0 as f32,
                cell_size.height * row as f32,
            );
        let bg = if content.selection.is_some_and(|s| s.contains(cell.point)) {
            egui::Color32::from_rgb(46, 75, 70)
        } else if let Some(selected) = search.open.then(|| search.highlight(cell.point)).flatten() {
            egui::Color32::from_rgb(
                if selected { 105 } else { 55 },
                if selected { 83 } else { 65 },
                if selected { 35 } else { 84 },
            )
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
        window.paint_quad(fill(Bounds::new(origin, cell_size), color(bg)));
    }
    for cell in content.display_iter {
        let row = cell.point.line.0 + offset;
        if row < 0
            || cell.c == ' '
            || cell.flags.intersects(
                Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER | Flags::HIDDEN,
            )
        {
            continue;
        }
        let origin = bounds.origin
            + point(
                cell_size.width * cell.point.column.0 as f32,
                cell_size.height * row as f32,
            );
        let mut fg = terminal_color(
            if cell.flags.contains(Flags::INVERSE) {
                cell.bg
            } else {
                cell.fg
            },
            content.colors,
        );
        let mut text = cell.c.to_string();
        if let Some(zero_width) = cell.zerowidth() {
            text.extend(zero_width);
        }
        if cell.flags.contains(Flags::DIM) {
            fg = fg.gamma_multiply(0.65);
        }
        let mut font = terminal_font();
        if cell.flags.contains(Flags::BOLD) {
            font.weight = gpui::FontWeight::BOLD;
        }
        let run = TextRun {
            len: text.len(),
            font,
            color: color(fg),
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
            px(font_size),
            &[run],
            Some(cell_size.width),
        );
        line.paint(origin, cell_size.height, window, cx)?;
    }
    let cursor = content.cursor;
    let row = cursor.point.line.0 + offset;
    if cursor.shape != alacritty_terminal::vte::ansi::CursorShape::Hidden && row >= 0 {
        let origin = bounds.origin
            + point(
                cell_size.width * cursor.point.column.0 as f32,
                cell_size.height * row as f32,
            );
        window.paint_quad(fill(
            Bounds::new(
                origin + point(px(0.), cell_size.height - px(2.)),
                size(cell_size.width, px(2.)),
            ),
            rgb(0x84d8bd),
        ));
    }
    Ok(search_changed)
}
