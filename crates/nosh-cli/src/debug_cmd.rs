//! `nosh debug …`: developer utilities.

use std::io::Write;

use clap::Subcommand;
use nosh_hub::ModelHub;
use nosh_llm::{
    ChatEngine, Event, LocalChatEngine, LocalEngineOptions, Message, SessionSpec, ToolSpec, rss_mb,
};

#[derive(Debug, Subcommand)]
pub enum DebugCmd {
    /// Generate a reply to PROMPT and report prefill/decode speed.
    Gen {
        prompt: String,
        #[arg(long, default_value_t = 256)]
        max_tokens: usize,
        #[arg(long, default_value = "You are a helpful assistant.")]
        system: String,
        /// Offer a run_command tool.
        #[arg(long)]
        tools: bool,
        /// Enable thinking.
        #[arg(long)]
        think: bool,
        #[arg(long, default_value_t = 8192)]
        ctx: usize,
        #[arg(long)]
        temp: Option<f32>,
        /// Run the same prompt this many times in one conversation (tests KV reuse).
        #[arg(long, default_value_t = 1)]
        repeat: usize,
    },
}

pub fn run(
    cmd: DebugCmd,
    model_path: Option<&std::path::Path>,
    model: Option<&str>,
    seed: Option<u64>,
) -> i32 {
    match cmd {
        DebugCmd::Gen {
            prompt,
            max_tokens,
            system,
            tools,
            think,
            ctx,
            temp,
            repeat,
        } => {
            let hub = ModelHub::new();
            let resolved = match model_path {
                Some(p) => hub.resolve_path(p, model),
                None => hub.find(model).and_then(|r| {
                    r.ok_or_else(|| {
                        nosh_hub::HubError::NotInstalled(model.unwrap_or("default").into())
                    })
                }),
            };
            let resolved = match resolved {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("nosh: {e}");
                    return 1;
                }
            };
            let mut engine = match LocalChatEngine::load(
                &resolved,
                LocalEngineOptions {
                    context_length: ctx,
                    seed,
                    ..LocalEngineOptions::default()
                },
            ) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("nosh: {e}");
                    return 1;
                }
            };
            let info = engine.info().clone();
            eprintln!(
                "[{} | {} layers | ctx {} | {} threads | load {:.2}s]",
                info.model_id, info.layers, info.context, info.threads, info.load_secs
            );
            let mut sampling = engine.default_sampling();
            if let Some(t) = temp {
                sampling.temperature = t;
            }
            let tool_specs = if tools {
                vec![ToolSpec {
                    name: "run_command".into(),
                    description: "Run a bash command in the user's shell session.".into(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {"command": {"type": "string", "description": "The command line."}},
                        "required": ["command"]
                    }),
                }]
            } else {
                vec![]
            };
            let sid = match engine.open(SessionSpec {
                system,
                tools: tool_specs,
                thinking: think,
                sampling,
                max_new_tokens: max_tokens,
            }) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("nosh: {e}");
                    return 1;
                }
            };
            for round in 0..repeat.max(1) {
                let mut out = std::io::stdout();
                let res = engine.step(
                    sid,
                    vec![Message::User(prompt.clone())],
                    &mut |ev| match ev {
                        Event::Text(t) => {
                            let _ = write!(out, "{t}");
                            let _ = out.flush();
                        }
                        Event::Think(t) => {
                            let _ = write!(out, "\x1b[2m{t}\x1b[0m");
                            let _ = out.flush();
                        }
                        Event::ToolCall(c) => {
                            let _ = writeln!(
                                out,
                                "\n⚙ {} {}",
                                c.name,
                                serde_json::Value::Object(c.args)
                            );
                        }
                        Event::CallError(e) => {
                            let _ = writeln!(out, "\n✗ tool call error: {e}");
                        }
                        Event::Prefill { .. } => {}
                    },
                );
                println!();
                match res {
                    Ok(o) => {
                        let u = &o.usage;
                        let (rss, peak) = rss_mb().unwrap_or((0.0, 0.0));
                        eprintln!(
                            "[round {} | prompt {} tok (cached {}) prefill {:.1} tok/s | TTFT {:.2}s | {} tok decode {:.1} tok/s | stop {:?} | ctx {}/{} | RSS {:.0} MB peak {:.0} MB]",
                            round + 1,
                            u.prompt_tokens,
                            u.cached_tokens,
                            u.prefill_tps(),
                            u.ttft_secs,
                            u.completion_tokens,
                            u.decode_tps(),
                            o.stop,
                            u.context_used,
                            u.context_max,
                            rss,
                            peak
                        );
                    }
                    Err(e) => {
                        eprintln!("nosh: {e}");
                        return 1;
                    }
                }
            }
            0
        }
    }
}
