//! Shell presentation only: semantic colors, capability-aware styles and contrast.

mod editor;
mod fragment;
pub mod preview;

use crate::status::{ColorDepth, StatusPalette};
pub use crate::status::{ColorPair, Theme, ThemeHandle};
use crate::style::Terminal;
pub(crate) use editor::{FrameTheme, ThemedHinter, completion_menu};
pub use fragment::Fragment;
use nu_ansi_term::{Color, Style};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Builtin {
    #[default]
    Dark,
    Light,
}

impl Builtin {
    pub const ALL: [Self; 2] = [Self::Dark, Self::Light];

    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "soft-dark",
            Self::Light => "soft-light",
        }
    }

    pub fn palette(self) -> Theme {
        let p = pair;
        match self {
            Self::Dark => Theme {
                powerline: false,
                environment: p(0x11111b, 0xfab387, 233, 216),
                git_branch: p(0x11111b, 0xf9e2af, 233, 223),
                status: StatusPalette {
                    neutral: p(0xcdd6f4, 0x242433, 189, 235),
                    advice: p(0x11111b, 0xb4befe, 233, 147),
                    pending: p(0xbac2de, 0x272938, 146, 236),
                    notice: p(0x11111b, 0xf9e2af, 233, 223),
                    error: p(0x11111b, 0xf38ba8, 233, 211),
                },
                operation: p(0xcdd6f4, 0x1e1e2e, 189, 235),
                mode: p(0xa6adc8, 0x1e1e2e, 146, 235),
                selection: p(0xcdd6f4, 0x45416a, 189, 239),
                text: TextPalette([
                    p(0xcdd6f4, 0x1e1e2e, 189, 235),
                    p(0xa6adc8, 0x1e1e2e, 146, 235),
                    p(0x89b4fa, 0x1e1e2e, 111, 235),
                    p(0xa6e3a1, 0x1e1e2e, 151, 235),
                    p(0xf9e2af, 0x1e1e2e, 223, 235),
                    p(0xf38ba8, 0x1e1e2e, 211, 235),
                    p(0xcba6f7, 0x1e1e2e, 183, 235),
                    p(0xf2cdcd, 0x1e1e2e, 224, 235),
                ]),
            },
            Self::Light => Theme {
                powerline: false,
                environment: p(0x11111b, 0xfe640b, 233, 202),
                git_branch: p(0x11111b, 0xdf8e1d, 233, 172),
                status: StatusPalette {
                    neutral: p(0x4c4f69, 0xe6e9f0, 240, 255),
                    advice: p(0x11111b, 0x7287fd, 233, 105),
                    pending: p(0x5c5f77, 0xe4e7ee, 60, 254),
                    notice: p(0x11111b, 0xdf8e1d, 233, 172),
                    error: p(0xeff1f5, 0xd20f39, 231, 161),
                },
                operation: p(0x4c4f69, 0xeff1f5, 240, 255),
                mode: p(0x5c5f77, 0xeff1f5, 60, 255),
                selection: p(0x354d82, 0xccd7ef, 240, 189),
                text: TextPalette([
                    p(0x4c4f69, 0xeff1f5, 240, 255),
                    p(0x5c5f77, 0xeff1f5, 60, 255),
                    p(0x1e66d5, 0xeff1f5, 26, 255),
                    p(0x28733f, 0xeff1f5, 238, 255),
                    p(0x815200, 0xeff1f5, 94, 255),
                    p(0xb92346, 0xeff1f5, 125, 255),
                    p(0x7830cc, 0xeff1f5, 92, 255),
                    p(0x805010, 0xeff1f5, 94, 255),
                ]),
            },
        }
    }
}

impl std::str::FromStr for Builtin {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|theme| theme.name() == value)
            .ok_or_else(|| format!("unknown theme {value:?}; expected soft-dark or soft-light"))
    }
}

/// Only known direct renderers qualify; UTF-8 alone says nothing about glyphs.
pub fn automatic_powerline(terminal: Terminal) -> bool {
    if !terminal.tty
        || !terminal.ansi
        || !terminal.color
        || !terminal.unicode
        || crate::status::color_depth() == ColorDepth::Plain
    {
        return false;
    }
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        builtin_powerline(
            std::env::var("TERM").ok().as_deref(),
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            std::env::var("KITTY_WINDOW_ID").ok().as_deref(),
            [
                "TMUX",
                "STY",
                "ZELLIJ",
                "SSH_TTY",
                "SSH_CONNECTION",
                "SSH_CLIENT",
            ]
            .iter()
            .any(|name| std::env::var_os(name).is_some()),
        )
    })
}

