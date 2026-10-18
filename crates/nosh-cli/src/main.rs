//! `nosh` entry point: dispatches between the interactive shell, `-c`/script
//! execution, one-shot agent tasks (`-a`), suggestions (`-s`) and management
//! subcommands.

mod abbr_cmd;
mod agent_cmd;
mod config;
mod debug_cmd;
mod doctor;
mod engine;
mod eval_trace;
#[cfg(unix)]
mod eval_worker;
#[cfg(windows)]
#[path = "eval_worker/windows.rs"]
mod eval_worker;
mod init_cmd;
mod model_cmd;
mod model_defaults;
mod shell_cmd;
mod starship;
mod theme_cmd;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

#[cfg(any(target_os = "macos", windows))]
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

impl Cli {
    fn parse_modes_from(
        arguments: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
    ) -> Result<Self, clap::Error> {
        let arguments: Vec<_> = arguments.into_iter().map(Into::into).collect();
        // In shell/task modes, "init" and other subcommand names are input.
        // Global options alone must still allow `nosh --offline init bash`.
        let matches = Self::command()
            .args_conflicts_with_subcommands(true)
            .try_get_matches_from(&arguments)?;
        let cli = Self::from_arg_matches(&matches)?;
        if cli.cmd.is_some()
            || cli.command.is_some()
            || cli.agent
            || cli.suggest
            || cli.login
            || cli.interactive
            || cli.errexit
            || cli.xtrace
            || cli.nounset
            || cli.json
        {
            Ok(cli)
        } else {
            Self::try_parse_from(arguments)
        }
    }
}

