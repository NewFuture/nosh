//! `nosh` entry point: dispatches between the interactive shell, `-c`/script
//! execution, one-shot agent tasks (`-a`), suggestions (`-s`) and management
//! subcommands.

mod agent_cmd;
mod config;
mod debug_cmd;
mod doctor;
mod engine;
mod eval_trace;
mod model_cmd;
mod shell_cmd;

use clap::{Parser, Subcommand};

#[cfg(target_os = "macos")]
#[global_allocator]
static INPUT_WORKER_ALLOCATOR: nosh_shell::input_assist::WorkerAllocator =
    nosh_shell::input_assist::WorkerAllocator;

#[derive(Debug, Parser)]
#[command(
    name = "nosh",
    version,
    about = "nosh: a bash-compatible AI shell with an offline local LLM",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Run a command string (bash `-c`); no model, no extra output.
    #[arg(short = 'c', value_name = "COMMAND")]
    command: Option<String>,

    /// One-shot agent task; stdin (if piped) is attached as context.
    #[arg(short = 'a', long = "agent")]
    agent: bool,

    /// Print a single suggested command for the description.
    #[arg(short = 's', long = "suggest")]
    suggest: bool,

    /// Login shell.
    #[arg(short = 'l', long = "login")]
    login: bool,

    /// Force an interactive shell.
    #[arg(short = 'i')]
    interactive: bool,

    /// Exit on the first failing command (bash -e).
    #[arg(short = 'e')]
    errexit: bool,

    /// Trace commands (bash -x).
    #[arg(short = 'x')]
    xtrace: bool,

    /// Treat unset variables as errors (bash -u).
    #[arg(short = 'u')]
    nounset: bool,

    /// Emit JSON Lines events (with -a).
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    global: GlobalOpts,

    /// Script path and arguments, or the task/description words for -a/-s.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    rest: Vec<String>,
}

#[derive(Debug, clap::Args)]
struct GlobalOpts {
    /// Never touch the network.
    #[arg(long, global = true)]
    offline: bool,
    /// Use this GGUF file (or directory) instead of the model store.
    #[arg(long, global = true, value_name = "PATH")]
    model_path: Option<std::path::PathBuf>,
    /// Model id from the registry.
    #[arg(long, global = true, value_name = "ID")]
    model: Option<String>,
    /// Do not download a missing model.
    #[arg(long, global = true)]
    no_download: bool,
    /// Do not load ~/.bashrc.
    #[arg(long, global = true)]
    norc: bool,
    /// No rc files and no AI.
    #[arg(long, global = true)]
    safe: bool,
    /// Auto approval (default): low-impact operations and common builds/tests.
    #[arg(long, global = true, conflicts_with = "yolo")]
    auto: bool,
    /// YOLO: no per-call approval for non-prohibited operations; user deny still wins.
    #[arg(long, global = true)]
    yolo: bool,
    /// Sampling seed for reproducible generations.
    #[arg(long, global = true)]
    seed: Option<u64>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Manage local models.
    Model {
        #[command(subcommand)]
        cmd: model_cmd::ModelCmd,
    },
    /// Check CPU, memory, model, download sources and configuration.
    Doctor,
    /// Developer utilities.
    Debug {
        #[command(subcommand)]
        cmd: debug_cmd::DebugCmd,
    },
}

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--__nosh_input_worker"))
    {
        let code = nosh_shell::input_assist::run_worker_from_env().unwrap_or_else(|| {
            eprintln!("nosh: missing internal input worker context");
            2
        });
        std::process::exit(code);
    }
    // SAFETY: bootstrap runs before runtime threads or environment readers.
    let control = match unsafe { nosh_shell::pty::inherited_control() } {
        Ok(control) => control,
        Err(error) => {
            eprintln!("nosh: PTY host bootstrap: {error}");
            std::process::exit(2);
        }
    };
    let argv0_login = std::env::args_os()
        .next()
        .is_some_and(|a| a.to_string_lossy().starts_with('-'));
    let mut cli = Cli::parse();
    use std::io::IsTerminal;
    let interactive = cli.cmd.is_none()
        && !cli.agent
        && !cli.suggest
        && cli.command.is_none()
        && cli.rest.is_empty()
        && (cli.interactive || std::io::stdin().is_terminal());
    let interactive_config = interactive.then(config::Config::load);
    if control.is_none()
        && interactive_config
            .as_ref()
            .is_some_and(|c| c.capture_output == nosh_shell::CaptureOutput::Last)
    {
        let relay = std::env::current_exe().and_then(|exe| {
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new(exe);
            if let Some(argv0) = std::env::args_os().next() {
                command.arg0(argv0);
            }
            command.args(std::env::args_os().skip(1));
            nosh_shell::pty::SessionPty::spawn(&mut command)
        });
        match relay {
            Ok(relay) => {
                let code = relay.run().unwrap_or_else(|error| {
                    eprintln!("nosh: terminal relay failed: {error}");
                    70
                });
                std::process::exit(code);
            }
            Err(error) => eprintln!(
                "nosh: user output capture unavailable; continuing without capture: {error}"
            ),
        }
    }
    // SAFETY: the active shell host still has no runtime threads.
    unsafe { nosh_llm::local::configure_thread_env() };
    nosh_shell::register_internal_env(nosh_llm::local::env_overrides());
    if cli.global.offline {
        nosh_hub::net::set_offline(true);
    }
    let code = match cli.cmd.take() {
        Some(Cmd::Model { cmd }) => model_cmd::run(cmd),
        Some(Cmd::Debug { cmd }) => debug_cmd::run(
            cmd,
            cli.global.model_path.as_deref(),
            cli.global.model.as_deref(),
            cli.global.seed,
        ),
        Some(Cmd::Doctor) => {
            let cfg = config::Config::load();
            doctor::run(&cfg, &engine_setup(&cli, &cfg))
        }
        None => run_shell(&cli, argv0_login, interactive_config, control),
    };
    std::process::exit(code);
}

