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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Pair {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub foreground_index: u8,
    pub background_index: u8,
}

impl Pair {
    fn normal(background: [u8; 3], background_index: u8) -> Self {
        Self {
            foreground: [238, 243, 248],
            foreground_index: 231,
            background,
            background_index,
        }
    }

    pub fn sgr(self, depth: ColorDepth) -> String {
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

pub(super) fn pair(zone: usize, tone: Tone) -> Pair {
    match zone {
        0 => Pair::normal([20, 60, 64], 23),
        2 => Pair::normal([36, 69, 97], 24),
        3 => Pair {
            foreground: [199, 212, 224],
            foreground_index: 252,
            background: [27, 37, 51],
            background_index: 234,
        },
        _ => match tone {
            Tone::Advice => Pair::normal([35, 60, 91], 18),
            Tone::Pending => Pair::normal([45, 53, 66], 235),
            Tone::Notice => Pair::normal([89, 72, 38], 58),
            Tone::Error => Pair::normal([100, 54, 59], 88),
            _ => Pair::normal([37, 47, 63], 236),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
