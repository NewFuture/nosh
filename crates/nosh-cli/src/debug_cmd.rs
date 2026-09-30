//! `nosh debug …`: developer utilities.

use std::io::Write;

use clap::Subcommand;
use nosh_llm::{
    ChatEngine, Event, KvDtype, LocalChatEngine, LocalEngineOptions, Message, SessionSpec,
    ToolSpec, rss_mb,
};
use nosh_shell::style;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum KvArg {
    F16,
    F32,
}

#[derive(Debug, Subcommand)]
pub enum DebugCmd {
    /// Private serial engine for the evaluation harness (not a user service).
    #[command(hide = true)]
    EvalWorker {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
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
        /// KV cache element type (CUDA currently supports f16 only).
        #[arg(long, value_enum, default_value = "f16")]
        kv: KvArg,
        /// Keep raw weights instead of prepacking eligible CPU matrices at load.
        #[arg(long)]
        no_prepack: bool,
    },
}

pub fn run(cmd: DebugCmd, setup: &crate::engine::EngineSetup) -> i32 {
    let device = match &setup.device {
        Ok(device) => *device,
        Err(error) => {
            eprintln!("nosh: {error}");
            return 1;
        }
    };
    match cmd {
        DebugCmd::EvalWorker { socket } => match crate::eval_worker::run(&socket, setup) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("nosh: evaluation worker: {error}");
                2
            }
        },
        DebugCmd::Gen {
            prompt,
            max_tokens,
            system,
            tools,
            think,
            ctx,
            temp,
            repeat,
            kv,
            no_prepack,
        } => {
            let resolved = crate::engine::locate(setup).and_then(|r| {
                r.ok_or_else(|| {
                    nosh_hub::HubError::NotInstalled(
                        setup.model_id.as_deref().unwrap_or("default").into(),
                    )
                })
            });
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
                    device,
                    context_length: ctx,
                    seed: setup.seed,
                    kv_dtype: match kv {
                        KvArg::F16 => KvDtype::F16,
                        KvArg::F32 => KvDtype::F32,
                    },
                    prepack_weights: !no_prepack,
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
                "[device {} -> {}: {}]",
                info.device_selection.requested, info.device, info.device_selection.reason,
            );
            eprintln!(
                "[{} | device {} | {} layers | ctx {} | {} threads | load {:.2}s | KV {:?} | prepacked {} matrices, {:.0} MiB raw released in {:.2}s | RSS {:.0} MB]",
                info.model_id,
                info.device,
                info.layers,
                info.context,
                info.threads,
                info.load_secs,
                info.kv_dtype,
                info.prepack.tensors,
                info.prepack.released_bytes as f64 / (1024.0 * 1024.0),
                info.prepack.secs,
                rss_mb().map_or(0.0, |r| r.0)
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
                label: "debug".into(),
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
                            let _ = write!(out, "{}", style::stdout().paint("2", &t));
                            let _ = out.flush();
                        }
                        Event::ToolCall(c) => {
                            let _ = writeln!(
                                out,
                                "\n{} {} {}",
                                style::stdout().glyph("⚙", "*"),
                                c.name,
                                serde_json::Value::Object(c.args)
                            );
                        }
                        Event::CallError(e) => {
                            let _ = writeln!(
                                out,
                                "\n{} tool call error: {e}",
                                style::stdout().glyph("✗", "x")
                            );
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
