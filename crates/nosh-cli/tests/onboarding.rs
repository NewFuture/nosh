#![cfg(any(unix, windows))]

use std::fs::{self, File};
use std::path::Path;
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};

use nosh_hub::Registry;

#[cfg(unix)]
#[path = "support/terminal.rs"]
mod terminal;
#[cfg(windows)]
#[path = "support/windows.rs"]
mod terminal;

const CONFIG: &str = "\
[shell]
input_assist = false
completion = false
command_assist = false
capture_output = 'off'
[download]
source_selection = 'hf'
";

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
    command
        .current_dir(home)
        .env("NOSH_HOME", home)
        .env("NOSH_LANG", "en")
        .env("NOSH_OFFLINE", "0")
        .env("HF_HUB_OFFLINE", "0")
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .env("PS1", "NATIVE> ")
        .env("PROMPT_COMMAND", "printf 'NOSH-TEST-PROMPT\\n'")
        .env_remove("NOSH_MODEL")
        .env_remove("NOSH_MODEL_PATH")
        .env_remove("NOSH_DISABLE_AI")
        .env_remove("NOSH_EVAL_WORKER")
        .env_remove("NOSH_EVAL_TRACE")
        .env_remove("NOSH_INTERNAL_PTY_FD")
        .args(["--norc", "--device", "cpu"]);
    command
}

fn spawn(command: Command) -> terminal::Terminal {
    #[cfg(unix)]
    {
        terminal::Terminal::new(command, 30, 100)
    }
    #[cfg(windows)]
    {
        terminal::Terminal::spawn(command)
    }
}

fn wait(terminal: &mut terminal::Terminal, needle: &str) {
    #[cfg(unix)]
    terminal.wait_for(needle);
    #[cfg(windows)]
    terminal.expect(needle);
}

fn transcript(terminal: &terminal::Terminal) -> String {
    #[cfg(unix)]
    {
        String::from_utf8_lossy(&terminal.output).into_owned()
    }
    #[cfg(windows)]
    {
        terminal.text()
    }
}

fn prompt(terminal: &mut terminal::Terminal) {
    #[cfg(unix)]
    {
        wait(terminal, "NOSH-TEST-PROMPT");
        wait(terminal, "NATIVE>");
    }
    #[cfg(windows)]
    terminal.wait_for_prompt();
}

fn finish(terminal: &mut terminal::Terminal) {
    terminal.send(b"exit 0\r");
    #[cfg(windows)]
    terminal.finish();
    #[cfg(unix)]
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = terminal.poll() {
                assert!(status.success(), "{}", transcript(terminal));
                break;
            }
            assert!(Instant::now() < deadline, "{}", transcript(terminal));
            terminal.pump(10);
        }
    }
}

fn lock_downloads(home: &Path, registry: &Registry) -> Vec<File> {
    registry
        .models
        .iter()
        .map(|entry| {
            let directory = home.join("models").join(entry.dir_name());
            fs::create_dir_all(&directory).unwrap();
            let file = File::create(directory.join("tokenizer.json.lock")).unwrap();
            file.try_lock().unwrap();
            file
        })
        .collect()
}

