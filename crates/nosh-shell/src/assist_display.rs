//! Versioned, display-only command assistance. Typing invalidates pending results.

use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub enum Assistance {
    Command {
        command_id: u64,
        intent: String,
        program: String,
    },
    Message(String),
}

#[derive(Default)]
struct State {
    version: u64,
    result: Option<Assistance>,
    cancel: Option<Arc<dyn Fn() + Send + Sync>>,
    repaint: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Clone, Default)]
pub struct AssistDisplay {
    state: Arc<Mutex<State>>,
}

impl AssistDisplay {
    pub fn invalidate(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.version += 1;
        let version = state.version;
        let cancel = state.cancel.take();
        state.result = None;
        drop(state);
        if let Some(cancel) = cancel {
            cancel();
        }
        version
    }

    pub fn on_cancel(&self, version: u64, cancel: Arc<dyn Fn() + Send + Sync>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.version == version {
            state.cancel = Some(cancel);
        } else {
            drop(state);
            cancel();
        }
    }

    pub fn on_repaint(&self, repaint: Arc<dyn Fn() + Send + Sync>) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).repaint = Some(repaint);
    }

    /// Returns whether this version was still current at publication.
    pub fn publish(&self, version: u64, result: Option<Assistance>) -> bool {
        let repaint = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.version != version {
                return false;
            }
            state.result = result;
            state.cancel = None;
            state.repaint.clone()
        };
        if let Some(repaint) = repaint {
            repaint();
        }
        true
    }

    pub fn result(&self) -> Option<Assistance> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .result
            .clone()
    }

    pub(crate) fn status(&self) -> String {
        match self.result() {
            Some(Assistance::Command {
                intent, program, ..
            }) => format!(
                "{intent}: {}  (Ctrl+G)",
                crate::style::clip_line(&crate::style::visible_text(&program), 100, 0, "...")
            ),
            Some(Assistance::Message(text)) => crate::style::visible_text(&text).into_owned(),
            None => String::new(),
        }
    }
}

pub(crate) struct AssistEditMode {
    pub inner: Box<dyn reedline::EditMode>,
    pub display: AssistDisplay,
}

impl reedline::EditMode for AssistEditMode {
    fn parse_event(&mut self, raw: reedline::ReedlineRawEvent) -> reedline::ReedlineEvent {
        let event = self.inner.parse_event(raw);
        if !matches!(
            &event,
            reedline::ReedlineEvent::Resize(..) | reedline::ReedlineEvent::Repaint
        ) && !matches!(&event, reedline::ReedlineEvent::ExecuteHostCommand(command) if command == "__nosh_suggest__")
        {
            self.display.invalidate();
        }
        event
    }

    fn edit_mode(&self) -> reedline::PromptEditMode {
        self.inner.edit_mode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superseded_results_cannot_replace_input() {
        let display = AssistDisplay::default();
        let old = display.invalidate();
        let new = display.invalidate();
        assert!(!display.publish(old, Some(Assistance::Message("old".into()))));
        assert!(display.result().is_none());
        assert!(display.publish(new, Some(Assistance::Message("new".into()))));
        assert!(matches!(display.result(), Some(Assistance::Message(s)) if s == "new"));
        display.invalidate();
        assert!(display.result().is_none());
    }

    #[test]
    fn cancellation_callbacks_run_without_holding_display_state() {
        let display = AssistDisplay::default();
        let version = display.invalidate();
        let shared = display.clone();
        display.on_cancel(
            version,
            Arc::new(move || assert!(shared.state.try_lock().is_ok())),
        );
        display.invalidate();
        let shared = display.clone();
        display.on_cancel(
            version,
            Arc::new(move || assert!(shared.state.try_lock().is_ok())),
        );
    }
}