#[derive(Debug, clap::Args)]
struct GlobalOpts {
    /// Inference device: auto (default; available CUDA memory), cpu, cuda or cuda:N.
    #[arg(long, global = true, value_name = "DEVICE")]
    device: Option<nosh_llm::InferenceDevice>,
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
    /// Manage visible command abbreviations (list, set, remove, enable, disable).
    Abbr {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print shell integration code; does not modify startup files.
    Init {
        #[arg(value_enum)]
        shell: init_cmd::Shell,
    },
    /// Preview and select the shell's color theme.
    Theme {
        #[command(subcommand)]
        cmd: theme_cmd::ThemeCmd,
    },
    /// Manage local models.
    Model {
        #[command(subcommand)]
        cmd: model_cmd::ModelCmd,
    },
    /// Check inference device selection, CPU, memory, model, sources and configuration.
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
    let mut cli = Cli::parse_modes_from(std::env::args_os()).unwrap_or_else(|error| error.exit());
    if let Some(Cmd::Init { shell }) = cli.cmd {
        std::process::exit(init_cmd::run(shell, &cli.global));
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
            #[cfg(unix)]
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new(exe);
            #[cfg(unix)]
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
        Some(Cmd::Abbr { args }) => abbr_cmd::run(&args),
        Some(Cmd::Init { .. }) => unreachable!("init is dispatched before runtime setup"),
        Some(Cmd::Theme { cmd }) => theme_cmd::run(cmd),
        Some(Cmd::Model { cmd }) => model_cmd::run(cmd, cli.global.device.unwrap_or_default()),
        Some(Cmd::Debug { cmd }) => {
            let cfg = config::Config::load();
            cfg.print_warnings();
            let allow_cuda = !matches!(
                &cmd,
                debug_cmd::DebugCmd::Gen {
                    kv: debug_cmd::KvArg::F32,
                    ..
                }
            );
            debug_cmd::run(cmd, &engine_setup_with_cuda(&cli, &cfg, allow_cuda))
        }
        Some(Cmd::Doctor) => {
            let cfg = config::Config::load();
            doctor::run(&cfg, &engine_setup(&cli, &cfg))
        }
        None => run_shell(&cli, argv0_login, interactive_config, control),
    };
    std::process::exit(code);
}

fn engine_setup(cli: &Cli, cfg: &config::Config) -> engine::EngineSetup {
    engine_setup_with_cuda(cli, cfg, true)
}

fn engine_setup_with_cuda(
    cli: &Cli,
    cfg: &config::Config,
    allow_cuda: bool,
) -> engine::EngineSetup {
    let model_path = cli.global.model_path.clone().or_else(|| {
        std::env::var_os("NOSH_MODEL_PATH")
            .filter(|s| !s.is_empty())
            .map(Into::into)
            .or_else(|| cfg.model_path.clone())
    });
    let model_id = cli
        .global
        .model
        .clone()
        .or_else(|| std::env::var("NOSH_MODEL").ok().filter(|s| !s.is_empty()))
        .or_else(|| cfg.model_id.clone());
    let device = cli
        .global
        .device
        .map(Ok)
        .unwrap_or_else(|| cfg.model_device.clone());
    let model_id = match &device {
        Ok(device) => model_defaults::select(
            model_id,
            model_path.is_some(),
            if allow_cuda {
                *device
            } else {
                nosh_llm::InferenceDevice::Cpu
            },
        ),
        Err(_) => Ok(model_id),
    };
    engine::EngineSetup {
        theme: nosh_shell::theme::ThemeHandle::new(cfg.theme.palette),
        device,
        model_id,
        model_path,
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
                    cfg.theme
                        .palette
                        .styles(nosh_shell::style::stderr())
                        .red_bold(&nosh_core::handler::yolo_warning())
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
    shell.configure_project_env(if cli.global.safe {
        Ok(nosh_shell::project_env::Provider::Off)
    } else {
        cfg.project_env.clone()
    });
    // Ctrl-C stops a model download (the partial file is kept).
    shell.interrupts().on_interrupt(nosh_hub::net::cancel);
    let ai_on = !ai_disabled(cli);
    let themes = nosh_shell::theme::ThemeHandle::new(cfg.theme.palette);
    let theme_path = cfg
        .path
        .clone()
        .unwrap_or_else(nosh_platform::paths::config_file);
    let theme_loader = std::sync::Arc::new(config::theme::Loader::new(
        theme_path.clone(),
        cfg.theme.clone(),
    ));
    let theme_reload = theme_loader.clone();
    let abbreviation_path = cfg
        .path
        .clone()
        .unwrap_or_else(nosh_platform::paths::config_file);
    let abbreviation_load_path = abbreviation_path.clone();
    let repl_cfg = nosh_shell::ReplConfig {
        abbreviations: cfg.abbreviations.clone(),
        abbreviation_loader: Some(nosh_shell::abbreviations::Loader(std::sync::Arc::new(
            move || config::abbreviations::load(&abbreviation_load_path),
        ))),
        abbreviation_command: Some(nosh_shell::abbreviations::Command(std::sync::Arc::new(
            move |shell, operation| abbr_cmd::execute(&abbreviation_path, shell, operation),
        ))),
        abbreviation_worker: match nosh_shell::input_assist::WorkerCommand::nosh() {
            Ok(worker) => Some(worker),
            Err(error) => {
                eprintln!("nosh: abbreviation worker unavailable: {error}");
                None
            }
        },
        terminal_integration: cfg.terminal_integration,
        status_bar: nosh_shell::status::Config {
            enabled: cfg.status_bar,
            theme: themes.clone(),
        },
        theme_loader: Some(nosh_shell::repl::ThemeLoader(std::sync::Arc::new(
            move || theme_reload.refresh(),
        ))),
        theme_command: Some(nosh_shell::repl::ThemeCommand(std::sync::Arc::new(
            move |shell, name| theme_cmd::select(&theme_path, &shell.cwd(), name),
        ))),
        environment_loader: Some(nosh_shell::repl::EnvironmentLoader(std::sync::Arc::new(
            move |shell| theme_loader.environment(shell),
        ))),
        trigger: nosh_shell::TriggerConfig {
            ai_prefix: cfg.ai_prefix.clone(),
            trigger_on_error: cfg.trigger_on_error,
            nl_guard: cfg.nl_guard,
            ai_enabled: ai_on,
        },
        on_failure: cfg.on_failure,
        command_assist: cfg.command_assist,
        editing: cfg.editing.clone(),
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
        completion: nosh_shell::completion::Config {
            enabled: cfg.completion,
            scripts: cfg.completion_scripts,
            inline_prefix: None,
            abbreviations: Default::default(),
            selection_observer: None,
            owner: None,
            worker: if cfg.completion {
                match nosh_shell::completion::WorkerCommand::nosh() {
                    Ok(worker) => Some(worker),
                    Err(error) => {
                        eprintln!("nosh: completion worker unavailable: {error}");
                        None
                    }
                }
            } else {
                None
            },
        },
    };
    if !ai_on {
        return nosh_shell::repl::run(&mut shell, &mut nosh_shell::repl::NoAi, repl_cfg);
    }
    let mut setup = engine_setup(cli, &cfg);
    setup.theme = themes.clone();
    engine::offer_first_download(&setup);
    let mode = approval_mode(cli, &cfg);
    if mode == nosh_permissions::ApprovalMode::Yolo {
        eprintln!(
            "{}",
            cfg.theme
                .palette
                .styles(nosh_shell::style::stderr())
                .red_bold(&nosh_core::handler::yolo_warning())
        );
    }
    let loader_setup = setup.clone();
    let mut ai = nosh_core::ShellAi::new(
        Box::new(move |mode| engine::load(&loader_setup, mode)),
        agent_cmd::agent_config(&cfg, mode, cli.global.seed),
        nosh_core::TerminalApproval::detect_with_theme(themes.clone()),
    )
    .with_theme(themes);
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
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            eprintln!("nosh: starting {fallback} -l instead");
            let err = std::process::Command::new(fallback).arg("-l").exec();
            eprintln!("nosh: could not start {fallback}: {err}");
        }
        #[cfg(windows)]
        {
            eprintln!("nosh: starting {fallback} instead");
            match std::process::Command::new(fallback).status() {
                Ok(status) => return status.code().unwrap_or(70),
                Err(error) => eprintln!("nosh: could not start {fallback}: {error}"),
            }
        }
    }
    70
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosh_permissions::ApprovalMode;