fn selected_model(home: &Path) -> Option<String> {
    fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .parse::<toml::Table>()
        .unwrap()
        .get("model")
        .and_then(|model| model.get("id"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}

#[test]
fn enter_selects_the_recommendation_then_shell_runs_during_background_installation() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("config.toml"), CONFIG).unwrap();
    let registry = Registry::builtin();
    // Hold every tokenizer lock so this exercises startup without downloading real models.
    let _locks = lock_downloads(home.path(), &registry);
    let mut terminal = spawn(command(home.path()));
    wait(&mut terminal, "Model [Up/Down");
    let output = transcript(&terminal);
    let line = output
        .lines()
        .find(|line| line.contains("[recommended]"))
        .unwrap();
    let recommended = registry
        .models
        .iter()
        .find(|entry| line.contains(&entry.id))
        .unwrap();
    assert!(!output.contains("Continue?"), "{output}");
    wait(&mut terminal, &recommended.id);
    terminal.send(b"\r");
    prompt(&mut terminal);
    if !transcript(&terminal).contains("AI: downloading") {
        wait(&mut terminal, "AI: downloading");
    }
    assert_eq!(
        selected_model(home.path()).as_deref(),
        Some(recommended.id.as_str())
    );
    terminal.send(b"printf 'SHELL-%s\\n' WORKS\r");
    wait(&mut terminal, "SHELL-WORKS");
    prompt(&mut terminal);
    terminal.send(b"echo not-submitted");
    #[cfg(unix)]
    {
        terminal.send(b"\x03");
        prompt(&mut terminal);
    }
    #[cfg(windows)]
    {
        terminal.control_c();
        wait(&mut terminal, "NATIVE>");
    }
    terminal.send(b"#status\r");
    wait(&mut terminal, "model: AI: downloading");
    wait(&mut terminal, "waiting for it");
    prompt(&mut terminal);
    finish(&mut terminal);
    for entry in &registry.models {
        assert!(
            !home
                .path()
                .join("models")
                .join(entry.dir_name())
                .join("tokenizer.json.partial")
                .exists()
        );
    }
}

#[test]
fn arrows_and_numbers_select_another_model_instead_of_the_default() {
    let registry = Registry::builtin();
    for (keys, expected) in [(b"\x1b[B".as_slice(), 1), (b"3".as_slice(), 2)] {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), CONFIG).unwrap();
        let _locks = lock_downloads(home.path(), &registry);
        let mut command = command(home.path());
        command.args(["--model", &registry.models[0].id]);
        let mut terminal = spawn(command);
        wait(&mut terminal, "Model [Up/Down");
        terminal.send(keys);
        wait(&mut terminal, &registry.models[expected].id);
        terminal.send(b"\r");
        prompt(&mut terminal);
        assert_eq!(
            selected_model(home.path()).as_deref(),
            Some(registry.models[expected].id.as_str())
        );
        finish(&mut terminal);
    }
}

#[test]
fn escape_and_skip_enter_the_shell_without_installing_or_prompting_again() {
    let skip = format!("{}\r", Registry::builtin().models.len() + 1);
    for keys in [b"\x1b".as_slice(), skip.as_bytes()] {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), CONFIG).unwrap();
        let mut terminal = spawn(command(home.path()));
        wait(&mut terminal, "Model [Up/Down");
        terminal.send(keys);
        prompt(&mut terminal);
        assert!(
            home.path()
                .join("state")
                .join("download-declined")
                .is_file()
        );
        assert!(selected_model(home.path()).is_none());
        assert!(!home.path().join("models").exists());
        finish(&mut terminal);

        let mut terminal = spawn(command(home.path()));
        prompt(&mut terminal);
        assert!(!transcript(&terminal).contains("Choose a model"));
        finish(&mut terminal);
    }
}

#[test]
#[cfg(windows)]
fn control_c_skips_model_selection_without_installing() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("config.toml"), CONFIG).unwrap();
    let mut terminal = spawn(command(home.path()));
    wait(&mut terminal, "Model [Up/Down");
    terminal.control_c();
    wait(&mut terminal, "NATIVE>");
    assert!(
        home.path()
            .join("state")
            .join("download-declined")
            .is_file()
    );
    assert!(selected_model(home.path()).is_none());
    assert!(!home.path().join("models").exists());
    finish(&mut terminal);
}

#[test]
fn offline_disabled_downloads_and_invalid_paths_never_offer_installation() {
    for args in [
        vec!["--offline"],
        vec!["--no-download"],
        vec!["--model-path", "missing.gguf"],
    ] {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), CONFIG).unwrap();
        let mut command = command(home.path());
        command.args(args);
        let mut terminal = spawn(command);
        prompt(&mut terminal);
        assert!(!transcript(&terminal).contains("Choose a model"));
        assert!(selected_model(home.path()).is_none());
        assert!(!home.path().join("models").exists());
        finish(&mut terminal);
    }
}