fn builtin_powerline(
    term: Option<&str>,
    program: Option<&str>,
    kitty_window: Option<&str>,
    indirect: bool,
) -> bool {
    !indirect
        && match term {
            Some("xterm-kitty") => kitty_window
                .and_then(|id| id.parse::<u64>().ok())
                .is_some_and(|id| id > 0),
            Some("xterm-ghostty") => program == Some("ghostty"),
            _ => false,
        }
}

fn pair(foreground: u32, background: u32, fg: u8, bg: u8) -> ColorPair {
    let rgb = |value: u32| {
        let [_, r, g, b] = value.to_be_bytes();
        [r, g, b]
    };
    ColorPair::new(rgb(foreground), rgb(background), fg, bg)
        .expect("built-in palettes use extended color indexes")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Text,
    Muted,
    Accent,
    Positive,
    Warning,
    Error,
    Keyword,
    String,
}

impl Role {
    pub const ALL: [Self; 8] = [
        Self::Text,
        Self::Muted,
        Self::Accent,
        Self::Positive,
        Self::Warning,
        Self::Error,
        Self::Keyword,
        Self::String,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Muted => "muted",
            Self::Accent => "accent",
            Self::Positive => "positive",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Keyword => "keyword",
            Self::String => "string",
        }
    }
}

/// Backgrounds here describe the host reference surface; only foregrounds are painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextPalette(pub [ColorPair; 8]);

impl Theme {
    pub fn text_color(self, role: Role) -> ColorPair {
        self.text.0[role as usize]
    }
}

pub fn nearest_index(rgb: [u8; 3]) -> u8 {
    (16..=255)
        .min_by_key(|&index| {
            indexed_rgb(index)
                .expect("extended index")
                .into_iter()
                .zip(rgb)
                .map(|(a, b)| (i32::from(a) - i32::from(b)).pow(2))
                .sum::<i32>()
        })
        .expect("extended palette is nonempty")
}

pub fn color_rgb(color: Color) -> Result<Option<[u8; 3]>, String> {
    const ANSI: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 0, 0],
        [0, 205, 0],
        [205, 205, 0],
        [0, 0, 238],
        [205, 0, 205],
        [0, 205, 205],
        [229, 229, 229],
        [127, 127, 127],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [92, 92, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    match color {
        Color::Default => Ok(None),
        Color::Rgb(r, g, b) => Ok(Some([r, g, b])),
        Color::Fixed(index) if index < 16 => Ok(Some(ANSI[usize::from(index)])),
        Color::Fixed(index) => indexed_rgb(index).map(Some).map_err(|e| e.to_string()),
        _ => Err("expected a decoded RGB or indexed color".into()),
    }
}

impl Theme {
    pub fn pairs(self) -> impl Iterator<Item = (&'static str, ColorPair)> {
        [
            ("environment", self.environment),
            ("git_branch", self.git_branch),
            ("status", self.status.neutral),
            ("advice", self.status.advice),
            ("pending", self.status.pending),
            ("notice", self.status.notice),
            ("failure", self.status.error),
            ("operation", self.operation),
            ("mode", self.mode),
            ("selection", self.selection),
        ]
        .into_iter()
        .chain(Role::ALL.map(|role| (role.name(), self.text_color(role))))
    }

    pub fn override_color(&mut self, name: &str, pair: ColorPair) -> Result<(), String> {
        self.validate_color(name, pair)?;
        let target = match name {
            "environment" => &mut self.environment,
            "git_branch" => &mut self.git_branch,
            "status" => &mut self.status.neutral,
            "advice" => &mut self.status.advice,
            "pending" => &mut self.status.pending,
            "notice" => &mut self.status.notice,
            "failure" => &mut self.status.error,
            "operation" => &mut self.operation,
            "mode" => &mut self.mode,
            "selection" => &mut self.selection,
            _ => {
                let role = Role::ALL
                    .into_iter()
                    .find(|role| role.name() == name)
                    .ok_or_else(|| format!("unknown theme color role {name:?}"))?;
                &mut self.text.0[role as usize]
            }
        };
        *target = pair;
        Ok(())
    }

