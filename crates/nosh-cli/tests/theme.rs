#![cfg(unix)]

#[path = "theme/powerline.rs"]
mod powerline;
#[path = "theme/starship.rs"]
mod starship;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

const DARK: &str = "\x1b[0;38;2;17;17;27;48;2;250;179;135m";
const LIGHT: &str = "\x1b[0;38;2;17;17;27;48;2;254;100;11m";

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
    command
        .env("NOSH_HOME", home)
        .env("HOME", home)
        .env("NOSH_DISABLE_AI", "1")
        .env("NOSH_LANG", "en")
        .env("LC_ALL", "C.UTF-8")
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("NOSH_INTERNAL_PTY_FD");
    for name in [
        "TERM_PROGRAM",
        "KITTY_WINDOW_ID",
        "WT_SESSION",
        "TMUX",
        "STY",
        "ZELLIJ",
        "SSH_TTY",
        "SSH_CONNECTION",
        "SSH_CLIENT",
    ] {
        command.env_remove(name);
    }
    command
}

fn theme(home: &Path, args: &[&str]) -> Output {
    command(home).arg("theme").args(args).output().unwrap()
}

#[test]
fn management_works_without_ai_and_previews_never_save_or_emit_ansi_to_pipes() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("config.toml");
    let list = theme(home.path(), &["list"]);
    assert!(list.status.success());
    assert_eq!(
        String::from_utf8(list.stdout).unwrap(),
        "* soft-dark\n  soft-light\n"
    );
    let preview = theme(
        home.path(),
        &["preview", "soft-light", "--columns", "80", "--mode", "off"],
    );
    assert!(preview.status.success(), "{:?}", preview.stderr);
    assert!(!preview.stdout.contains(&0x1b));
    assert!(
        String::from_utf8(preview.stdout)
            .unwrap()
            .contains("not applicable")
    );
    assert!(!file.exists(), "list/preview must not create config");
    assert!(theme(home.path(), &["set", "soft-light"]).status.success());
    let saved = fs::read(&file).unwrap();
    assert!(
        theme(home.path(), &["preview", "soft-dark"])
            .status
            .success()
    );
    assert_eq!(fs::read(&file).unwrap(), saved);
    assert!(
        String::from_utf8(theme(home.path(), &["list"]).stdout)
            .unwrap()
            .contains("* soft-light")
    );
    assert!(!theme(home.path(), &["set", "classic"]).status.success());
    assert_eq!(fs::read(&file).unwrap(), saved);
}

#[test]
fn save_confirmation_uses_plain_language_in_both_languages() {
    for (language, message) in [
        ("zh", "已保存，无需重启"),
        ("en", "saved; no restart needed"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let result = command(home.path())
            .env("NOSH_LANG", language)
            .args(["theme", "set", "soft-light"])
            .output()
            .unwrap();
        assert!(result.status.success(), "{:?}", result.stderr);
        assert!(result.stdout.is_empty());
        assert_eq!(
            String::from_utf8(result.stderr).unwrap(),
            format!(
                "nosh: soft-light {message} ({})\n",
                home.path().join("config.toml").display()
            )
        );
    }
}

#[test]
fn config_path_diagnostics_cannot_inject_terminal_controls() {
    let parent = tempfile::tempdir().unwrap();
    let home = parent.path().join("theme-\x1b[31m");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("config.toml"), "broken [").unwrap();
    let result = theme(&home, &["list"]);
    assert!(!result.status.success());
    assert!(!result.stderr.contains(&0x1b));
    fs::write(home.join("config.toml"), "").unwrap();
    let result = theme(&home, &["set", "soft-light"]);
    assert!(result.status.success());
    assert!(!result.stderr.contains(&0x1b));
}

