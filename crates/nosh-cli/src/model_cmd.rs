//! `nosh model …` subcommands.

use std::path::PathBuf;

use clap::Subcommand;
use nosh_hub::{BarProgress, FileState, HubError, ModelHub, PullOptions, SourceSelection, net, tr};
use nosh_shell::style;

#[derive(Debug, Subcommand)]
pub enum ModelCmd {
    /// Download (or resume) a model and its tokenizer, verifying SHA-256.
    Pull {
        /// Model id (default: the registry default, minicpm5-2b:q4_k_m).
        id: Option<String>,
        /// Source: auto | hf | hf-mirror | modelscope.
        #[arg(long, default_value = "auto")]
        source: String,
    },
    /// List registry models and their install state.
    List,
    /// Recompute SHA-256 of installed files.
    Verify { id: Option<String> },
    /// Import a local GGUF (identified by SHA-256) into the model store.
    Import {
        gguf: PathBuf,
        #[arg(long)]
        tokenizer: Option<PathBuf>,
    },
    /// Print the model directory.
    Path { id: Option<String> },
}

fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1e9)
}

pub fn run(cmd: ModelCmd) -> i32 {
    let hub = ModelHub::new();
    match cmd {
        ModelCmd::Pull { id, source } => {
            let Some(selection) = SourceSelection::parse(&source) else {
                eprintln!("nosh: unknown source '{source}' (auto|hf|hf-mirror|modelscope)");
                return 2;
            };
            let entry = match hub.registry().lookup(id.as_deref()) {
                Ok(e) => e.clone(),
                Err(e) => {
                    eprintln!("nosh: {e}");
                    return 1;
                }
            };
            if let Ok(Some(found)) = hub.find(Some(&entry.id)) {
                eprintln!(
                    "{} {}",
                    style::glyph("✔", "+"),
                    tr!(
                        format!("{} 已安装：{}", entry.display, found.dir.display()),
                        format!("{} is installed: {}", entry.display, found.dir.display())
                    )
                );
                return 0;
            }
            if net::is_offline() {
                report(&HubError::Offline);
                return 1;
            }
            eprintln!(
                "{}",
                tr!(
                    format!(
                        "下载 {}（{}，{}）",
                        entry.display,
                        gb(entry.total_size()),
                        entry.license
                    ),
                    format!(
                        "downloading {} ({}, {})",
                        entry.display,
                        gb(entry.total_size()),
                        entry.license
                    )
                )
            );
            let opts = PullOptions {
                selection,
                ..Default::default()
            };
            match hub.pull(Some(&entry.id), &opts, &BarProgress::new()) {
                Ok(r) => {
                    eprintln!(
                        "{} {}",
                        style::glyph("✔", "+"),
                        tr!(
                            "SHA-256 校验通过。之后可以完全断网使用。",
                            "SHA-256 verified. nosh can now run fully offline."
                        )
                    );
                    eprintln!("  {}", r.dir.display());
                    0
                }
                Err(e) => {
                    report(&e);
                    1
                }
            }
        }
        ModelCmd::List => {
            let default_id = hub.registry().default_model().id.clone();
            for m in &hub.registry().models {
                let st = hub.status(m);
                let state = if let Some(f) = &st.found {
                    format!("{}  {}", tr!("已安装", "installed"), f.dir.display())
                } else {
                    match (&st.weights, &st.tokenizer) {
                        (FileState::Partial(n), _) => format!(
                            "{} {}%",
                            tr!("部分下载", "partial"),
                            n * 100 / m.weights().size.max(1)
                        ),
                        (FileState::Present { .. }, t) if !t.is_present() => {
                            tr!("缺少 tokenizer", "tokenizer missing").to_string()
                        }
                        (FileState::WrongSize(_), _) => tr!("文件损坏", "corrupt").to_string(),
                        _ => tr!("未安装", "not installed").to_string(),
                    }
                };
                let mark = if m.id == default_id { "*" } else { " " };
                println!("{:<20}{mark} {:>8}  {state}", m.id, gb(m.total_size()));
            }
            0
        }
        ModelCmd::Verify { id } => {
            let res = hub.verify(id.as_deref(), |f, r| match r {
                Ok(()) => println!("{} {}", style::stdout().glyph("✔", "+"), f.name),
                Err(e) => println!("{} {}: {e}", style::stdout().glyph("✗", "x"), f.name),
            });
            match res {
                Ok(true) => 0,
                Ok(false) => 1,
                Err(e) => {
                    report(&e);
                    1
                }
            }
        }
        ModelCmd::Import { gguf, tokenizer } => {
            match hub.import(&gguf, tokenizer.as_deref(), &BarProgress::new()) {
                Ok(r) => {
                    eprintln!(
                        "{} {}",
                        style::glyph("✔", "+"),
                        tr!(
                            format!(
                                "已导入 {} {} {}",
                                r.entry.id,
                                style::glyph("→", "->"),
                                r.dir.display()
                            ),
                            format!(
                                "imported {} {} {}",
                                r.entry.id,
                                style::glyph("→", "->"),
                                r.dir.display()
                            )
                        )
                    );
                    0
                }
                Err(e) => {
                    report(&e);
                    1
                }
            }
        }
        ModelCmd::Path { id } => match hub.registry().lookup(id.as_deref()) {
            Ok(entry) => {
                let dir = hub
                    .find(Some(&entry.id))
                    .ok()
                    .flatten()
                    .map(|r| r.dir)
                    .unwrap_or_else(|| hub.user_model_dir(entry));
                println!("{}", dir.display());
                0
            }
            Err(e) => {
                report(&e);
                1
            }
        },
    }
}

fn report(e: &HubError) {
    eprintln!("nosh: {e}");
    match e {
        HubError::Offline => eprintln!(
            "{}",
            tr!(
                "提示：离线模式下可以用 `nosh model import <gguf>` 导入模型。",
                "hint: in offline mode, use `nosh model import <gguf>`."
            )
        ),
        HubError::AllSourcesFailed(_) | HubError::Network(_) if !net::is_offline() => eprintln!(
            "{}",
            tr!(
                "提示：可以重试（会断点续传），或手动下载后用 `nosh model import` 导入。",
                "hint: retry to resume, or download manually and `nosh model import` it."
            )
        ),
        _ => {}
    }
}
