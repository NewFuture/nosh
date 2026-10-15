//! `nosh -a` (one-shot agent task) and `nosh -s` (print one suggested command).

use std::io::{IsTerminal, Read};
use std::time::Duration;

use nosh_core::{
    Agent, AgentConfig, AgentUi, Attachment, Environment, JsonUi, TaskInput, TermUi,
    TerminalApproval, ToolSet,
};
use nosh_permissions::{ApprovalMode, UserRules};
use nosh_platform::tr;
use nosh_shell::{EmbeddedShell, ShellOptions, Trigger};

use crate::config::Config;
use crate::engine::{self, EngineSetup};

pub fn agent_config(cfg: &Config, mode: ApprovalMode, seed: Option<u64>) -> AgentConfig {
    let mut ac = AgentConfig {
        mode,
        rules: UserRules {
            allow: cfg.allow.clone(),
            deny: cfg.deny.clone(),
        },
        rules_error: cfg.safety_error.clone(),
        protected: cfg.protected_paths.clone(),
        max_steps: cfg.max_steps,
        command_timeout: Duration::from_secs(cfg.command_timeout_sec),
        thinking: cfg.thinking,
        restore_cwd: cfg.restore_cwd,
        idle_reset: Duration::from_secs(cfg.conversation_idle_minutes * 60),
        ..AgentConfig::default()
    };
    ac.sampling.seed = seed;
    ac
}

const STDIN_BYTES: u64 = 1 << 20;

fn read_piped_input(input: impl Read) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    input.take(STDIN_BYTES).read_to_end(&mut buf)?;
    Ok((!buf.is_empty()).then_some(buf))
}

/// Attached stdin, capped at 1 MiB.
fn read_stdin() -> Result<Option<Vec<u8>>, i32> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }
    read_piped_input(stdin.lock()).map_err(|error| {
        eprintln!("nosh: cannot read stdin: {error}");
        2
    })
}

fn open_shell() -> Result<EmbeddedShell, i32> {
    let shell = EmbeddedShell::new(ShellOptions {
        catch_sigint: true,
        ..ShellOptions::default()
    })
    .map_err(|e| {
        eprintln!("nosh: {e}");
        2
    })?;
    // Ctrl-C stops a model download (the partial file is kept).
    shell.interrupts().on_interrupt(nosh_hub::net::cancel);
    Ok(shell)
}

pub fn run_agent(
    words: &[String],
    json: bool,
    cfg: &Config,
    mode: ApprovalMode,
    setup: &EngineSetup,
    seed: Option<u64>,
) -> i32 {
    if let Some(error) = &cfg.safety_error {
        eprintln!("nosh: AI execution blocked by invalid safety configuration: {error}");
        return 2;
    }
    let task = words.join(" ");
    let stdin = match read_stdin() {
        Ok(input) => input,
        Err(code) => return code,
    };
    if task.trim().is_empty() && stdin.is_none() {
        eprintln!("usage: nosh -a \"task\"  (stdin, if piped, is attached)");
        return 2;
    }
    let mut shell = match open_shell() {
        Ok(s) => s,
        Err(c) => return c,
    };
    if stdin.is_none() {
        shell.configure_project_env(cfg.project_env.clone());
        if shell.refresh_project_env().is_err() {
            return if shell.interrupts().count() > 0 {
                130
            } else {
                2
            };
        }
    }
    let loaded = match engine::load(setup, nosh_core::LoadMode::Foreground) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("nosh: {e}");
            return if shell.interrupts().count() > 0 {
                130
            } else {
                e.exit_code()
            };
        }
    };
    if shell.interrupts().count() > 0 {
        return 130;
    }
    let (trigger, tools) = if stdin.is_some() {
        (Trigger::Pipe, ToolSet::ReadOnly)
    } else {
        (Trigger::Cli, ToolSet::Full)
    };
    let text = if task.trim().is_empty() {
        tr!("总结附件内容", "Summarize the attachment").to_string()
    } else {
        task
    };
    let input = TaskInput {
        trigger,
        text,
        failed: None,
        user_output: None,
        attachment: stdin.map(|b| Attachment::from_bytes("stdin", &b)),
    };
    let env = Environment::detect(&shell);
    let themes = nosh_shell::theme::ThemeHandle::new(cfg.theme.palette);
    let mut agent = Agent::new(loaded.engine, agent_config(cfg, mode, seed), env, tools)
        .with_user_input(Box::new(
            nosh_core::user_input::TerminalUserInput.with_theme(themes.clone()),
        ));
    let mut approval = TerminalApproval::detect_with_theme(themes.clone());
    let mut term_ui;
    let mut json_ui;
    let ui: &mut dyn AgentUi = if json {
        json_ui = JsonUi;
        &mut json_ui
    } else {
        term_ui = TermUi::with_theme(true, themes);
        term_ui.show_think = cfg.thinking;
        &mut term_ui
    };
    let out = agent.run_task(&mut shell, input, approval.as_mut(), ui);
    if let Some(cmd) = &out.proposed
        && !json
        && !std::io::stdout().is_terminal()
    {
        println!("{cmd}");
    }
    out.status.exit_code()
}