fn engine_setup(cli: &Cli, cfg: &config::Config) -> engine::EngineSetup {
    engine::EngineSetup {
        model_id: cli
            .global
            .model
            .clone()
            .or_else(|| std::env::var("NOSH_MODEL").ok().filter(|s| !s.is_empty()))
            .or_else(|| cfg.model_id.clone()),
        model_path: cli.global.model_path.clone().or_else(|| {
            std::env::var_os("NOSH_MODEL_PATH")
                .filter(|s| !s.is_empty())
                .map(Into::into)
                .or_else(|| cfg.model_path.clone())
        }),
        context_length: cfg.context_length,
        seed: cli.global.seed,
        no_download: cli.global.no_download || !cfg.download_auto,
        selection: cfg.source_selection,
    }
}

fn approval_mode(cli: &Cli, cfg: &config::Config) -> nosh_permissions::ApprovalMode {
    use nosh_permissions::ApprovalMode;
    if cli.global.yolo {
        ApprovalMode::Yolo
    } else if cli.global.auto {
        ApprovalMode::Auto
    } else {
        cfg.approval
    }
}

fn ai_disabled(cli: &Cli) -> bool {
    cli.global.safe || std::env::var("NOSH_DISABLE_AI").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn run_shell(
    cli: &Cli,
    argv0_login: bool,
    interactive_config: Option<config::Config>,
    control: Option<nosh_shell::pty::Control>,
) -> i32 {
    let args = shell_cmd::ShellArgs {
        command: cli.command.as_deref(),
        rest: &cli.rest,
        login: cli.login || argv0_login,
        interactive: cli.interactive,
        norc: cli.global.norc || cli.global.safe,
        errexit: cli.errexit,
        xtrace: cli.xtrace,
        nounset: cli.nounset,
    };
    if cli.agent || cli.suggest {
        let cfg = config::Config::load();
        cfg.print_warnings();
        if ai_disabled(cli) {
            eprintln!("nosh: AI is disabled");
            return 2;
        }
        let setup = engine_setup(cli, &cfg);
        return if cli.suggest {
            agent_cmd::run_suggest(&cli.rest, &cfg, &setup, cli.global.seed)
        } else {
            let mode = approval_mode(cli, &cfg);
            if mode == nosh_permissions::ApprovalMode::Yolo {
                eprintln!(
                    "{}",
                    nosh_shell::style::red_bold(&nosh_core::handler::yolo_warning())
                );
            }
            agent_cmd::run_agent(&cli.rest, cli.json, &cfg, mode, &setup, cli.global.seed)
        };
    }
    if let Some(code) = shell_cmd::run_noninteractive(&args) {
        return code;
    }
    let cfg = interactive_config.unwrap_or_else(config::Config::load);
    cfg.print_warnings();
    let mut shell = match shell_cmd::open_interactive(&args) {
        Ok(s) => s,
        Err(c) => return c,
    };
    shell.configure_output_capture(cfg.capture_output, control);
    // Ctrl-C stops a model download (the partial file is kept).
    shell.interrupts().on_interrupt(nosh_hub::net::cancel);
    let ai_on = !ai_disabled(cli);
    let repl_cfg = nosh_shell::ReplConfig {
        trigger: nosh_shell::TriggerConfig {
            ai_prefix: cfg.ai_prefix.clone(),
            builtin_name: cfg.builtin_name.clone(),
            trigger_on_error: cfg.trigger_on_error,
            nl_guard: cfg.nl_guard,
            ai_enabled: ai_on,
        },
        on_failure: cfg.on_failure,
        input_assist: nosh_shell::input_assist::Config {
            enabled: cfg.input_assist,
            worker: if cfg.input_assist {
                match nosh_shell::input_assist::WorkerCommand::nosh() {
                    Ok(worker) => Some(worker),
                    Err(error) => {
                        eprintln!("nosh: input diagnostics unavailable: {error}");
                        None
                    }
                }
            } else {
                None
            },
        },
        input_abbreviations: Default::default(),
    };
    if !ai_on {
        return nosh_shell::repl::run(&mut shell, &mut nosh_shell::repl::NoAi, repl_cfg);
    }
    let setup = engine_setup(cli, &cfg);
    engine::offer_first_download(&setup);
    let mode = approval_mode(cli, &cfg);
    if mode == nosh_permissions::ApprovalMode::Yolo {
        eprintln!(
            "{}",
            nosh_shell::style::red_bold(&nosh_core::handler::yolo_warning())
        );
    }
    let loader_setup = setup.clone();
    let mut ai = nosh_core::ShellAi::new(
        Box::new(move || engine::load(&loader_setup, true)),
        agent_cmd::agent_config(&cfg, mode, cli.global.seed),
        nosh_core::TerminalApproval::detect(),
    );
    let login = args.login;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        nosh_shell::repl::run(&mut shell, &mut ai, repl_cfg)
    }));
    match result {
        Ok(code) => code,
        Err(_) => fall_back(login, &cfg.fallback_shell),
    }
}

