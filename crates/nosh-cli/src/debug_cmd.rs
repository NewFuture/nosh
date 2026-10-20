//! `nosh debug …`: developer utilities.

use std::io::Write;

use clap::Subcommand;
use nosh_engine::{ChatEngine, Event, Message, SessionSpec};
use nosh_llm::{KvDtype, LocalChatEngine, LocalEngineOptions};
use nosh_platform::process::rss_mb;
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
        /// Offer the exec tool.
        #[arg(long)]
        tools: bool,
        /// Enable thinking.
        #[arg(long)]
        think: bool,
        #[arg(long, default_value_t = 8192)]
        ctx: usize,
        #[arg(long)]
        temp: Option<f32>,
        /// Override the model's top-k default; zero disables the filter.
        #[arg(long)]
        top_k: Option<usize>,
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

fn memory_text(value: Option<f64>) -> String {
    value.map_or_else(|| "unavailable".into(), |value| format!("{value:.0}"))
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
            top_k,
            repeat,
            kv,
            no_prepack,
        } => {
            let resolved = crate::engine::locate(setup).and_then(|r| {
                let id = setup.selected_model()?;
                r.ok_or_else(|| {
                    nosh_core::EngineLoadError::NotReady(
                        nosh_hub::HubError::NotInstalled(id.unwrap_or("default").into())
                            .to_string(),
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
                crate::engine::model_source(&resolved),
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
                "[{} | device {} | {} layers | ctx {} | {} threads | load {:.2}s | KV {:?} | recurrent {} | prepacked {} matrices, {:.0} MiB raw released in {:.2}s | RSS {} MB]",
                info.model_id,
                info.device,
                info.layers,
                info.context,
                info.threads,
                info.load_secs,
                info.kv_dtype,
                info.recurrent_state_dtype.unwrap_or("none"),
                info.prepack.tensors,
                info.prepack.released_bytes as f64 / (1024.0 * 1024.0),
                info.prepack.secs,
                memory_text(rss_mb().map(|r| r.0))
            );
            let mut sampling = engine.default_sampling();
            if let Some(t) = temp {
                sampling.temperature = t;
            }
            if let Some(k) = top_k {
                sampling.top_k = k;
            }
            let tool_specs = if tools {
                vec![nosh_core::tools::exec_spec()]
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
                            let _ = write!(
                                out,
                                "{}",
                                setup.theme.snapshot().styles(style::stdout()).dim(&t)
                            );
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
                        let memory = rss_mb();
                        eprintln!(
                            "[round {} | prompt {} tok (cached {}) prefill {:.1} tok/s | TTFT {:.2}s | {} tok decode {:.1} tok/s | stop {:?} | ctx {}/{} | RSS {} MB peak {} MB]",
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
                            memory_text(memory.map(|m| m.0)),
                            memory_text(memory.map(|m| m.1))
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
