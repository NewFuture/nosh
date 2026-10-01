use super::Theme;
use std::{
    fmt,
    sync::{Arc, Mutex, Weak},
};

type Repaint = dyn Fn() + Send + Sync;

struct State {
    theme: Theme,
    subscribers: Vec<Weak<Repaint>>,
}

/// A shared theme selection for a REPL; replacement requests a repaint, not a new editor.
#[derive(Clone)]
pub struct ThemeHandle(Arc<Mutex<State>>);

impl Default for ThemeHandle {
    fn default() -> Self {
        Self::new(Theme::default())
    }
}

impl fmt::Debug for ThemeHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ThemeHandle")
            .field(&self.snapshot())
            .finish()
    }
}

impl ThemeHandle {
    pub fn new(theme: Theme) -> Self {
        Self(Arc::new(Mutex::new(State {
            theme,
            subscribers: Vec::new(),
        })))
    }

    pub fn snapshot(&self) -> Theme {
        self.0.lock().expect("status theme mutex poisoned").theme
    }

    /// Returns false for the current palette; callbacks run after releasing the lock.
    pub fn replace(&self, theme: Theme) -> bool {
        let repaint = {
            let mut state = self.0.lock().expect("status theme mutex poisoned");
            if state.theme == theme {
                return false;
            }
            state.theme = theme;
            let mut repaint = Vec::new();
            state.subscribers.retain(|subscriber| {
                if let Some(subscriber) = subscriber.upgrade() {
                    repaint.push(subscriber);
                    true
                } else {
                    false
                }
            });
            repaint
        };
        for callback in repaint {
            callback();
        }
        true
    }

    pub(crate) fn on_repaint(&self, repaint: Arc<Repaint>) -> Subscription {
        let mut state = self.0.lock().expect("status theme mutex poisoned");
        state
            .subscribers
            .retain(|subscriber| subscriber.strong_count() > 0);
        state.subscribers.push(Arc::downgrade(&repaint));
        Subscription { _repaint: repaint }
    }
}

#[derive(Clone)]
pub(crate) struct Subscription {
    _repaint: Arc<Repaint>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::ColorPair;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn replacement_notifies_live_editors_without_holding_the_theme_lock() {
        let handle = ThemeHandle::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let callback = |handle: ThemeHandle, calls: Arc<AtomicUsize>| {
            Arc::new(move || {
                let _ = handle.snapshot();
                calls.fetch_add(1, Ordering::SeqCst);
            }) as Arc<Repaint>
        };
        let first = handle.on_repaint(callback(handle.clone(), calls.clone()));
        let second = handle.on_repaint(callback(handle.clone(), calls.clone()));
        assert!(!handle.replace(Theme::default()));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let updated = Theme {
            operation: ColorPair::new([238, 243, 248], [30, 64, 83], 231, 24).unwrap(),
            ..Default::default()
        };
        assert!(handle.replace(updated));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(handle.snapshot(), updated);
        drop(first);
        assert!(handle.replace(Theme::default()));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        drop(second);
        assert!(handle.replace(updated));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
