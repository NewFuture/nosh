//! One self-contained status-row palette, with an explicit extended-color fallback.

use super::Tone;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorDepth {
    Plain,
    Indexed,
    Rgb,
}

impl ColorDepth {
    fn from_env(term: Option<&str>, colorterm: Option<&str>, windows_terminal: bool) -> Self {
        if matches!(term, Some("linux" | "vt100" | "vt220" | "dumb" | "unknown")) {
            return Self::Plain;
        }
        if windows_terminal
            || colorterm.is_some_and(|value| {
                value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
            })
        {
            Self::Rgb
        } else if term.is_some_and(|term| term.contains("256color")) {
            Self::Indexed
        } else {
            Self::Plain
        }
    }
}

pub(crate) fn color_depth() -> ColorDepth {
    static DEPTH: OnceLock<ColorDepth> = OnceLock::new();
    *DEPTH.get_or_init(|| {
        ColorDepth::from_env(
            std::env::var("TERM").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var_os("WT_SESSION").is_some_and(|value| !value.is_empty())
                && std::env::var_os("TMUX").is_none(),
        )
    })
}

/// Explicit foreground/background colors and their extended 256-color fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorPair {
    foreground: [u8; 3],
    background: [u8; 3],
    foreground_index: u8,
    background_index: u8,
}

/// ANSI entries 0–15 are theme-overridable and cannot be an explicit fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidColorIndex(pub u8);

impl std::fmt::Display for InvalidColorIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "status theme fallback must use a 256-color index in 16..=255, got {}",
            self.0
        )
    }
}

impl std::error::Error for InvalidColorIndex {}

impl ColorPair {
    pub fn new(
        foreground: [u8; 3],
        background: [u8; 3],
        foreground_index: u8,
        background_index: u8,
    ) -> Result<Self, InvalidColorIndex> {
        for index in [foreground_index, background_index] {
            if index < 16 {
                return Err(InvalidColorIndex(index));
            }
        }
        Ok(Self {
            foreground,
            background,
            foreground_index,
            background_index,
        })
    }

    pub fn foreground_rgb(self) -> [u8; 3] {
        self.foreground
    }
    pub fn background_rgb(self) -> [u8; 3] {
        self.background
    }
    pub fn foreground_index(self) -> u8 {
        self.foreground_index
    }
    pub fn background_index(self) -> u8 {
        self.background_index
    }

    fn normal(background: [u8; 3], background_index: u8) -> Self {
        Self {
            foreground: [238, 243, 248],
            foreground_index: 231,
            background,
            background_index,
        }
    }

    pub(crate) fn sgr(self, depth: ColorDepth) -> String {
        match depth {
            ColorDepth::Rgb => {
                let [r, g, b] = self.foreground;
                let [br, bg, bb] = self.background;
                format!("\x1b[0;38;2;{r};{g};{b};48;2;{br};{bg};{bb}m")
            }
            ColorDepth::Indexed => format!(
                "\x1b[0;38;5;{};48;5;{}m",
                self.foreground_index, self.background_index
            ),
            ColorDepth::Plain => String::new(),
        }
    }
}

/// Stable semantic regions; a theme does not own their geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Environment,
    Status,
    Operation,
    Mode,
}

/// Presentation semantics, not command risk, permission or execution outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusTone {
    Neutral,
    Advice,
    Pending,
    Notice,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusPalette {
    pub neutral: ColorPair,
    pub advice: ColorPair,
    pub pending: ColorPair,
    pub notice: ColorPair,
    pub error: ColorPair,
}

/// One injectable color palette, independent of layout, glyphs and business state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub environment: ColorPair,
    pub status: StatusPalette,
    pub operation: ColorPair,
    pub mode: ColorPair,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            environment: ColorPair::normal([20, 60, 64], 23),
            status: StatusPalette {
                neutral: ColorPair::normal([37, 47, 63], 236),
                advice: ColorPair::normal([35, 60, 91], 18),
                pending: ColorPair::normal([45, 53, 66], 235),
                notice: ColorPair::normal([89, 72, 38], 58),
                error: ColorPair::normal([100, 54, 59], 88),
            },
            operation: ColorPair::normal([36, 69, 97], 24),
            mode: ColorPair {
                foreground: [199, 212, 224],
                foreground_index: 252,
                background: [27, 37, 51],
                background_index: 234,
            },
        }
    }
}

