use nu_ansi_term::{Color, Style};

use super::Styles;
use crate::style;

/// Cached external prompt text. Only printable text and SGR survive decoding.
#[derive(Clone, Debug, Default)]
pub struct Fragment {
    text: String,
    spans: Vec<(std::ops::Range<usize>, Style)>,
    width: usize,
}

impl Fragment {
    pub fn parse(input: &str) -> Self {
        struct Decoder {
            fragment: Fragment,
            style: Style,
        }
        impl Decoder {
            fn push(&mut self, text: &str) {
                let start = self.fragment.text.len();
                self.fragment.text.push_str(text);
                let end = self.fragment.text.len();
                if let Some((range, style)) = self.fragment.spans.last_mut()
                    && *style == self.style
                {
                    range.end = end;
                } else {
                    self.fragment.spans.push((start..end, self.style));
                }
            }
        }
        impl vte::Perform for Decoder {
            fn print(&mut self, c: char) {
                let mut encoded = [0; 4];
                self.push(&style::visible_text(c.encode_utf8(&mut encoded)));
            }
            fn execute(&mut self, byte: u8) {
                if matches!(byte, b'\r' | b'\n' | b'\t') && !self.fragment.text.ends_with(' ') {
                    self.push(" ");
                }
            }
            fn csi_dispatch(
                &mut self,
                params: &vte::Params,
                intermediate: &[u8],
                ignore: bool,
                action: char,
            ) {
                if action != 'm' || ignore || !intermediate.is_empty() {
                    return;
                }
                let params: Vec<u16> = params.iter().flatten().copied().collect();
                let mut i = 0;
                while i < params.len() {
                    match params[i] {
                        0 => self.style = Style::new(),
                        1 => self.style.is_bold = true,
                        2 => self.style.is_dimmed = true,
                        3 => self.style.is_italic = true,
                        4 => self.style.is_underline = true,
                        5 | 6 => self.style.is_blink = true,
                        7 => self.style.is_reverse = true,
                        8 => self.style.is_hidden = true,
                        9 => self.style.is_strikethrough = true,
                        22 => {
                            self.style.is_bold = false;
                            self.style.is_dimmed = false;
                        }
                        23 => self.style.is_italic = false,
                        24 => self.style.is_underline = false,
                        25 => self.style.is_blink = false,
                        27 => self.style.is_reverse = false,
                        28 => self.style.is_hidden = false,
                        29 => self.style.is_strikethrough = false,
                        30..=37 => {
                            self.style.foreground = Some(Color::Fixed((params[i] - 30) as u8))
                        }
                        40..=47 => {
                            self.style.background = Some(Color::Fixed((params[i] - 40) as u8))
                        }
                        90..=97 => {
                            self.style.foreground = Some(Color::Fixed((params[i] - 90 + 8) as u8))
                        }
                        100..=107 => {
                            self.style.background = Some(Color::Fixed((params[i] - 100 + 8) as u8))
                        }
                        39 => self.style.foreground = None,
                        49 => self.style.background = None,
                        code @ (38 | 48) => {
                            let parsed = match params.get(i + 1) {
                                Some(5) => params
                                    .get(i + 2)
                                    .and_then(|&n| u8::try_from(n).ok())
                                    .map(|n| (Color::Fixed(n), 2)),
                                Some(2) => params.get(i + 2..i + 5).and_then(|rgb| {
                                    Some((
                                        Color::Rgb(
                                            u8::try_from(rgb[0]).ok()?,
                                            u8::try_from(rgb[1]).ok()?,
                                            u8::try_from(rgb[2]).ok()?,
                                        ),
                                        4,
                                    ))
                                }),
                                _ => None,
                            };
                            if let Some((color, consumed)) = parsed {
                                if code == 38 {
                                    self.style.foreground = Some(color);
                                } else {
                                    self.style.background = Some(color);
                                }
                                i += consumed;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
        }
        let mut decoder = Decoder {
            fragment: Self::default(),
            style: Style::new(),
        };
        vte::Parser::<0>::new_with_size().advance(&mut decoder, input.as_bytes());
        decoder.fragment.width = style::width(&decoder.fragment.text);
        decoder.fragment
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn width(&self) -> usize {
        self.width
    }

    pub fn runs(&self) -> impl Iterator<Item = (&str, Style)> {
        self.spans
            .iter()
            .map(|(range, style)| (&self.text[range.clone()], *style))
    }

    pub fn render(&self, columns: usize, styles: Styles) -> String {
        self.render_inner(columns, styles, false).0
    }

    /// Join a following segment without duplicating a recognized closing cap.
    pub(crate) fn render_prefix(&self, columns: usize, styles: Styles) -> (String, Option<Color>) {
        self.render_inner(columns, styles, true)
    }

    fn render_inner(
        &self,
        columns: usize,
        styles: Styles,
        joining: bool,
    ) -> (String, Option<Color>) {
        let background = |paint: Style| {
            if paint.is_reverse {
                paint.foreground
            } else {
                paint.background
            }
        };
        let truncated = self.width > columns;
        let suffix = if truncated {
            &"..."[..columns.min(3)]
        } else {
            ""
        };
        let mut end = if truncated {
            style::prefix_end(&self.text, columns - suffix.len())
        } else {
            self.text.len()
        };
        if joining
            && !truncated
            && styles.powerline()
            && let [.., (_, previous), (range, cap)] = self.spans.as_slice()
            && range.end == end
            && &self.text[range.clone()] == "\u{e0b4}"
            && !cap.is_reverse
            && cap.background.is_none()
            && cap.foreground.is_some()
            && cap.foreground == background(*previous)
        {
            end = range.start;
        }
        if !styles.terminal.color || styles.depth == super::ColorDepth::Plain {
            return (format!("{}{suffix}", &self.text[..end]), None);
        }
        let mut output = String::new();
        let mut tail = Style::new();
        for (range, style) in &self.spans {
            if range.start >= end {
                break;
            }
            tail = styles.external(*style);
            output.push_str(
                &tail
                    .paint(&self.text[range.start..range.end.min(end)])
                    .to_string(),
            );
        }
        if !suffix.is_empty() {
            output.push_str(&tail.paint(suffix).to_string());
        }
        (output, background(tail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        style::Terminal,
        theme::{Builtin, ColorDepth},
    };

    #[test]
    fn fragments_keep_styles_but_never_terminal_commands() {
        let fragment =
            Fragment::parse("\x1b]0;unsafe\x07\x1b[2J\x1b[1;38;2;80;120;160m中e\u{301}\x1b[0m\nok");
        assert_eq!(fragment.text(), "中e\u{301} ok");
        assert_eq!(fragment.width(), style::width(fragment.text()));
        let styles = Styles {
            theme: Builtin::Dark.palette(),
            terminal: Terminal {
                tty: true,
                ansi: true,
                color: true,
                unicode: true,
            },
            depth: ColorDepth::Rgb,
        };
        for width in 0..20 {
            let output = fragment.render(width, styles);
            assert!(style::width(&style::strip_ansi(&output)) <= width);
            assert!(!output.contains("\x1b[2J") && !output.contains("\x1b]"));
        }
        assert!(fragment.runs().next().unwrap().1.is_bold);
    }

    #[test]
    fn cached_width_and_plain_rendering_preserve_grapheme_clipping() {
        let fragment = Fragment::parse("\x1b[31m中e\u{301}\x1b[0m👩\u{200d}💻 text\tend");
        let styles = Styles {
            theme: Builtin::Dark.palette(),
            terminal: Terminal {
                tty: true,
                ansi: true,
                color: true,
                unicode: true,
            },
            depth: ColorDepth::Rgb,
        };
        assert_eq!(fragment.width(), style::width(fragment.text()));
        for columns in 0..=fragment.width() + 5 {
            let colored = fragment.render(columns, styles);
            let plain = fragment.render(
                columns,
                Styles {
                    depth: ColorDepth::Plain,
                    ..styles
                },
            );
            assert_eq!(plain, style::strip_ansi(&colored));
            assert_eq!(plain, style::clip_line(fragment.text(), columns, 0, "..."));
            assert!(!plain.contains('\x1b') && !plain.contains('\u{fffd}'));
            assert!(style::width(&plain) <= columns);
            if columns >= fragment.width() {
                assert_eq!(plain, fragment.text());
            }
        }
        assert_eq!(Fragment::default().width(), 0);
    }

    #[test]
    fn joining_keeps_native_spacing_and_literal_private_use_characters() {
        let mut theme = Builtin::Dark.palette();
        theme.powerline = true;
        let styles = Styles {
            theme,
            terminal: Terminal {
                tty: true,
                ansi: true,
                color: true,
                unicode: true,
            },
            depth: ColorDepth::Rgb,
        };
        for text in ["path\u{e0b4}", "path  ", "path\u{e0b4}  "] {
            let fragment = Fragment::parse(&Color::White.on(Color::Blue).paint(text).to_string());
            let (prefix, background) = fragment.render_prefix(80, styles);
            assert_eq!(style::strip_ansi(&prefix), text);
            assert_eq!(prefix, fragment.render(80, styles));
            assert!(background.is_some());
        }
    }
}
