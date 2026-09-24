//! Shell modes: `-c`, scripts, commands from stdin and the interactive REPL.

use std::io::IsTerminal;
use std::path::Path;

use nosh_shell::{EmbeddedShell, ShellOptions};

pub struct ShellArgs<'a> {
    pub command: Option<&'a str>,
    pub rest: &'a [String],
    pub login: bool,
    pub interactive: bool,
    pub norc: bool,
    pub errexit: bool,
    pub xtrace: bool,
    pub nounset: bool,
}

fn base(a: &ShellArgs) -> ShellOptions {
    ShellOptions {
        login: a.login,
        errexit: a.errexit,
        xtrace: a.xtrace,
        nounset: a.nounset,
        ..ShellOptions::default()
    }
}

fn open(opts: ShellOptions) -> Result<EmbeddedShell, i32> {
    EmbeddedShell::new(opts).map_err(|e| {
        eprintln!("nosh: {e}");
        2
    })
}

/// `nosh -c`, `nosh script`, and `… | nosh`: plain bash semantics, no model,
/// nothing printed beyond what the commands print. `None` means "interactive".
pub fn run_noninteractive(a: &ShellArgs) -> Option<i32> {
    if let Some(cmd) = a.command {
        let opts = ShellOptions {
            command_string_mode: true,
            load_rc: a.login && !a.norc,
            name: a.rest.first().cloned(),
            args: a.rest.iter().skip(1).cloned().collect(),
            ..base(a)
        };
        return Some(match open(opts) {
            Ok(mut sh) => sh.run_dash_c(cmd),
            Err(c) => c,
        });
    }
    if let Some(script) = a.rest.first() {
        let opts = ShellOptions {
            load_rc: a.login && !a.norc,
            name: Some(script.clone()),
            args: a.rest[1..].to_vec(),
            ..base(a)
        };
        return Some(match open(opts) {
            Ok(mut sh) => sh.run_script(Path::new(script), &a.rest[1..]),
            Err(c) => c,
        });
    }
    if !a.interactive && !std::io::stdin().is_terminal() {
        let opts = ShellOptions {
            load_rc: a.login && !a.norc,
            ..base(a)
        };
        return Some(match open(opts) {
            Ok(mut sh) => sh.run_stdin(),
            Err(c) => c,
        });
    }
    None
}

pub fn open_interactive(a: &ShellArgs) -> Result<EmbeddedShell, i32> {
    open(ShellOptions {
        interactive: true,
        load_rc: !a.norc,
        ..base(a)
    })
}