impl Theme {
    pub fn colors(&self, region: Region, state: StatusTone) -> ColorPair {
        match region {
            Region::Environment => self.environment,
            Region::Operation => self.operation,
            Region::Mode => self.mode,
            Region::Status => match state {
                StatusTone::Neutral => self.status.neutral,
                StatusTone::Advice => self.status.advice,
                StatusTone::Pending => self.status.pending,
                StatusTone::Notice => self.status.notice,
                StatusTone::Error => self.status.error,
            },
        }
    }

    pub(super) fn pair(&self, zone: usize, tone: Tone) -> ColorPair {
        let region = match zone {
            0 => Region::Environment,
            1 => Region::Status,
            2 => Region::Operation,
            3 => Region::Mode,
            _ => unreachable!("status row has exactly four regions"),
        };
        let state = match tone {
            Tone::Advice => StatusTone::Advice,
            Tone::Pending => StatusTone::Pending,
            Tone::Notice => StatusTone::Notice,
            Tone::Error => StatusTone::Error,
            _ => StatusTone::Neutral,
        };
        self.colors(region, state)
    }
}

#[cfg(test)]
pub(super) fn pair(zone: usize, tone: Tone) -> ColorPair {
    Theme::default().pair(zone, tone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_colors_reject_theme_overridable_fallback_indexes() {
        for index in 0..16 {
            assert_eq!(
                ColorPair::new([255; 3], [0; 3], index, 23),
                Err(InvalidColorIndex(index))
            );
            assert_eq!(
                ColorPair::new([255; 3], [0; 3], 231, index),
                Err(InvalidColorIndex(index))
            );
        }
        assert!(ColorPair::new([255; 3], [0; 3], 16, 255).is_ok());
    }

    #[test]
    fn default_colors_preserve_the_accepted_rgb_and_indexed_bytes() {
        for (region, state, rgb, indexed) in [
            (
                Region::Environment,
                StatusTone::Neutral,
                "\x1b[0;38;2;238;243;248;48;2;20;60;64m",
                "\x1b[0;38;5;231;48;5;23m",
            ),
            (
                Region::Status,
                StatusTone::Neutral,
                "\x1b[0;38;2;238;243;248;48;2;37;47;63m",
                "\x1b[0;38;5;231;48;5;236m",
            ),
            (
                Region::Operation,
                StatusTone::Neutral,
                "\x1b[0;38;2;238;243;248;48;2;36;69;97m",
                "\x1b[0;38;5;231;48;5;24m",
            ),
            (
                Region::Mode,
                StatusTone::Neutral,
                "\x1b[0;38;2;199;212;224;48;2;27;37;51m",
                "\x1b[0;38;5;252;48;5;234m",
            ),
            (
                Region::Status,
                StatusTone::Advice,
                "\x1b[0;38;2;238;243;248;48;2;35;60;91m",
                "\x1b[0;38;5;231;48;5;18m",
            ),
            (
                Region::Status,
                StatusTone::Pending,
                "\x1b[0;38;2;238;243;248;48;2;45;53;66m",
                "\x1b[0;38;5;231;48;5;235m",
            ),
            (
                Region::Status,
                StatusTone::Notice,
                "\x1b[0;38;2;238;243;248;48;2;89;72;38m",
                "\x1b[0;38;5;231;48;5;58m",
            ),
            (
                Region::Status,
                StatusTone::Error,
                "\x1b[0;38;2;238;243;248;48;2;100;54;59m",
                "\x1b[0;38;5;231;48;5;88m",
            ),
        ] {
            let pair = Theme::default().colors(region, state);
            assert_eq!(pair.sgr(ColorDepth::Rgb), rgb);
            assert_eq!(pair.sgr(ColorDepth::Indexed), indexed);
            assert!(pair.sgr(ColorDepth::Plain).is_empty());
        }
    }

    #[test]
    fn semantic_overrides_change_only_the_selected_color_role() {
        let default = Theme::default();
        let mut updated = default;
        let replacement = ColorPair::new([238, 243, 248], [30, 64, 83], 231, 24).unwrap();
        updated.status.advice = replacement;
        for region in [
            Region::Environment,
            Region::Status,
            Region::Operation,
            Region::Mode,
        ] {
            for state in [
                StatusTone::Neutral,
                StatusTone::Advice,
                StatusTone::Pending,
                StatusTone::Notice,
                StatusTone::Error,
            ] {
                assert_eq!(
                    updated.colors(region, state),
                    if region == Region::Status && state == StatusTone::Advice {
                        replacement
                    } else {
                        default.colors(region, state)
                    }
                );
            }
        }
    }

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

    fn contrast(foreground: [u8; 3], background: [u8; 3]) -> f64 {
        let a = luminance(foreground);
        let b = luminance(background);
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    fn indexed_rgb(index: u8) -> [u8; 3] {
        assert!(
            index >= 16,
            "theme-overridable ANSI entries are not part of this palette"
        );
        if index >= 232 {
            return [8 + 10 * (index - 232); 3];
        }
        let cube = [0, 95, 135, 175, 215, 255];
        let color = usize::from(index - 16);
        [cube[color / 36], cube[color / 6 % 6], cube[color % 6]]
    }

    #[test]
    fn all_custom_text_and_boundary_pairs_exceed_small_text_contrast_minimum() {
        for (name, zone, tone) in [
            ("environment", 0, Tone::Environment),
            ("normal", 1, Tone::Secondary),
            ("operation", 2, Tone::Action),
            ("mode", 3, Tone::Secondary),
            ("advice", 1, Tone::Advice),
            ("pending", 1, Tone::Pending),
            ("notice", 1, Tone::Notice),
            ("error", 1, Tone::Error),
        ] {
            let pair = pair(zone, tone);
            let rgb = contrast(pair.foreground, pair.background);
            let indexed = contrast(
                indexed_rgb(pair.foreground_index),
                indexed_rgb(pair.background_index),
            );
            assert!(rgb >= 7.0, "{name}: RGB text contrast {rgb}");
            assert!(indexed >= 4.5, "{name}: 256-color text contrast {indexed}");
            assert!(
                indexed >= 7.0 || name == "notice",
                "{name}: fallback should prefer 7:1"
            );
            for outside in [[11, 13, 16], [247, 248, 250]] {
                assert!(
                    contrast(pair.foreground, outside).max(contrast(pair.background, outside))
                        >= 4.5,
                    "{name}: neither the edge nor the block stands out against the surrounding background"
                );
            }
            println!(
                "{name}: foreground={:02X?} background={:02X?} RGB={rgb:.2}:1; foreground-index={} background-index={} 256-color={indexed:.2}:1",
                pair.foreground, pair.background, pair.foreground_index, pair.background_index
            );
        }
    }

    #[test]
    fn capability_detection_does_not_assume_an_ansi_theme_or_full_rgb_support() {
        assert_eq!(
            ColorDepth::from_env(Some("xterm-256color"), None, false),
            ColorDepth::Indexed
        );
        assert_eq!(
            ColorDepth::from_env(Some("xterm-256color"), Some("TRUECOLOR"), false),
            ColorDepth::Rgb
        );
        assert_eq!(
            ColorDepth::from_env(Some("xterm-256color"), None, true),
            ColorDepth::Rgb
        );
        assert_eq!(
            ColorDepth::from_env(Some("screen-256color"), None, false),
            ColorDepth::Indexed
        );
        for term in [Some("xterm"), Some("vt100"), Some("linux"), None] {
            assert_eq!(ColorDepth::from_env(term, None, false), ColorDepth::Plain);
        }
        assert_eq!(
            ColorDepth::from_env(Some("linux"), Some("truecolor"), true),
            ColorDepth::Plain
        );
        assert!(pair(0, Tone::Environment).sgr(ColorDepth::Plain).is_empty());
    }

    #[test]
    fn regional_bases_and_state_semantics_have_distinct_backgrounds() {
        let regions = (0..4)
            .map(|zone| pair(zone, Tone::Secondary))
            .collect::<Vec<_>>();
        for (index, region) in regions.iter().enumerate() {
            for other in &regions[index + 1..] {
                assert_ne!(region.background, other.background);
                assert_ne!(region.background_index, other.background_index);
            }
        }
        let states = [
            Tone::Secondary,
            Tone::Advice,
            Tone::Pending,
            Tone::Notice,
            Tone::Error,
        ]
        .map(|tone| pair(1, tone));
        for (index, state) in states.iter().enumerate() {
            for other in &states[index + 1..] {
                assert_ne!(state.background, other.background);
                assert_ne!(state.background_index, other.background_index);
            }
        }
    }
}