struct Shell {
    child: Child,
    terminal: File,
    bytes: Vec<u8>,
    queries: usize,
}

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Shell {
    fn new(home: &Path) -> Self {
        Self::with_path(home, None)
    }

    fn with_path(home: &Path, path: Option<&std::ffi::OsStr>) -> Self {
        let mut command = command(home);
        command.arg("--safe");
        if let Some(path) = path {
            command.env("PATH", path);
        }
        Self::start(home, command)
    }

    fn inline(home: &Path) -> Self {
        let mut command = command(home);
        command
            .env_remove("NOSH_DISABLE_AI")
            .args(["--offline", "--no-download", "--norc", "-i"]);
        Self::start(home, command)
    }

    fn start(home: &Path, mut command: Command) -> Self {
        let (mut master, mut slave) = (-1, -1);
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes both owned descriptors on success.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let (terminal, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        for fd in [terminal.as_raw_fd(), slave.as_raw_fd()] {
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        command
            .current_dir(home)
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        // SAFETY: the child only runs async-signal-safe terminal setup.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }

                Ok(())
            });
        }
        Self {
            child: command.spawn().unwrap(),
            terminal,
            bytes: Vec::new(),
            queries: 0,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.terminal.write_all(bytes).unwrap();
    }

    fn pump(&mut self, milliseconds: i32) {
        let mut poll = libc::pollfd {
            fd: self.terminal.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, milliseconds) } > 0 {
            let mut bytes = [0; 8192];
            let count = self.terminal.read(&mut bytes).unwrap();
            self.bytes.extend_from_slice(&bytes[..count]);
            let queries = self
                .bytes
                .windows(4)
                .filter(|bytes| *bytes == b"\x1b[6n")
                .count();
            for _ in self.queries..queries {
                self.send(b"\x1b[1;1R");
            }
            self.queries = queries;
        }
    }

    fn wait(&mut self, start: usize, text: &str) -> usize {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !String::from_utf8_lossy(&self.bytes[start..]).contains(text) {
            assert!(
                Instant::now() < deadline,
                "missing {text:?}: {}",
                String::from_utf8_lossy(&self.bytes)
            );
            self.pump(50);
        }
        start
            + self.bytes[start..]
                .windows(text.len())
                .position(|bytes| bytes == text.as_bytes())
                .expect("observed terminal text")
            + text.len()
    }
}

#[test]
fn live_cli_loads_saved_theme_at_next_prompt_without_executing_the_draft() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    fs::write(&config, "[shell]\ntheme = 'soft-dark'\ncapture_output = 'off'\ninput_assist = false\ncompletion = false\ncommand_assist = false\n").unwrap();
    let mut shell = Shell::new(home.path());
    shell.wait(0, DARK);
    shell.send(b"touch must_not_execute");
    shell.wait(0, "touch must_not_execute");
    assert!(theme(home.path(), &["set", "soft-light"]).status.success());
    let start = shell.bytes.len();
    for _ in 0..4 {
        shell.pump(50);
    }
    assert!(
        !String::from_utf8_lossy(&shell.bytes[start..]).contains(LIGHT),
        "a config write interrupted active editing"
    );
    shell.send(b"\x03");
    shell.wait(start, LIGHT);
    assert!(!home.path().join("must_not_execute").exists());
    fs::write(&config, "[shell]\ntheme = 'invalid'\n").unwrap();
    let start = shell.bytes.len();
    shell.send(b"\x03");
    shell.wait(start, "theme unchanged");
    shell.wait(start, LIGHT);
    assert!(!String::from_utf8_lossy(&shell.bytes[start..]).contains(DARK));
}

