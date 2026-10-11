//! Versioned, display-only command assistance. Typing invalidates pending results.

use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub enum Assistance {
    Pending {
        command_id: u64,
        intent: String,
    },
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
        let repaint = state.result.take().and_then(|_| state.repaint.clone());
        drop(state);
        if let Some(cancel) = cancel {
            cancel();
        }
        if let Some(repaint) = repaint {
            repaint();
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
        self.update(version, result, true)
    }

    pub fn pending(&self, version: u64, command_id: u64, intent: String) -> bool {
        self.update(
            version,
            Some(Assistance::Pending { command_id, intent }),
            false,
        )
    }

    fn update(&self, version: u64, result: Option<Assistance>, finished: bool) -> bool {
        let repaint = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.version != version {
                return false;
            }
            state.result = result;
            if finished {
                state.cancel = None;
            }
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

    pub(crate) fn status(&self, key: Option<&str>) -> String {
        status_text(self.result().as_ref(), key)
    }
}

pub(crate) fn status_text(result: Option<&Assistance>, key: Option<&str>) -> String {
    match result {
        Some(Assistance::Pending { intent, .. }) => format!(
            "AI {}: {}",
            crate::style::visible_text(intent),
            nosh_platform::tr!("生成建议中", "generating suggestion")
        ),
        Some(Assistance::Command {
            intent, program, ..
        }) => format!(
            "{intent}: {}{}",
            crate::style::clip_line(&crate::style::visible_text(program), 100, 0, "..."),
            key.map(|key| format!("  ({key})")).unwrap_or_default(),
        ),
        Some(Assistance::Message(text)) => crate::style::visible_text(text).into_owned(),
        None => String::new(),
    }
}

pub(crate) struct AssistEditMode {
    pub inner: Box<dyn reedline::EditMode>,
    pub display: AssistDisplay,
    pub completion_pending: bool,
}

impl reedline::EditMode for AssistEditMode {
    fn parse_event(&mut self, raw: reedline::ReedlineRawEvent) -> reedline::ReedlineEvent {
        self.parse_event_with_context(raw, reedline::EditContext::Editing)
    }

    fn parse_event_with_context(
        &mut self,
        raw: reedline::ReedlineRawEvent,
        context: reedline::EditContext,
    ) -> reedline::ReedlineEvent {
        let event = self.inner.parse_event_with_context(raw, context);
        self.completion_pending =
            matches!(event, reedline::ReedlineEvent::CompleteOrHostCommand { .. });
        if !matches!(
            &event,
            reedline::ReedlineEvent::Resize(..) | reedline::ReedlineEvent::Repaint
        ) && !matches!(&event, reedline::ReedlineEvent::ExecuteHostCommand(command)
            if command == crate::repl::SUGGEST_COMMAND
                || command == crate::editing::COMPLETION_AI_COMMAND
                || command == crate::editing::FOCUS_NOTICE
                || command == crate::editing::VI_LIMIT_NOTICE
                || command == crate::editing::EDITOR_NOTICE)
            && !matches!(
                &event,
                reedline::ReedlineEvent::CompleteOrHostCommand { .. }
            )
        {
            self.display.invalidate();
        }
        event
    }

    fn edit_mode(&self) -> reedline::PromptEditMode {
        self.inner.edit_mode()
    }

    fn has_pending_input(&self) -> bool {
        self.inner.has_pending_input()
    }

    fn handle_mode_specific_event(
        &mut self,
        event: reedline::ReedlineEvent,
    ) -> reedline::EventStatus {
        self.inner.handle_mode_specific_event(event)
    }

    fn after_event(&mut self, context: reedline::EditContext, edited: bool) {
        self.inner.after_event(context, edited);
        if self.completion_pending && (edited || context != reedline::EditContext::Editing) {
            self.display.invalidate();
        }
        self.completion_pending = false;
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

    #[test]
    fn pending_state_preserves_cancellation_and_cannot_revive_old_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let display = AssistDisplay::default();
        let cancelled = Arc::new(AtomicUsize::new(0));
        let observed = cancelled.clone();
        let version = display.invalidate();
        display.on_cancel(
            version,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }),
        );
        assert!(display.pending(version, 7, "Next".into()));
        assert!(matches!(
            display.result(),
            Some(Assistance::Pending { command_id: 7, .. })
        ));
        assert!(display.status(Some("F2")).contains("generating suggestion"));
        assert!(!display.status(Some("F2")).contains("F2"));
        assert_eq!(cancelled.load(Ordering::SeqCst), 0);
        display.invalidate();
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        assert!(display.result().is_none());
        assert!(!display.pending(version, 7, "Next".into()));
        assert!(!display.publish(version, Some(Assistance::Message("late".into()))));
    }

    #[test]
    fn invalidating_visible_assistance_repaints_even_for_an_unbound_key() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        use reedline::EditMode;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let display = AssistDisplay::default();
        let repaints = Arc::new(AtomicUsize::new(0));
        let observed = repaints.clone();
        let state = Arc::downgrade(&display.state);
        display.on_repaint(Arc::new(move || {
            assert!(state.upgrade().unwrap().try_lock().is_ok());
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        let mut mode = AssistEditMode {
            inner: Box::new(reedline::Emacs::default()),
            display: display.clone(),
            completion_pending: false,
        };
        for result in [
            Assistance::Pending {
                command_id: 1,
                intent: "Next".into(),
            },
            Assistance::Command {
                command_id: 1,
                intent: "Next".into(),
                program: "true".into(),
            },
            Assistance::Message("unavailable".into()),
        ] {
            let version = display.invalidate();
            display.publish(version, Some(result));
            repaints.store(0, Ordering::SeqCst);
            let key = reedline::ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::F(12),
                KeyModifiers::NONE,
            )))
            .unwrap();
            assert!(matches!(
                mode.parse_event(key),
                reedline::ReedlineEvent::None
            ));
            assert!(display.result().is_none());
            assert_eq!(
                repaints.load(Ordering::SeqCst),
                1,
                "cleared assistance stayed painted"
            );
            display.invalidate();
            assert_eq!(
                repaints.load(Ordering::SeqCst),
                1,
                "empty state caused an extra repaint"
            );
        }
    }
}
