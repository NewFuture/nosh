//! Bilingual UI strings: Chinese when the locale is `zh*`, English otherwise.

use std::sync::OnceLock;

fn detect() -> bool {
    if let Ok(v) = std::env::var("NOSH_LANG") {
        return v.to_ascii_lowercase().starts_with("zh");
    }
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("zh"))
}

/// Whether UI text should be Chinese.
pub fn zh() -> bool {
    static ZH: OnceLock<bool> = OnceLock::new();
    *ZH.get_or_init(detect)
}

/// `tr!(zh, en)` picks the string for the current UI language.
#[macro_export]
macro_rules! tr {
    ($zh:expr, $en:expr $(,)?) => {
        if $crate::lang::zh() { $zh } else { $en }
    };
}
