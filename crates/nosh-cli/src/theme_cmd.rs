use std::path::Path;

use crate::config;
use clap::Subcommand;
use nosh_platform::tr;
use nosh_shell::{
    style,
    theme::{Builtin, preview},
};

#[derive(Debug, Subcommand)]
pub enum ThemeCmd {
    /// List built-in themes and the saved selection.
    List,
    /// Render sample states without executing commands or changing the theme.
    Preview {
        name: Option<String>,
        #[arg(long, value_parser = clap::value_parser!(u16).range(17..=4096))]
        columns: Option<u16>,
        #[arg(long, default_value = "auto", value_parser = ["auto", "confirm", "yolo", "off"])]
        mode: String,
        #[arg(long, value_parser = ["idle", "input", "fields", "pending", "advice", "error", "unavailable", "completion", "history"])]
        case: Option<String>,
        /// Force Powerline caps; requires supporting fonts or built-in glyphs.
        #[arg(long)]
        nerd_font: bool,
    },
    /// Save the theme without restarting nosh.
    Set { name: String },
    /// Write an interoperable Starship configuration to stdout.
    Export {
        name: Option<String>,
        #[arg(long)]
        nerd_font: bool,
    },
}

pub fn run(command: ThemeCmd) -> i32 {
    match execute(command) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("nosh: {}", style::visible_text(&error));
            2
        }
    }
}

pub fn select(path: &Path, cwd: &Path, name: Option<&str>) -> Result<String, String> {
    let Some(name) = name else {
        return Ok(format!(
            "theme: {}",
            style::visible_text(&config::theme::load(path)?.label())
        ));
    };
    let selected = config::theme::selection_key(name, cwd)?;
    config::theme::save(path, &selected)?;
    Ok(format!(
        "nosh: {} {} ({})",
        style::visible_text(&selected),
        tr!("已保存，无需重启", "saved; no restart needed"),
        style::visible_text(&path.to_string_lossy())
    ))
}

fn execute(command: ThemeCmd) -> Result<(), String> {
    let path = nosh_platform::paths::config_file();
    match command {
        ThemeCmd::Set { name } => {
            eprintln!("{}", select(&path, Path::new("."), Some(&name))?);
        }
        ThemeCmd::List => {
            let selected = config::theme::load(&path)?;
            for theme in Builtin::ALL {
                println!(
                    "{} {}",
                    if selected.builtin() == Some(theme) {
                        "*"
                    } else {
                        " "
                    },
                    theme.name()
                );
            }
            if selected.builtin().is_none() {
                println!("* {}", style::visible_text(&selected.label()));
            }
        }
        ThemeCmd::Export { name, nerd_font } => {
            let source = if let Some(name) = name {
                let builtin: Builtin = name.parse()?;
                crate::starship::export(builtin, nerd_font)
            } else {
                let mut selected = config::theme::load(&path)?;
                if selected.builtin().is_none() && nerd_font {
                    return Err("edit the existing Starship format to add caps; --nerd-font export applies to internal themes".into());
                }
                selected.palette.powerline |= nerd_font;
                selected.starship().into_owned()
            };
            print!("{source}");
        }
        ThemeCmd::Preview {
            name,
            columns,
            mode,
            case,
            nerd_font,
        } => {
            let text = config::read_text(&path)?;
            let table = text
                .parse()
                .map_err(|e| format!("invalid configuration: {e}"))?;
            let requested = name
                .as_deref()
                .map(|name| config::theme::selection_key(name, Path::new(".")))
                .transpose()?;
            let mut selected = config::theme::resolve_at(&table, requested.as_deref(), &path)?;
            selected.palette.powerline |= nerd_font;
            let terminal = style::stdout();
            let columns = columns.unwrap_or_else(|| {
                nosh_shell::term::stdout_columns().unwrap_or(80).min(4096) as u16
            });
            let mode_name = match mode.as_str() {
                "confirm" => tr!("审批: 询问", "Approval: Confirm"),
                "yolo" => "YOLO",
                "off" => "",
                _ => tr!("审批: 自动", "Approval: Auto"),
            };
            println!(
                "{} ({columns}) - {}",
                style::visible_text(&selected.label()),
                tr!(
                    "渲染样本；命令不会执行，主题不会保存",
                    "renderer samples; no execution, no changes saved"
                )
            );
            for sample in preview::samples(
                selected.palette,
                terminal,
                columns,
                mode_name,
                mode != "off",
            ) {
                if case.as_ref().is_some_and(|case| *case != sample.name) {
                    continue;
                }
                println!("\n[{}]\n{}\n{}", sample.name, sample.status, sample.input);
                if !sample.detail.is_empty() {
                    println!("{}", sample.detail);
                }
            }
            if mode == "off" {
                println!(
                    "\n{}",
                    tr!(
                        "AI 建议/审批：不适用（AI 关闭）",
                        "AI suggestion/approval: not applicable (AI off)"
                    )
                );
            }
        }
    }
    Ok(())
}
