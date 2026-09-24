//! Download progress reporting.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

pub trait Progress: Send + Sync {
    /// A file transfer begins; `already` bytes are present from a previous attempt.
    fn start(&self, name: &str, total: u64, already: u64);
    /// Absolute position within the current file.
    fn advance(&self, pos: u64);
    /// Human-readable status line (source switch, verification, …).
    fn note(&self, msg: &str);
    fn finish(&self, ok: bool);
}

pub struct NoProgress;

impl Progress for NoProgress {
    fn start(&self, _: &str, _: u64, _: u64) {}
    fn advance(&self, _: u64) {}
    fn note(&self, _: &str) {}
    fn finish(&self, _: bool) {}
}

/// Terminal progress bar on stderr (hidden automatically when stderr is not a TTY).
#[derive(Default)]
pub struct BarProgress {
    bar: Mutex<Option<ProgressBar>>,
}

impl BarProgress {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Progress for BarProgress {
    fn start(&self, name: &str, total: u64, already: u64) {
        let bar = ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::stderr());
        let tpl = if crate::lang::zh() {
            "{msg:24!} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} 剩余 {eta}"
        } else {
            "{msg:24!} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}"
        };
        bar.set_style(
            ProgressStyle::with_template(tpl)
                .expect("valid template")
                .progress_chars("█▌ "),
        );
        bar.set_message(name.to_string());
        bar.set_position(already);
        bar.enable_steady_tick(Duration::from_millis(250));
        *self.bar.lock().unwrap() = Some(bar);
    }

    fn advance(&self, pos: u64) {
        if let Some(b) = self.bar.lock().unwrap().as_ref() {
            b.set_position(pos);
        }
    }

    fn note(&self, msg: &str) {
        match self.bar.lock().unwrap().as_ref() {
            Some(b) => b.println(msg),
            None => eprintln!("{msg}"),
        }
    }

    fn finish(&self, ok: bool) {
        if let Some(b) = self.bar.lock().unwrap().take() {
            if ok {
                b.finish();
            } else {
                b.abandon();
            }
        }
    }
}

/// Snapshot of a background download, e.g. for a shell prompt.
#[derive(Debug, Clone, Default)]
pub struct DownloadStatus {
    pub file: String,
    pub total: u64,
    pub pos: u64,
    pub last_note: String,
    pub done: bool,
    pub failed: bool,
}

impl DownloadStatus {
    pub fn percent(&self) -> u32 {
        if self.total == 0 {
            0
        } else {
            ((self.pos as f64 / self.total as f64) * 100.0).min(100.0) as u32
        }
    }
}

/// Progress sink that records state for polling from another thread.
#[derive(Clone, Default)]
pub struct SharedProgress {
    pub state: Arc<Mutex<DownloadStatus>>,
}

impl Progress for SharedProgress {
    /// A new transfer: earlier completion or failure no longer applies.
    fn start(&self, name: &str, total: u64, already: u64) {
        let mut s = self.state.lock().unwrap();
        *s = DownloadStatus {
            file: name.to_string(),
            total,
            pos: already,
            last_note: std::mem::take(&mut s.last_note),
            done: false,
            failed: false,
        };
    }
    fn advance(&self, pos: u64) {
        self.state.lock().unwrap().pos = pos;
    }
    fn note(&self, msg: &str) {
        self.state.lock().unwrap().last_note = msg.to_string();
    }
    /// The transfer ended: `done` either way, `failed` unless it succeeded.
    fn finish(&self, ok: bool) {
        let mut s = self.state.lock().unwrap();
        s.done = true;
        s.failed = !ok;
        if ok {
            s.pos = s.total;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_progress_records_each_transfer_lifecycle() {
        let p = SharedProgress::default();
        let snap = || p.state.lock().unwrap().clone();
        p.start("tokenizer.json", 100, 0);
        p.advance(40);
        assert_eq!(snap().percent(), 40);
        assert!(!snap().done);
        p.finish(true);
        let s = snap();
        assert!(s.done && !s.failed, "{s:?}");
        assert_eq!(s.percent(), 100);
        // The next file starts clean, and a failure is also completion.
        p.note("switching to modelscope.cn");
        p.start("model.gguf", 1000, 250);
        let s = snap();
        assert!(!s.done && !s.failed, "{s:?}");
        assert_eq!((s.file.as_str(), s.pos, s.total), ("model.gguf", 250, 1000));
        assert_eq!(s.last_note, "switching to modelscope.cn");
        p.finish(false);
        let s = snap();
        assert!(s.done && s.failed, "{s:?}");
        assert_eq!(s.pos, 250, "a failed transfer keeps its position");
        p.start("model.gguf", 1000, 250);
        assert!(!snap().failed, "a retry clears the failure");
    }
}