    #[test]
    fn debug_top_k_accepts_explicit_disable_and_rejects_negative_values() {
        assert!(Cli::try_parse_from(["nosh", "debug", "gen", "hello", "--top-k", "0"]).is_ok());
        assert!(Cli::try_parse_from(["nosh", "debug", "gen", "hello", "--top-k", "-1"]).is_err());
    }

    #[test]
    fn device_override_is_global_and_config_is_shared() {
        use nosh_llm::InferenceDevice;
        let cfg = config::Config::parse("[model]\ndevice = 'cuda:1'");
        let plain = Cli::try_parse_from(["nosh"]).unwrap();
        assert_eq!(
            engine_setup(&plain, &config::Config::default()).device,
            Ok(InferenceDevice::Auto)
        );
        assert_eq!(
            engine_setup(&plain, &cfg).device,
            Ok(InferenceDevice::Cuda(1))
        );
        for args in [
            vec!["nosh", "--device", "cpu", "-s", "list files"],
            vec!["nosh", "--device", "cpu", "-a", "list files"],
            vec!["nosh", "doctor", "--device", "cpu"],
            vec!["nosh", "debug", "gen", "hello", "--device", "cpu"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(engine_setup(&cli, &cfg).device, Ok(InferenceDevice::Cpu));
        }
        let invalid = config::Config::parse("[model]\ndevice = 'metal'");
        assert!(engine_setup(&plain, &invalid).device.is_err());
        assert!(Cli::try_parse_from(["nosh", "--device", "cdua"]).is_err());
    }

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
