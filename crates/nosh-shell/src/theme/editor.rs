use std::sync::{Arc, Mutex};

use super::{Role, Styles, Theme};
use reedline::{ColumnarMenu, Hinter, History, MenuBuilder, MenuTextStyle};

/// Latched by Prompt::begin_paint, shared by all consumers of that paint.
#[derive(Clone, Debug)]
pub(crate) struct FrameTheme(Arc<Mutex<Styles>>);

impl Default for FrameTheme {
    fn default() -> Self {
        Self::new(Theme::default().styles(crate::style::stdout()))
    }
}

impl FrameTheme {
    #[cfg(test)]
    pub(crate) fn test() -> Self {
        Self::new(Styles {
            theme: Theme::default(),
            terminal: crate::style::Terminal {
                tty: true,
                ansi: true,
                color: true,
                unicode: true,
            },
            depth: crate::status::ColorDepth::Rgb,
        })
    }

    pub(crate) fn new(styles: Styles) -> Self {
        Self(Arc::new(Mutex::new(styles)))
    }

    pub(crate) fn latch(&self, theme: Theme) {
        self.0.lock().expect("frame theme mutex poisoned").theme = theme;
    }

    pub(crate) fn styles(&self) -> Styles {
        *self.0.lock().expect("frame theme mutex poisoned")
    }
}

pub(crate) fn completion_menu(name: &str, frame: FrameTheme) -> ColumnarMenu {
    ColumnarMenu::default()
        .with_name(name)
        .with_preserve_case(true)
        .with_input_mode(reedline::InputMode::FullBuffer)
        .with_marker("")
        .with_columns(10)
        .with_style_provider(move || {
            let styles = frame.styles();
            MenuTextStyle {
                text_style: styles.style(Role::Text),
                selected_text_style: styles.selection(),
                description_style: styles.style(Role::Muted),
                match_style: styles.style(Role::Accent).underline(),
                selected_match_style: styles.selection().underline(),
            }
        })
}

pub(crate) struct ThemedHinter {
    pub inner: Box<dyn Hinter>,
    pub frame: FrameTheme,
}

impl Hinter for ThemedHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        history: &dyn History,
        color: bool,
        cwd: &str,
    ) -> String {
        let hint = self.inner.handle(line, pos, history, false, cwd);
        if color {
            let styles = self.frame.styles();
            let style = styles.style(Role::Muted);
            // No attributes in NO_COLOR/plain mode, even for the history hint.
            let style = if style.foreground.is_some() {
                style.italic()
            } else {
                style
            };
            style.paint(hint).to_string()
        } else {
            hint
        }
    }

    fn complete_hint(&self) -> String {
        self.inner.complete_hint()
    }
    fn next_hint_token(&self) -> String {
        self.inner.next_hint_token()
    }
}