    fn validate_color(&self, name: &str, pair: ColorPair) -> Result<(), String> {
        let reference = self.text_color(Role::Text);
        if (matches!(name, "operation" | "mode")
            || Role::ALL.iter().any(|role| role.name() == name))
            && (reference.background_rgb() != pair.background_rgb()
                || reference.background_index() != pair.background_index())
        {
            return Err(format!(
                "{name}: text colors inherit the theme's host reference background"
            ));
        }
        validate_pair(name, pair)
    }

    pub fn validate(self) -> Result<(), String> {
        for (name, pair) in self.pairs() {
            self.validate_color(name, pair)?;
        }
        Ok(())
    }

    pub fn styles(self, terminal: Terminal) -> Styles {
        Styles {
            theme: self,
            terminal,
            depth: crate::status::color_depth(),
        }
    }
}

pub fn contrast(foreground: [u8; 3], background: [u8; 3]) -> f64 {
    fn luminance(rgb: [u8; 3]) -> f64 {
        let [r, g, b] = rgb.map(|component| {
            let value = f64::from(component) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }
    let a = luminance(foreground);
    let b = luminance(background);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

pub fn indexed_rgb(index: u8) -> Result<[u8; 3], crate::status::InvalidColorIndex> {
    if index < 16 {
        return Err(crate::status::InvalidColorIndex(index));
    }
    if index >= 232 {
        return Ok([8 + 10 * (index - 232); 3]);
    }
    let cube = [0, 95, 135, 175, 215, 255];
    let color = usize::from(index - 16);
    Ok([cube[color / 36], cube[color / 6 % 6], cube[color % 6]])
}

fn validate_pair(name: &str, pair: ColorPair) -> Result<(), String> {
    let rgb = contrast(pair.foreground_rgb(), pair.background_rgb());
    let indexed = contrast(
        indexed_rgb(pair.foreground_index()).map_err(|e| e.to_string())?,
        indexed_rgb(pair.background_index()).map_err(|e| e.to_string())?,
    );
    if rgb < 4.5 || indexed < 4.5 {
        return Err(format!(
            "{name}: contrast must be at least 4.5:1 (RGB {rgb:.2}:1, indexed {indexed:.2}:1)"
        ));
    }
    Ok(())
}

/// A complete, immutable styling snapshot for one rendering operation.
#[derive(Clone, Copy, Debug)]
pub struct Styles {
    pub(crate) theme: Theme,
    pub(crate) terminal: Terminal,
    pub(crate) depth: ColorDepth,
}

impl Styles {
    pub fn powerline(self) -> bool {
        self.theme.powerline
            && self.terminal.color
            && self.terminal.unicode
            && self.depth != ColorDepth::Plain
    }

    pub fn external(self, mut style: Style) -> Style {
        if !self.terminal.color || self.depth == ColorDepth::Plain {
            return Style::new();
        }
        if self.depth == ColorDepth::Indexed {
            for color in [&mut style.foreground, &mut style.background] {
                if let Some(Color::Rgb(r, g, b)) = *color {
                    *color = Some(Color::Fixed(nearest_index([r, g, b])));
                }
            }
        }
        style
    }

    pub fn style(self, role: Role) -> Style {
        self.pair_style(self.theme.text_color(role), false)
    }

    pub fn selection(self) -> Style {
        self.pair_style(self.theme.selection, true)
    }

    pub(crate) fn pair_style(self, pair: ColorPair, background: bool) -> Style {
        if !self.terminal.color || self.depth == ColorDepth::Plain {
            return Style::new();
        }
        let color = |rgb: [u8; 3], index| match self.depth {
            ColorDepth::Rgb => Color::Rgb(rgb[0], rgb[1], rgb[2]),
            ColorDepth::Indexed => Color::Fixed(index),
            ColorDepth::Plain => Color::Default,
        };
        let style = pair
            .attributes()
            .fg(color(pair.foreground_rgb(), pair.foreground_index()));
        if background {
            style.on(color(pair.background_rgb(), pair.background_index()))
        } else {
            style
        }
    }

    pub fn paint(self, role: Role, text: &str) -> String {
        self.style(role).paint(text).to_string()
    }

    pub fn dim(self, text: &str) -> String {
        self.paint(Role::Muted, text)
    }
    pub fn red(self, text: &str) -> String {
        self.paint(Role::Error, text)
    }
    pub fn green(self, text: &str) -> String {
        self.paint(Role::Positive, text)
    }
    pub fn yellow(self, text: &str) -> String {
        self.paint(Role::Warning, text)
    }
    pub fn cyan(self, text: &str) -> String {
        self.paint(Role::Accent, text)
    }
    pub fn bold(self, text: &str) -> String {
        self.emphasis(Role::Text, text)
    }
    pub fn red_bold(self, text: &str) -> String {
        self.emphasis(Role::Error, text)
    }
    pub fn blue_bold(self, text: &str) -> String {
        self.emphasis(Role::Accent, text)
    }

    fn emphasis(self, role: Role, text: &str) -> String {
        if !self.terminal.color || self.depth == ColorDepth::Plain {
            return text.to_owned();
        }
        self.style(role).bold().paint(text).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_powerline_requires_consistent_direct_renderer_hints() {
        for (term, program, window) in [
            (Some("xterm-kitty"), None, Some("1")),
            (Some("xterm-ghostty"), Some("ghostty"), None),
        ] {
            assert!(builtin_powerline(term, program, window, false));
            assert!(!builtin_powerline(term, program, window, true));
        }
        for (term, program, window) in [
            (None, None, None),
            (Some("xterm-256color"), Some("vscode"), None),
            (Some("xterm-256color"), Some("WezTerm"), None),
            (Some("xterm-256color"), Some("ghostty"), Some("1")),
            (Some("screen-256color"), Some("ghostty"), Some("1")),
            (Some("tmux-256color"), Some("ghostty"), Some("1")),
            (Some("xterm-kitty"), None, None),
            (Some("xterm-kitty"), None, Some("")),
            (Some("xterm-kitty"), None, Some("0")),
            (Some("xterm-kitty"), None, Some("invalid")),
            (Some("xterm-ghostty"), None, None),
            (Some("xterm-ghostty"), Some("vscode"), None),
        ] {
            assert!(
                !builtin_powerline(term, program, window, false),
                "{term:?}/{program:?}/{window:?}"
            );
        }
    }

    #[test]
    fn powerline_shapes_require_color_and_unicode_even_when_explicitly_enabled() {
        for powerline in [false, true] {
            for color in [false, true] {
                for unicode in [false, true] {
                    for depth in [ColorDepth::Plain, ColorDepth::Indexed, ColorDepth::Rgb] {
                        let styles = Styles {
                            theme: Theme {
                                powerline,
                                ..Theme::default()
                            },
                            terminal: Terminal {
                                tty: true,
                                ansi: true,
                                color,
                                unicode,
                            },
                            depth,
                        };
                        assert_eq!(
                            styles.powerline(),
                            powerline && color && unicode && depth != ColorDepth::Plain
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn builtins_and_all_extended_fallbacks_meet_contrast_minimum() {
        for theme in Builtin::ALL {
            theme.palette().validate().unwrap();
            assert_eq!(theme.name().parse::<Builtin>().unwrap(), theme);
        }
        for name in ["classic", "default", "", "auto"] {
            assert!(name.parse::<Builtin>().is_err());
        }
    }

    #[test]
    fn invalid_override_is_atomic() {
        let mut theme = Theme::default();
        let original = theme;
        let low = ColorPair::new([0; 3], [0; 3], 16, 16).unwrap();
        assert!(theme.override_color("operation", low).is_err());
        assert_eq!(theme, original);
        assert!(theme.override_color("unknown", theme.operation).is_err());
        assert!(theme.override_color("muted", theme.environment).is_err());
        assert!(
            theme
                .override_color("operation", theme.environment)
                .is_err()
        );
        assert!(theme.override_color("mode", theme.environment).is_err());
        assert_eq!(theme, original);
    }

    #[test]
    fn styles_respect_capabilities_and_do_not_paint_text_backgrounds() {
        for theme in Builtin::ALL {
            for depth in [ColorDepth::Rgb, ColorDepth::Indexed, ColorDepth::Plain] {
                for color in [true, false] {
                    let styles = Styles {
                        theme: theme.palette(),
                        terminal: Terminal {
                            tty: true,
                            ansi: true,
                            color,
                            unicode: false,
                        },
                        depth,
                    };
                    for role in Role::ALL {
                        let painted = styles.paint(role, "text");
                        assert_eq!(crate::style::strip_ansi(&painted), "text");
                        assert_eq!(
                            painted.contains('\x1b'),
                            color && depth != ColorDepth::Plain
                        );
                        assert!(styles.style(role).background.is_none());
                    }
                    assert_eq!(
                        styles.bold("x").contains('\x1b'),
                        color && depth != ColorDepth::Plain
                    );
                }
            }
        }
    }
}
