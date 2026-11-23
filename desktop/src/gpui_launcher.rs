use crate::gpui_input::Action;

pub struct LaunchItem {
    pub title: String,
    pub detail: String,
    pub group: &'static str,
    pub glyph: &'static str,
    pub shortcut: &'static str,
    pub action: Action,
}

impl LaunchItem {
    pub fn new(
        title: impl Into<String>,
        detail: impl Into<String>,
        group: &'static str,
        glyph: &'static str,
        shortcut: &'static str,
        action: Action,
    ) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
            group,
            glyph,
            shortcut,
            action,
        }
    }

    pub fn matches(&self, query: &str) -> bool {
        let text = format!(
            "{} {} {} {}",
            self.title, self.detail, self.group, self.glyph
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|word| text.contains(&word.to_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_all_words_across_identity_and_group_without_changing_the_action() {
        let item = LaunchItem::new(
            "Build 中",
            "dev@build.example:2222",
            "Saved connections",
            "SSH",
            "",
            Action::ConnectSaved(3),
        );
        assert!(item.matches("  DEV 2222 "));
        assert!(item.matches("saved 中"));
        assert!(item.matches("SSH 2222"));
        assert!(!item.matches("dev production"));
        assert_eq!(item.action, Action::ConnectSaved(3));
    }
}
