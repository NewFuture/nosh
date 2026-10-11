//! Side-effect-free script generation; suggestions still use the ordinary CLI.

use std::ffi::{OsStr, OsString};
use std::io::Write;

use nosh_shell::editing::BashKey;

use crate::{GlobalOpts, config::Config};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Shell {
    Bash,
}

pub fn run(shell: Shell, options: &GlobalOpts) -> i32 {
    let config = Config::load();
    config.print_warnings();
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("nosh init: cannot locate executable: {error}");
            return 2;
        }
    };
    let command = match command(executable.as_os_str(), options) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("nosh init: cannot resolve model path: {error}");
            return 2;
        }
    };
    let script = match shell {
        Shell::Bash => render_bash(&command, &config),
    };
    if let Err(error) = std::io::stdout().lock().write_all(script.as_bytes()) {
        eprintln!("nosh init: cannot write script: {error}");
        return 2;
    }
    0
}

fn command(executable: &OsStr, options: &GlobalOpts) -> std::io::Result<String> {
    let model_path = options
        .model_path
        .as_ref()
        .map(std::path::absolute)
        .transpose()?
        .map(std::path::PathBuf::into_os_string);
    let mut arguments = vec![executable.to_owned()];
    for (enabled, flag) in [
        (options.offline, "--offline"),
        (options.no_download, "--no-download"),
        (options.norc, "--norc"),
        (options.safe, "--safe"),
        (options.auto, "--auto"),
        (options.yolo, "--yolo"),
    ] {
        if enabled {
            arguments.push(flag.into());
        }
    }
    for (flag, value) in [
        (
            "--device",
            options
                .device
                .map(|device| OsString::from(device.to_string())),
        ),
        ("--model-path", model_path),
        ("--model", options.model.as_ref().map(OsString::from)),
        (
            "--seed",
            options.seed.map(|seed| OsString::from(seed.to_string())),
        ),
    ] {
        if let Some(value) = value {
            arguments.extend([flag.into(), value]);
        }
    }
    arguments.extend([OsString::from("-s"), OsString::from("--")]);
    Ok(format!(
        "builtin command {} \"$1\"",
        arguments
            .iter()
            .map(|value| quote(value))
            .collect::<Vec<_>>()
            .join(" ")
    ))
}

fn quote(value: &OsStr) -> String {
    let mut quoted = String::from("$'");
    for &byte in value.as_encoded_bytes() {
        match byte {
            b'\'' => quoted.push_str("\\'"),
            b'\\' => quoted.push_str("\\\\"),
            b' '..=b'~' => quoted.push(char::from(byte)),
            byte => {
                use std::fmt::Write;
                write!(quoted, "\\{byte:03o}").expect("writing to String");
            }
        }
    }
    quoted.push('\'');
    quoted
}

fn render_bash(command: &str, config: &Config) -> String {
    let (bindings, warnings) = config.editing.bash_suggestions();
    for warning in warnings {
        eprintln!("nosh: {warning}");
    }
    let mut installs = String::new();
    for binding in bindings {
        let (kind, key) = match binding.key {
            BashKey::Literal(key) => ("literal", key),
            BashKey::Terminfo(key) => ("terminfo", key),
        };
        use std::fmt::Write;
        writeln!(
            installs,
            "    __nosh_bash_bind {} {kind} {} {} || return",
            quote(OsStr::new(binding.keymap)),
            quote(OsStr::new(&key)),
            if binding.explicit {
                "explicit"
            } else {
                "default"
            },
        )
        .expect("writing to String");
    }
    include_str!("init/bash.sh")
        .replace("\r\n", "\n")
        .replace("# NOSH_BINDINGS", &installs)
        .replace("# NOSH_COMMAND", command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn init_word_in_task_text_does_not_dispatch_a_subcommand() {
        for mode in ["-s", "-a", "-l", "-i", "-e", "-x", "-u", "-eux", "--json"] {
            let cli = crate::Cli::parse_modes_from(["nosh", mode, "init"]).unwrap();
            assert!(cli.cmd.is_none());
            assert_eq!(cli.rest, ["init"]);
        }
        let cli = crate::Cli::parse_modes_from(["nosh", "--offline", "init", "bash"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Some(crate::Cmd::Init { shell: Shell::Bash })
        ));
        assert!(cli.global.offline);
        let cli =
            crate::Cli::parse_modes_from(["nosh", "-c", "printf '%s' \"$0\"", "init"]).unwrap();
        assert!(cli.cmd.is_none());
        assert_eq!(cli.rest, ["init"]);
    }

    #[test]
    fn explicit_options_are_preserved_but_defaults_are_not_frozen() {
        let cli = crate::Cli::parse_from([
            "nosh",
            "--offline",
            "--no-download",
            "--safe",
            "--model-path",
            "a'b/model.gguf",
            "--device",
            "cpu",
            "--seed",
            "7",
            "init",
            "bash",
        ]);
        let text = command(OsStr::new("/a'b/nosh"), &cli.global).unwrap();
        assert!(text.starts_with("builtin command $'/a\\'b/nosh' "));
        let model_path = std::path::absolute("a'b/model.gguf").unwrap();
        assert!(text.contains(&quote(model_path.as_os_str())));
        for flag in [
            "--offline",
            "--no-download",
            "--safe",
            "--model-path",
            "--device",
            "--seed",
        ] {
            assert!(text.contains(flag), "{text}");
        }
        assert!(text.ends_with("$'-s' $'--' \"$1\""));
        let cli = crate::Cli::parse_from(["nosh", "init", "bash"]);
        assert_eq!(
            command(OsStr::new("nosh"), &cli.global).unwrap(),
            "builtin command $'nosh' $'-s' $'--' \"$1\"",
        );
    }

    #[test]
    fn executable_bytes_cannot_inject_shell_syntax() {
        assert_eq!(
            quote(OsStr::new("a'\\\n$(touch bad)")),
            "$'a\\'\\\\\\012$(touch bad)'",
        );
    }
}