#[test]
fn prompt_command_metrics_are_real_optional_and_do_not_load_ai() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("config.toml"),
        "[shell]\ncapture_output='off'\ninput_assist=false\ncompletion=false\ncommand_assist=false\n",
    ).unwrap();
    let mut shell = Shell::new(home.path());
    shell.wait(0, DARK);
    assert!(!String::from_utf8_lossy(&shell.bytes).contains("ctx "));
    let start = shell.bytes.len();
    shell.send(b"(exit 7)\r");
    shell.wait(start, "last exit 7");
    let start = shell.bytes.len();
    shell.send(b"touch must_not_run");
    shell.wait(start, "touch must_not_run");
    let start = shell.bytes.len();
    shell.send(b"\x03");
    shell.wait(start, "last exit 7");
    shell.wait(start, "Ctrl+R history");
    assert!(!home.path().join("must_not_run").exists());
    let start = shell.bytes.len();
    shell.send(b"sleep 2.1; printf 'SLOW_DONE\\n'\r");
    let start = shell.wait(start, "SLOW_DONE\r\n");
    shell.wait(start, "last ");
    let start = shell.bytes.len();
    shell.send(b"printf 'METRICS_DONE\\n'\r");
    let start = shell.wait(start, "METRICS_DONE\r\n");
    shell.wait(start, DARK);
    shell.wait(start, "Ctrl+R history");
    assert!(!String::from_utf8_lossy(&shell.bytes[start..]).contains("last "));
    let output = String::from_utf8_lossy(&shell.bytes);
    assert!(
        !output.contains("Downloading") && !output.contains("not installed"),
        "{output}"
    );
}

#[test]
fn inline_theme_selection_saves_and_repaints_without_loading_a_model() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    let original = "[shell]\nai_prefix='?'\ntheme='soft-dark'\ncapture_output='off'\ninput_assist=false\ncompletion=false\ncommand_assist=false\n";
    fs::write(&config, original).unwrap();
    let mut shell = Shell::inline(home.path());
    shell.wait(0, DARK);
    let start = shell.bytes.len();
    shell.send(b"?theme\r");
    shell.wait(start, "theme: soft-dark");
    assert_eq!(fs::read_to_string(&config).unwrap(), original);

    let start = shell.bytes.len();
    shell.send(b"?theme soft-light\r");
    shell.wait(start, "soft-light saved");
    shell.wait(start, LIGHT);
    let saved = fs::read_to_string(&config).unwrap();
    let table: toml::Table = saved.parse().unwrap();
    assert_eq!(table["shell"]["theme"].as_str(), Some("soft-light"));

    let start = shell.bytes.len();
    shell.send(b"?theme\r");
    shell.wait(start, "theme: soft-light");
    for (input, error) in [
        (
            "?theme soft-dark extra\r",
            "invalid arguments; usage: ?theme",
        ),
        ("?theme ./missing.toml\r", "nosh: theme "),
        (
            "?theme $(touch must_not_execute)\r",
            "invalid arguments; usage: ?theme",
        ),
    ] {
        let start = shell.bytes.len();
        shell.send(input.as_bytes());
        shell.wait(start, error);
        shell.wait(start, LIGHT);
        assert_eq!(fs::read_to_string(&config).unwrap(), saved);
    }
    assert!(!home.path().join("must_not_execute").exists());

    fs::set_permissions(&config, fs::Permissions::from_mode(0o400)).unwrap();
    let start = shell.bytes.len();
    shell.send(b"?theme soft-dark\r");
    shell.wait(start, "read-only; selection was not saved");
    shell.wait(start, LIGHT);
    assert_eq!(fs::read_to_string(&config).unwrap(), saved);
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let output = String::from_utf8_lossy(&shell.bytes);
    assert!(
        !output.contains("not installed") && !output.contains("Downloading"),
        "{output}"
    );
}

