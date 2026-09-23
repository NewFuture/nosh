//! `nosh -a` (one-shot agent task) and `nosh -s` (print one suggested command).

use std::io::{IsTerminal, Read};
use std::time::Duration;

use nosh_core::{
    Agent, AgentConfig, AgentUi, Attachment, Environment, JsonUi, TaskInput, TermUi,
    TerminalApproval, ToolSet,
};
use nosh_hub::tr;
use nosh_permissions::{ApprovalMode, UserRules};
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

/// Attached stdin, capped at 1 MiB.
fn read_stdin() -> Option<Vec<u8>> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut buf = Vec::new();
    let _ = std::io::stdin().take(1 << 20).read_to_end(&mut buf);
    (!buf.is_empty()).then_some(buf)
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
    let task = words.join(" ");
    let stdin = read_stdin();
    if task.trim().is_empty() && stdin.is_none() {
        eprintln!("usage: nosh -a \"task\"  (stdin, if piped, is attached)");
        return 2;
    }
    let mut shell = match open_shell() {
        Ok(s) => s,
        Err(c) => return c,
    };
    let loaded = match engine::load(setup, true) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("nosh: {e}");
            return if shell.interrupts().count() > 0 {
                130
            } else {
                2
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
        attachment: stdin.map(|b| Attachment::from_bytes("stdin", &b)),
    };
    let env = Environment::detect(&shell);
    let mut agent = Agent::new(loaded.engine, agent_config(cfg, mode, seed), env, tools);
    let mut approval = TerminalApproval::detect();
    let mut term_ui;
    let mut json_ui;
    let ui: &mut dyn AgentUi = if json {
        json_ui = JsonUi;
        &mut json_ui
    } else {
        term_ui = TermUi::new(true);
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
    if text.trim().is_empty()
        && let Some(b) = read_stdin()
    {
        text = String::from_utf8_lossy(&b).trim().to_string();
    }
    if text.trim().is_empty() {
        eprintln!("usage: nosh -s \"description\"");
        return 2;
    }
    let shell = match open_shell() {
        Ok(s) => s,
        Err(c) => return c,
    };
    let mut loaded = match engine::load(setup, true) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("nosh: {e}");
            return if shell.interrupts().count() > 0 {
                130
            } else {
                2
            };
        }
    };
    if shell.interrupts().count() > 0 {
        return 130;
    }
    let cancel = loaded.engine.cancel_handle();
    shell.interrupts().on_interrupt(move || cancel.cancel());
    let env = Environment::detect(&shell);
    let sampling = agent_config(cfg, ApprovalMode::Confirm, seed).sampling;
    let r = nosh_core::suggest::suggest(
        loaded.engine.as_mut(),
        &env,
        &shell,
        &text,
        Trigger::Cli,
        sampling,
    );
    if shell.interrupts().count() > 0 {
        return 130;
    }
    match r {
        Ok(Some(s)) => {
            println!("{}", s.command);
            if let Some(e) = s.explanation.filter(|e| !e.trim().is_empty()) {
                eprintln!("{}", e.trim());
            }
            0
        }
        Ok(None) => {
            eprintln!("{}", tr!("nosh: 没有建议", "nosh: no suggestion"));
            1
        }
        Err(e) => {
            eprintln!("nosh: {e}");
            2
        }
    }
}