pub fn run_suggest(words: &[String], cfg: &Config, setup: &EngineSetup, seed: Option<u64>) -> i32 {
    let mut text = words.join(" ");
    if text.trim().is_empty() {
        match read_stdin() {
            Ok(Some(bytes)) => text = String::from_utf8_lossy(&bytes).trim().to_string(),
            Ok(None) => {}
            Err(code) => return code,
        }
    }
    if text.trim().is_empty() {
        eprintln!("usage: nosh -s \"description\"");
        return 2;
    }
    let shell = match open_shell() {
        Ok(s) => s,
        Err(c) => return c,
    };
    let mut loaded = match engine::load(setup, nosh_core::LoadMode::Foreground) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("nosh: {e}");
            return if shell.interrupts().count() > 0 {
                130
            } else {
                e.exit_code()
            };
        }
    };
    if shell.interrupts().count() > 0 {
        return 130;
    }
    let cancel = loaded.engine.cancel_handle();
    shell.interrupts().on_interrupt(move || cancel.cancel());
    let config = agent_config(cfg, ApprovalMode::Confirm, seed);
    let r = nosh_core::command_assist::generate(loaded.engine.as_mut(), &shell, &text, &config);
    if shell.interrupts().count() > 0 {
        return 130;
    }
    use nosh_core::command_assist::{AssistError, AssistResult};
    match r.map(|outcome| outcome.result) {
        Ok(AssistResult::Command(command)) => {
            println!("{command}");
            0
        }
        Ok(AssistResult::NoSuggestion) => {
            eprintln!("{}", tr!("nosh: 没有建议", "nosh: no suggestion"));
            1
        }
        Err(e) => {
            eprintln!("nosh: {e}");
            if matches!(e, AssistError::Cancelled) {
                130
            } else {
                2
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piped_input_preserves_bytes_and_empty_input() {
        assert_eq!(read_piped_input(&b""[..]).unwrap(), None);
        assert_eq!(
            read_piped_input(&b"raw\0bytes"[..]).unwrap(),
            Some(b"raw\0bytes".to_vec())
        );
    }

    #[test]
    fn piped_input_stops_at_the_existing_byte_limit() {
        let mut input = std::io::Cursor::new(vec![b'x'; STDIN_BYTES as usize + 1]);
        let bytes = read_piped_input(&mut input).unwrap().unwrap();
        assert_eq!(bytes, vec![b'x'; STDIN_BYTES as usize]);
        assert_eq!(input.position(), STDIN_BYTES);
    }

    #[test]
    fn piped_input_does_not_turn_read_errors_into_empty_or_partial_input() {
        struct FailedReader;
        impl Read for FailedReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "input read failed",
                ))
            }
        }
        for prefix in [&b""[..], &b"partial input"[..]] {
            let error = read_piped_input(prefix.chain(FailedReader)).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(error.to_string(), "input read failed");
        }
    }
}
