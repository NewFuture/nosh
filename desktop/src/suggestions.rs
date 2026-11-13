use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::connections::suggestion_args;

const OUTPUT_LIMIT: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(180);

pub struct SuggestionJob {
    cancel: Arc<AtomicBool>,
    receiver: Receiver<Result<String, String>>,
    worker: Option<JoinHandle<()>>,
}

impl SuggestionJob {
    pub fn start(
        nosh: &Path,
        folder: &Path,
        description: &str,
        context: eframe::egui::Context,
    ) -> io::Result<Self> {
        let mut command = std::process::Command::new(nosh);
        command
            .args(suggestion_args(description))
            .current_dir(folder);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("desktop-suggest".into())
            .spawn(move || {
                let result = run(&mut command, &worker_cancel, TIMEOUT);
                if sender.send(result).is_err() {
                    eprintln!("nosh-desktop: suggestion result receiver was closed");
                }
                context.request_repaint();
            })?;
        Ok(Self {
            cancel,
            receiver,
            worker: Some(worker),
        })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn poll(&self) -> Option<Result<String, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err("Suggestion worker stopped without a result.".into()))
            }
        }
    }
}

impl Drop for SuggestionJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("nosh-desktop: suggestion worker panicked");
        }
    }
}

fn run(
    command: &mut std::process::Command,
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<String, String> {
    if cancel.load(Ordering::SeqCst) {
        return Err("Suggestion cancelled.".into());
    }
    if timeout.is_zero() {
        return Err("Suggestion timed out.".into());
    }
    let output = capture(command, cancel, timeout).map_err(|error| error.to_string())?;
    if !output.complete {
        return Err("Suggestion output exceeded 64 KiB; the process was stopped.".into());
    }
    if !output.success {
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "nosh suggestion failed ({}):\n{diagnostic}",
            output.status
        ));
    }
    let command = String::from_utf8(output.stdout)
        .map_err(|error| format!("Invalid UTF-8 suggestion: {error}"))?;
    let command = command.trim();
    if command.is_empty() {
        return Err("nosh returned an empty suggestion.".into());
    }
    Ok(command.to_owned())
}

struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    success: bool,
    status: String,
    complete: bool,
}

#[cfg(windows)]
fn capture(
    command: &mut std::process::Command,
    cancel: &AtomicBool,
    timeout: Duration,
) -> io::Result<Captured> {
    let output = nosh_platform::process::capture(command, timeout, OUTPUT_LIMIT, || {
        cancel.load(Ordering::SeqCst)
    })?;
    Ok(Captured {
        success: output.status.is_some_and(|status| status.success()),
        status: output
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "incomplete".into()),
        stdout: output.stdout,
        stderr: output.stderr,
        complete: output.complete,
    })
}

#[cfg(unix)]
fn capture(
    command: &mut std::process::Command,
    cancel: &AtomicBool,
    timeout: Duration,
) -> io::Result<Captured> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::Instant;

    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let result = (|| {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing stderr"))?;
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // SAFETY: both owned pipe descriptors remain valid throughout capture.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let start = Instant::now();
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Suggestion cancelled.",
                ));
            }
            if start.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Suggestion timed out.",
                ));
            }
            let mut buffer = [0; 4096];
            let mut done = 0;
            let mut remaining = OUTPUT_LIMIT - out.len() - err.len();
            let mut exceeded = false;
            for (pipe, bytes) in [
                (&mut stdout as &mut dyn Read, &mut out),
                (&mut stderr as &mut dyn Read, &mut err),
            ] {
                match pipe.read(&mut buffer) {
                    Ok(0) => done += 1,
                    Ok(n) => {
                        let keep = n.min(remaining);
                        bytes.extend_from_slice(&buffer[..keep]);
                        remaining -= keep;
                        if keep != n {
                            exceeded = true;
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error),
                }
            }
            if exceeded {
                return Ok(Captured {
                    stdout: out,
                    stderr: err,
                    success: false,
                    status: "incomplete".into(),
                    complete: false,
                });
            }
            if done == 2
                && let Some(status) = child.try_wait()?
            {
                return Ok(Captured {
                    stdout: out,
                    stderr: err,
                    success: status.success(),
                    status: status.to_string(),
                    complete: true,
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // SAFETY: a negative PID addresses only this child's newly created process group.
    let killed = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let cleanup = if killed < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };
    let waited = child.wait();
    cleanup?;
    waited?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> std::process::Command {
        #[cfg(windows)]
        let mut command = std::process::Command::new("cmd.exe");
        #[cfg(windows)]
        command.args(["/d", "/c", script]);
        #[cfg(unix)]
        let mut command = std::process::Command::new("/bin/sh");
        #[cfg(unix)]
        command.args(["-c", script]);
        command
    }

    #[test]
    fn captures_success_and_reports_failure_not_a_success_shaped_fallback() {
        let cancel = AtomicBool::new(false);
        assert_eq!(
            run(&mut shell("echo hello"), &cancel, Duration::from_secs(5)).unwrap(),
            "hello"
        );
        let error = run(
            &mut shell("echo failed 1>&2 & exit 7"),
            &cancel,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.contains("failed"), "{error}");
        assert!(error.contains('7'), "{error}");
    }

    #[test]
    fn cancellation_and_timeout_are_errors() {
        let cancel = AtomicBool::new(true);
        assert!(run(&mut shell("echo hello"), &cancel, Duration::from_secs(1)).is_err());
        #[cfg(windows)]
        let script = "ping -n 10 127.0.0.1 >nul";
        #[cfg(unix)]
        let script = "sleep 10";
        let start = std::time::Instant::now();
        assert!(
            run(
                &mut shell(script),
                &AtomicBool::new(false),
                Duration::from_millis(50)
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn cancels_an_already_running_request_and_rejects_truncated_output() {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            worker_cancel.store(true, Ordering::SeqCst);
        });
        #[cfg(windows)]
        let script = "ping -n 10 127.0.0.1 >nul";
        #[cfg(unix)]
        let script = "sleep 10";
        let start = std::time::Instant::now();
        let error = run(&mut shell(script), &cancel, Duration::from_secs(5)).unwrap_err();
        trigger.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(error.to_lowercase().contains("cancel"), "{error}");

        #[cfg(windows)]
        let script = "for /L %i in (1,1,7000) do @echo DESKTOP_OVERFLOW";
        #[cfg(unix)]
        let script = "i=0; while [ $i -lt 7000 ]; do printf DESKTOP_OVERFLOW; i=$((i + 1)); done";
        let error = run(
            &mut shell(script),
            &AtomicBool::new(false),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.contains("64 KiB"), "{error}");
    }
}
