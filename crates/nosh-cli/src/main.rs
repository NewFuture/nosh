//! `nosh` entry point: dispatches between the interactive shell, `-c`/script
//! execution, one-shot agent tasks (`-a`), suggestions (`-s`) and management
//! subcommands.

mod model_cmd;

use clap::{Parser, Subcommand};

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
    /// Approval mode auto (fewer confirmations).
    #[arg(long, global = true, conflicts_with = "yolo")]
    auto: bool,
    /// Approval mode yolo (only Dangerous asks; Forbidden still denied).
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
}

fn main() {
    let cli = Cli::parse();
    if cli.global.offline {
        nosh_hub::net::set_offline(true);
    }
    let code = match cli.cmd {
        Some(Cmd::Model { cmd }) => model_cmd::run(cmd),
        None => {
            eprintln!("nosh: shell mode is not implemented yet");
            2
        }
    };
    std::process::exit(code);
}