#[test]
fn parameter_menus_open_without_tab_and_require_acceptance_before_switching() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    let original = "[shell]\nai_prefix='?'\ntheme='soft-dark'\ncapture_output='off'\ninput_assist=false\ncompletion=true\ncommand_assist=false\n";
    fs::write(&config, original).unwrap();
    let mut shell = Shell::inline(home.path());
    shell.wait(0, DARK);
    let start = shell.bytes.len();
    shell.send(b"?theme ");
    shell.wait(start, "Dark theme");
    shell.wait(start, "Light theme");
    shell.wait(start, "current");
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    let start = shell.bytes.len();
    shell.send(b"soft-l");
    shell.wait(start, "?theme soft-l");
    let start = shell.bytes.len();
    shell.send(b"\t\r");
    shell.wait(start, "?theme soft-light");
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    assert!(!String::from_utf8_lossy(&shell.bytes[start..]).contains("soft-light saved"));
    let start = shell.bytes.len();
    shell.send(b"\r");
    shell.wait(start, "soft-light saved");
    shell.wait(start, LIGHT);

    let start = shell.bytes.len();
    shell.send(b"?theme ");
    shell.wait(start, "current · Light theme");
    let start = shell.bytes.len();
    shell.send(b"\x03");
    shell.wait(start, LIGHT);
    let start = shell.bytes.len();
    shell.send(b"?mode ");
    shell.wait(start, "current · Apply safety rules");
    shell.wait(start, "Ask for approval");
    shell.wait(start, "high risk");
    let start = shell.bytes.len();
    shell.send(b"\t\r");
    shell.wait(start, "?mode confirm");
    assert!(!String::from_utf8_lossy(&shell.bytes[start..]).contains("Approval: Confirm"));
    let start = shell.bytes.len();
    shell.send(b"\r");
    shell.wait(start, "Approval: Confirm");
    let start = shell.bytes.len();
    shell.send(b"?mode ");
    shell.wait(start, "current · Ask for approval");
    let start = shell.bytes.len();
    shell.send(b"\x03");
    shell.wait(start, LIGHT);
    let start = shell.bytes.len();
    shell.send(b"?theme\r");
    shell.wait(start, "theme: soft-light");
    let start = shell.bytes.len();
    shell.send(b"?th\t");
    shell.wait(start, "show or save the theme selection");
    let saved = fs::read_to_string(&config).unwrap();
    let start = shell.bytes.len();
    shell.send(b"\r");
    shell.wait(start, "Light theme");
    assert_eq!(fs::read_to_string(&config).unwrap(), saved);
    assert!(!String::from_utf8_lossy(&shell.bytes[start..]).contains("theme: soft-light"));
    let start = shell.bytes.len();
    shell.send(b"\x1b");
    shell.wait(start, "Tab");
    shell.send(b"\r");
    shell.wait(start, "theme: soft-light");
    let output = String::from_utf8_lossy(&shell.bytes);
    assert!(
        !output.contains("not installed") && !output.contains("Downloading"),
        "{output}"
    );
}

#[test]
fn parameter_menus_rearm_after_deleting_out_of_the_argument() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    let original = "[shell]\nai_prefix='?'\ntheme='soft-dark'\ncapture_output='off'\ninput_assist=false\ncompletion=true\ncommand_assist=false\n";
    fs::write(&config, original).unwrap();
    let mut shell = Shell::inline(home.path());
    shell.wait(0, DARK);
    let start = shell.bytes.len();
    shell.send(b"?theme ");
    shell.wait(start, "Light theme");

    let start = shell.bytes.len();
    shell.send(b"\x1b");
    shell.wait(start, "Tab");
    let start = shell.bytes.len();
    shell.send(b"\x7f");
    shell.wait(start, "?theme");
    let start = shell.bytes.len();
    shell.send(b" ");
    shell.wait(start, "Light theme");

    let start = shell.bytes.len();
    shell.send(b"\x15");
    shell.wait(start, "❯ ");
    let start = shell.bytes.len();
    shell.send(b"?theme ");
    shell.wait(start, "Light theme");

    let start = shell.bytes.len();
    shell.send(b"\x1b");
    shell.wait(start, "Tab");
    let start = shell.bytes.len();
    shell.send(b"soft-light");
    shell.wait(start, "?theme soft-light");
    assert!(!String::from_utf8_lossy(&shell.bytes[start..]).contains("Light theme"));
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    let start = shell.bytes.len();
    shell.send(b"\r");
    shell.wait(start, "soft-light saved");
    shell.wait(start, LIGHT);
}
