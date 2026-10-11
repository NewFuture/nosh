//! Project only the effective suggestion action into the host's Readline maps.

use super::{Action, Capabilities, Config, Context, EditorMode, Key};
use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BashKey {
    Literal(String),
    Terminfo(String),
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BashBinding {
    pub keymap: &'static str,
    pub key: BashKey,
    pub explicit: bool,
}

impl Config {
    pub fn bash_suggestions(&self) -> (Vec<BashBinding>, Vec<String>) {
        let (compiled, mut warnings) = self.compile(Capabilities { enhanced: true }, true);
        let mut bindings = Vec::new();
        for (mode, keymap) in [
            (EditorMode::Emacs, "emacs-standard"),
            (EditorMode::ViInsert, "vi-insert"),
            (EditorMode::ViNormal, "vi-command"),
        ] {
            for (key, binding) in &compiled.map(mode, Context::Editing).keys {
                if binding.action != Some(Action::AiSuggest) {
                    continue;
                }
                if let Some(key) = bash_key(key.normalized(Capabilities { enhanced: false })) {
                    bindings.push(BashBinding {
                        keymap,
                        key,
                        explicit: binding.custom,
                    });
                } else {
                    warnings.push(format!(
                        "nosh init bash: {keymap}: suggestion key {key:?} has no supported Readline encoding"
                    ));
                }
            }
        }
        bindings.sort();
        (bindings, warnings)
    }
}

fn bash_key(key: Key) -> Option<BashKey> {
    use KeyCode as K;
    use KeyModifiers as M;
    let character = match (key.modifiers, key.code) {
        (M::CONTROL, K::Char(c @ 'a'..='z')) => char::from(c as u8 - b'a' + 1),
        (M::CONTROL, K::Char(c @ '\\'..='_')) => char::from(c as u8 - b'@'),
        (M::NONE, K::Enter) => '\r',
        (M::NONE, K::Tab) => '\t',
        (M::NONE, K::Esc) => '\x1b',
        (modifiers, K::Char(mut c)) if c.is_ascii() && !c.is_control() => {
            if modifiers.contains(M::SHIFT) {
                if !c.is_ascii_alphabetic() {
                    return None;
                }
                c = c.to_ascii_uppercase();
            }
            return match modifiers - M::SHIFT {
                M::NONE => Some(BashKey::Literal(c.to_string())),
                M::ALT => Some(BashKey::Literal(format!("\x1b{c}"))),
                _ => None,
            };
        }
        (M::NONE, K::F(n)) => return Some(BashKey::Terminfo(format!("kf{n}"))),
        (modifiers, code) => {
            let capability = match (modifiers, code) {
                (M::SHIFT, K::BackTab) => "kcbt",
                (M::NONE, K::Backspace) => "kbs",
                (M::NONE, K::Delete) => "kdch1",
                (M::NONE, K::Left) => "kcub1",
                (M::NONE, K::Right) => "kcuf1",
                (M::NONE, K::Up) => "kcuu1",
                (M::NONE, K::Down) => "kcud1",
                (M::NONE, K::Home) => "khome",
                (M::NONE, K::End) => "kend",
                (M::NONE, K::PageUp) => "kpp",
                (M::NONE, K::PageDown) => "knp",
                _ => return None,
            };
            return Some(BashKey::Terminfo(capability.into()));
        }
    };
    Some(BashKey::Literal(character.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editing::ModeBindings;

    #[test]
    fn defaults_and_explicit_overrides_come_from_the_editor() {
        let mut config = Config::default();
        let (bindings, warnings) = config.bash_suggestions();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(bindings.len(), 3);
        assert!(bindings.iter().all(|binding| {
            binding.key == BashKey::Terminfo("kf2".into()) && !binding.explicit
        }));

        config
            .keybindings
            .actions
            .insert("ai_suggest".into(), vec!["Ctrl+G".into(), "F3".into()]);
        config.keybindings.modes.insert(
            "vi_normal".into(),
            ModeBindings {
                actions: [("ai_suggest".into(), Vec::new())].into(),
                ..Default::default()
            },
        );
        let (bindings, _) = config.bash_suggestions();
        assert_eq!(bindings.len(), 4);
        assert!(bindings.iter().all(|binding| binding.explicit));
        assert!(
            bindings
                .iter()
                .all(|binding| binding.keymap != "vi-command")
        );
        assert!(
            bindings
                .iter()
                .any(|binding| { binding.key == BashKey::Literal("\x07".into()) })
        );
    }

    #[test]
    fn displaced_defaults_and_context_unbinding_do_not_reappear() {
        let mut config = Config::default();
        config
            .keybindings
            .actions
            .insert("undo".into(), vec!["F2".into()]);
        assert!(config.bash_suggestions().0.is_empty());
        config.keybindings.actions.clear();
        config
            .keybindings
            .contexts
            .insert("editing".into(), [("ai_suggest".into(), Vec::new())].into());
        assert!(config.bash_suggestions().0.is_empty());
    }

    #[test]
    fn unsupported_keys_warn_instead_of_installing_different_keys() {
        let mut config = Config::default();
        config
            .keybindings
            .actions
            .insert("ai_suggest".into(), vec!["Alt+F2".into()]);
        let (bindings, warnings) = config.bash_suggestions();
        assert!(bindings.is_empty());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("Readline encoding"))
        );

        config
            .keybindings
            .actions
            .insert("ai_suggest".into(), vec!["Ctrl+Shift+Z".into()]);
        let (bindings, warnings) = config.bash_suggestions();
        assert!(bindings.is_empty());
        assert!(!warnings.is_empty());

        config
            .keybindings
            .actions
            .insert("ai_suggest".into(), Vec::new());
        config
            .keybindings
            .actions
            .insert("undo".into(), vec!["Ctrl+Shift+Z".into()]);
        assert!(config.bash_suggestions().0.is_empty());
    }
}