/// The shell core crashed: a login shell must stay usable (design §3.6).
fn fall_back(login: bool, fallback: &str) -> i32 {
    eprintln!("nosh: internal error; please report it (run `nosh doctor` for details)");
    if login {
        use std::os::unix::process::CommandExt;
        eprintln!("nosh: starting {fallback} -l instead");
        let err = std::process::Command::new(fallback).arg("-l").exec();
        eprintln!("nosh: could not start {fallback}: {err}");
    }
    70
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosh_permissions::ApprovalMode;

    #[test]
    fn approval_defaults_and_explicit_overrides_are_shared() {
        assert_eq!(config::Config::default().approval, ApprovalMode::Auto);
        assert_eq!(nosh_core::AgentConfig::default().mode, ApprovalMode::Auto);
        let plain = Cli::try_parse_from(["nosh"]).unwrap();
        for mode in ["confirm", "auto", "yolo"] {
            let cfg = config::Config::parse(&format!("[agent]\napproval = '{mode}'"));
            assert_eq!(
                approval_mode(&plain, &cfg),
                ApprovalMode::parse(mode).unwrap()
            );
            let cli = Cli::try_parse_from(["nosh", "--auto"]).unwrap();
            assert_eq!(approval_mode(&cli, &cfg), ApprovalMode::Auto);
            let cli = Cli::try_parse_from(["nosh", "--yolo"]).unwrap();
            assert_eq!(approval_mode(&cli, &cfg), ApprovalMode::Yolo);
        }
        assert!(Cli::try_parse_from(["nosh", "--auto", "--yolo"]).is_err());
    }
}
