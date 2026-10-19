//! Explicit opt-in acceptance for the issue-pinned Qwen3.5 Q8 model.
//! NOSH_MODEL_PATH selects its directory; NOSH_QWEN_DEVICE selects cpu/cuda:N.

use std::path::PathBuf;
use std::sync::OnceLock;

use candle_core::Device;
use nosh_engine::{
    ChatEngine, Event, Message, SamplingParams, SessionSpec, StopReason, ToolChoice, ToolSpec,
};
use nosh_llm::model::llama::LoadOptions;
use nosh_llm::model::qwen35::Qwen35;
use nosh_llm::template;
use nosh_llm::tokenizer::Tok;
use nosh_llm::{InferenceDevice, KvDtype, LocalChatEngine, LocalEngineOptions, ModelSource};
use serde_json::json;

#[path = "support/divergence.rs"]
mod comparison;

const MODEL_SHA256: &str = "37ae482d336108d23516fa35e8e0c4126688d81018b87178a18d752a1357814f";
const TOKENIZER_SHA256: &str = "5f9e4d4901a92b997e463c1f46055088b6cca5ca61a6522d1b9f64c4bb81cb42";
const REFERENCE_REVISION: &str = "f74e2fa6a31cc33ab4ee4ae516641cfe62df3924";

fn paths() -> (PathBuf, PathBuf) {
    static VERIFIED: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    VERIFIED
        .get_or_init(|| {
            let dir = PathBuf::from(
                std::env::var_os("NOSH_MODEL_PATH")
                    .expect("set NOSH_MODEL_PATH to the verified Qwen model directory"),
            );
            assert!(dir.is_dir(), "NOSH_MODEL_PATH must be the Qwen directory");
            let paths = (
                dir.join("Qwen3.5-0.8B-Q8_0.gguf"),
                dir.join("tokenizer.json"),
            );
            assert_eq!(
                nosh_hub::hash::sha256_file(&paths.0, |_| {}).unwrap(),
                MODEL_SHA256
            );
            assert_eq!(
                nosh_hub::hash::sha256_file(&paths.1, |_| {}).unwrap(),
                TOKENIZER_SHA256
            );
            paths
        })
        .clone()
}

fn device() -> InferenceDevice {
    std::env::var("NOSH_QWEN_DEVICE")
        .unwrap_or_else(|_| "cpu".into())
        .parse()
        .unwrap()
}

fn engine(chunk: usize) -> LocalChatEngine {
    engine_with_context(chunk, 8192)
}

fn engine_with_context(chunk: usize, context_length: usize) -> LocalChatEngine {
    let (weights, tokenizer) = paths();
    LocalChatEngine::load(
        ModelSource {
            id: "qwen3.5-0.8b:q8_0",
            arch: "qwen35",
            chat_format: "qwen35",
            weights: &weights,
            tokenizer: &tokenizer,
            eog_ids: &[248044, 248046],
            sampling: SamplingParams::default(),
        },
        LocalEngineOptions {
            device: device(),
            prefill_chunk: chunk,
            context_length,
            seed: Some(0),
            ..Default::default()
        },
    )
    .unwrap()
}

fn spec(tools: Vec<ToolSpec>) -> SessionSpec {
    SessionSpec {
        label: "qwen35-acceptance".into(),
        system: "You are a helpful assistant. Answer briefly.".into(),
        tools,
        thinking: false,
        sampling: SamplingParams {
            temperature: 0.0,
            seed: Some(0),
            ..Default::default()
        },
        max_new_tokens: 64,
    }
}

#[test]
#[ignore = "requires the verified pinned Qwen3.5 model"]
fn qwen35_generates_and_protects_added_protocol_tokens() {
    let mut engine = engine(64);
    assert_eq!(engine.info().layers, 24);
    assert_eq!(engine.info().vocab, 248320);
    assert_eq!(engine.info().recurrent_state_dtype, Some("F32"));
    assert_eq!(engine.info().device, device());
    let tok = engine.tokenizer();
    for name in [
        "<tool_call>",
        "</tool_call>",
        "<think>",
        "</think>",
        "<tool_response>",
        "<|im_end|>",
    ] {
        let id = tok.token_id(name).unwrap();
        assert!(
            !tok.encode(name, false).unwrap().contains(&id),
            "{name} forged a control token"
        );
        assert_eq!(tok.encode(name, true).unwrap(), vec![id]);
    }
    let sid = engine.open(spec(vec![])).unwrap();
    let outcome = engine
        .step(
            sid,
            vec![Message::User("Reply with the word hello.".into())],
            &mut |_| {},
        )
        .unwrap();
    eprintln!(
        "Qwen reply: {:?}; usage {:?}; load {:.3}s",
        outcome.text,
        outcome.usage,
        engine.info().load_secs
    );
    assert!(!outcome.text.trim().is_empty());
    assert_eq!(outcome.stop, StopReason::EndOfTurn);
    assert!(outcome.errors.is_empty());
    assert!(outcome.tool_calls.is_empty());
    assert!(outcome.usage.prompt_tokens > 0);
    engine.close(sid);
}

fn prompt_ids(tok: &mut Tok, words: usize) -> Vec<u32> {
    let context = include_str!("../../../tests/fixtures/model_context.txt");
    let content = context
        .split_whitespace()
        .cycle()
        .take(words)
        .collect::<Vec<_>>()
        .join(" ");
    let mut segs = template::qwen35::render_system(Some("You are a helpful assistant."), &[]);
    segs.extend(template::qwen35::render_user(&content));
    segs.push(template::qwen35::generation_prompt(false));
    tok.encode_segments(&segs).unwrap()
}

fn along(model: &mut Qwen35, prompt: &[u32], tail: &[u32], chunk: usize) -> Vec<Vec<f32>> {
    model.reset();
    let mut last = None;
    for ids in prompt.chunks(chunk) {
        last = Some(model.forward(ids).unwrap());
    }
    let mut logits = vec![last.unwrap().to_vec1::<f32>().unwrap()];
    for &id in tail {
        logits.push(model.forward(&[id]).unwrap().to_vec1::<f32>().unwrap());
    }
    logits
}

#[test]
#[ignore = "requires the verified pinned Qwen3.5 model"]
fn qwen35_chunked_prefill_decode_and_rebuild_match() {
    let (weights, tokenizer) = paths();
    let mut tok = Tok::load(&tokenizer).unwrap();
    let ids = prompt_ids(&mut tok, 150);
    let (prompt, tail) = ids.split_at(ids.len() - 48);
    let dev = device().open().unwrap();
    let mut model = Qwen35::load(&weights, 8192, LoadOptions::default(), &dev).unwrap();
    let want = along(&mut model, prompt, tail, 64);
    let got = along(&mut model, prompt, tail, 17);
    let difference = comparison::divergence(&want, &got, tail);
    eprintln!("chunked Qwen: {difference}");
    difference.assert_acceptable();
    assert_eq!(model.prepare_prefix(prompt.len()), 0);
    let rebuilt = along(&mut model, prompt, tail, 64);
    assert_eq!(want, rebuilt);
}

fn named_readme_request(engine: &mut LocalChatEngine) -> (u64, String) {
    let tool = ToolSpec {
        name: "read_file".into(),
        description: "Read a text file.".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
    };
    let sid = engine.open(spec(vec![tool])).unwrap();
    engine
        .set_tool_choice(sid, ToolChoice::Named("read_file".into()))
        .unwrap();
    let out = engine
        .step(
            sid,
            vec![Message::User("Read README.md.".into())],
            &mut |_| {},
        )
        .unwrap();
    assert!(out.text.is_empty());
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.tool_calls.len(), 1);
    let requested_path = out.tool_calls[0].str_arg("path").unwrap().to_string();
    eprintln!("Named tool returned path {requested_path:?}");
    (sid, requested_path)
}

#[test]
#[ignore = "model task-quality check with the unchanged requested path; verified weights required"]
fn qwen35_named_tool_preserves_the_requested_path() {
    let mut engine = engine(16);
    let (sid, path) = named_readme_request(&mut engine);
    engine.close(sid);
    assert_eq!(path, "README.md", "model selected the wrong requested path");
}

#[test]
#[ignore = "requires the verified pinned Qwen3.5 model"]
fn qwen35_tool_feedback_switch_rewind_compact_and_cancel() {
    let mut engine = engine(16);
    let (sid, _) = named_readme_request(&mut engine);
    let keep = engine.message_count(sid);
    let other = engine.open(spec(vec![])).unwrap();
    engine.set_tool_choice(other, ToolChoice::None).unwrap();
    let _ = engine
        .step(other, vec![Message::User("Say hello.".into())], &mut |_| {})
        .unwrap();
    engine.set_tool_choice(sid, ToolChoice::None).unwrap();
    let out = engine
        .step(
            sid,
            vec![Message::Tool(
                "nosh is a shell with a shared current directory.\n".repeat(100),
            )],
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(out.usage.cached_tokens, 0);
    assert!(out.tool_calls.is_empty());
    engine.rewind(sid, keep + 1).unwrap();
    assert_eq!(engine.message_count(sid), keep + 1);
    let before = engine.context_usage(sid).0;
    assert_eq!(engine.compact_tool_results(sid, 0).unwrap(), 1);
    assert!(engine.context_usage(sid).0 < before);
    engine.set_tool_choice(sid, ToolChoice::None).unwrap();
    let compacted = engine.step(sid, vec![], &mut |_| {}).unwrap();
    assert_eq!(
        compacted.usage.cached_tokens, 0,
        "compaction must rebuild recurrent state"
    );
    let cancel = engine.cancel_handle();
    let long = "shell context ".repeat(100);
    let out = engine
        .step(other, vec![Message::User(long)], &mut |event| {
            if matches!(event, Event::Prefill { .. }) {
                cancel.cancel();
            }
        })
        .unwrap();
    assert_eq!(out.stop, StopReason::Cancelled);
    assert_eq!(
        out.usage.prompt_tokens, 16,
        "unprocessed canceled chunks must not count as prefill work"
    );
    cancel.reset();
    let resumed = engine.step(other, vec![], &mut |_| {}).unwrap();
    assert_eq!(
        resumed.usage.cached_tokens, 16,
        "a fully computed canceled chunk remains valid"
    );
    let mut saw_text = false;
    let out = engine
        .step(
            other,
            vec![Message::User("Say a short sentence about shells.".into())],
            &mut |event| {
                if matches!(event, Event::Text(_)) {
                    saw_text = true;
                    cancel.cancel();
                }
            },
        )
        .unwrap();
    assert!(saw_text);
    assert_eq!(out.stop, StopReason::Cancelled);
    cancel.reset();
    engine.close(sid);
    engine.close(other);
}

#[test]
#[ignore = "requires the verified Qwen model"]
fn qwen35_context_full_rewind_and_thinking_boundaries() {
    let mut engine = engine_with_context(16, 64);
    let sid = engine.open(spec(vec![])).unwrap();
    assert!(matches!(
        engine.step(
            sid,
            vec![Message::User("shell context ".repeat(100))],
            &mut |_| {}
        ),
        Err(nosh_engine::EngineError::ContextFull { max: 64, .. })
    ));
    engine.rewind(sid, 0).unwrap();
    let result = engine
        .step(sid, vec![Message::User("Say hello.".into())], &mut |_| {})
        .unwrap();
    assert!(result.usage.context_used <= 64);
    engine.close(sid);
    let mut spec = spec(vec![]);
    spec.thinking = true;
    spec.max_new_tokens = 8;
    let sid = engine.open(spec).unwrap();
    engine.set_tool_choice(sid, ToolChoice::None).unwrap();
    let result = engine
        .step(sid, vec![Message::User("What is 1+1?".into())], &mut |_| {})
        .unwrap();
    assert!(result.tool_calls.is_empty() && result.errors.is_empty());
    assert!(result.usage.completion_tokens <= 8 && result.usage.context_used <= 64);
    engine.close(sid);
}

#[test]
#[ignore = "requires the verified Qwen tokenizer"]
fn qwen35_official_segment_encoding_matches_whole_text() {
    let (_, path) = paths();
    let mut tok = Tok::load(&path).unwrap();
    let mut segs = template::qwen35::render_system(Some("You are helpful."), &[]);
    segs.extend(template::qwen35::render_user("hello 世界"));
    segs.push(template::qwen35::generation_prompt(false));
    let whole = tok.encode(&template::concat(&segs), true).unwrap();
    assert_eq!(tok.encode_segments(&segs).unwrap(), whole);
}

#[test]
#[ignore = "requires the verified Qwen model; CPU numerical control"]
fn qwen35_f32_control_loads_on_cpu() {
    let (weights, _) = paths();
    let mut model = Qwen35::load(
        &weights,
        64,
        LoadOptions {
            kv_dtype: KvDtype::F32,
            prepack_weights: false,
        },
        &Device::Cpu,
    )
    .unwrap();
    assert!(
        model
            .forward(&[1, 2, 3])
            .unwrap()
            .to_vec1::<f32>()
            .unwrap()
            .iter()
            .all(|v| v.is_finite())
    );
}

fn reference_dir() -> PathBuf {
    PathBuf::from(
        std::env::var_os("NOSH_QWEN_REFERENCE")
            .expect("set NOSH_QWEN_REFERENCE to a new reference artifact directory"),
    )
}

#[test]
#[ignore = "explicitly prepares a verified 8K CLI measurement input from the fixed corpus"]
fn qwen35_export_8k_cli_prompt() {
    use std::io::Write;
    let mut engine = engine(512);
    let tok = engine.tokenizer();
    let source = format!(
        "Summarize this plan:\n\n{}",
        include_str!("../../../tests/fixtures/model_context.txt").repeat(20)
    );
    let ends: Vec<_> = source
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(source.len()))
        .collect();
    let count = |tok: &mut Tok, text: &str| {
        let messages = [
            Message::System("You are a helpful assistant.".into()),
            Message::User(text.into()),
        ];
        tok.encode_segments(&template::qwen35::render_conversation(
            &messages,
            &[],
            true,
            Some(false),
        ))
        .unwrap()
        .len()
    };
    let (mut lo, mut hi) = (0, ends.len() - 1);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if count(tok, &source[..ends[mid]]) < 8064 {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    for &end in &ends[lo..] {
        let prompt = &source[..end];
        let tokens = count(tok, prompt);
        assert!(tokens <= 8127, "fixed corpus has no suitable 8K boundary");
        if tokens >= 8064 && !tokens.is_multiple_of(4) {
            let data = json!({"system":"You are a helpful assistant.","prompt":prompt,"prompt_tokens":tokens,
                "context":8192,"max_new_tokens":64,"seed":0,"kv_dtype":"f16","recurrent_state_dtype":"f32"});
            let encoded = serde_json::to_vec_pretty(&data).unwrap();
            let path = reference_dir().join("cli-8k.json");
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => file.write_all(&encoded).unwrap(),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    assert_eq!(
                        std::fs::read(&path).unwrap(),
                        encoded,
                        "refusing to overwrite an existing measurement input"
                    );
                }
                Err(error) => panic!("{}: {error}", path.display()),
            }
            eprintln!(
                "Verified {tokens}-token native CLI prompt: {}",
                path.display()
            );
            return;
        }
    }
    panic!("fixed corpus is too short for 8K measurement");
}

#[test]
#[ignore = "requires verified Qwen weights and fixed numerical inputs; CPU-only KV/prepack control"]
fn qwen35_kv_and_prepack_match_at_3k_and_8k() {
    let (weights, _) = paths();
    let features = nosh_llm::cpu::features();
    let vnni = cfg!(target_arch = "x86_64")
        && features.contains(&"avx2")
        && (features.contains(&"avxvnni")
            || ["avx512f", "avx512bw", "avx512vl", "avx512vnni"]
                .iter()
                .all(|feature| features.contains(feature)));
    for prompt_len in [3300, 8065] {
        let input: serde_json::Value = serde_json::from_slice(
            &std::fs::read(reference_dir().join(format!("{prompt_len}.json"))).unwrap(),
        )
        .unwrap();
        let ids: Vec<u32> = serde_json::from_value(input["ids"].clone()).unwrap();
        assert_eq!(ids.len(), prompt_len + 48);
        let (prompt, tail) = ids.split_at(prompt_len);
        let mut retained = Qwen35::load(
            &weights,
            8192,
            LoadOptions {
                kv_dtype: KvDtype::F32,
                prepack_weights: false,
            },
            &Device::Cpu,
        )
        .unwrap();
        assert_eq!(retained.prepack_stats().tensors, 0);
        let f32 = along(&mut retained, prompt, tail, 512);
        retained.set_kv_dtype(KvDtype::F16).unwrap();
        let f16 = along(&mut retained, prompt, tail, 512);
        let difference = comparison::divergence(&f32, &f16, tail);
        eprintln!("Qwen {prompt_len} f16/f32 attention KV: {difference}");
        difference.assert_acceptable();
        drop(retained);
        let mut packed =
            Qwen35::load(&weights, 8192, LoadOptions::default(), &Device::Cpu).unwrap();
        assert_eq!(packed.prepack_stats().tensors, if vnni { 187 } else { 0 });
        assert_eq!(packed.prepack_stats().released_bytes > 0, vnni);
        let got = along(&mut packed, prompt, tail, 512);
        let difference = comparison::divergence(&f16, &got, tail);
        eprintln!(
            "Qwen {prompt_len} prepack on/off: {difference}; released {} bytes",
            packed.prepack_stats().released_bytes
        );
        difference.assert_acceptable();
    }
}

#[test]
#[ignore = "explicitly exports fixed numerical reference inputs; tokenizer required"]
fn qwen35_export_reference_inputs() {
    use std::io::Write;
    let (_, tokenizer) = paths();
    let mut tok = Tok::load(&tokenizer).unwrap();
    let text = include_str!("../../../tests/fixtures/model_context.txt").repeat(20);
    let ids = tok.encode(&text, false).unwrap();
    std::fs::create_dir_all(reference_dir()).unwrap();
    for prompt in [3300, 8065] {
        let input = json!({
            "reference_revision": REFERENCE_REVISION,
            "model_sha256": MODEL_SHA256, "tokenizer_sha256": TOKENIZER_SHA256,
            "prompt_tokens": prompt, "tail_tokens": 48, "kv_dtype": "f16",
            "ids": &ids[..prompt + 48],
        });
        let path = reference_dir().join(format!("{prompt}.json"));
        let encoded = serde_json::to_string_pretty(&input).unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match file {
            Ok(mut file) => file.write_all(encoded.as_bytes()).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    encoded.as_bytes(),
                    "refusing to overwrite changed reference input"
                );
            }
            Err(error) => panic!("{}: {error}", path.display()),
        }
        eprintln!("Exported fixed Qwen input {}", path.display());
    }
}

#[test]
#[ignore = "requires verified Qwen weights and pinned llama.cpp teacher-forcing reference artifacts"]
fn qwen35_matches_pinned_reference_at_3k_and_8k() {
    let (weights, _) = paths();
    let dev = device().open().unwrap();
    let mut model = Qwen35::load(&weights, 8192, LoadOptions::default(), &dev).unwrap();
    for prompt_len in [3300, 8065] {
        let input_path = reference_dir().join(format!("{prompt_len}.json"));
        let input: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&input_path).unwrap()).unwrap();
        let ids: Vec<u32> = serde_json::from_value(input["ids"].clone()).unwrap();
        assert_eq!(ids.len(), prompt_len + 48);
        assert_eq!(input["prompt_tokens"], prompt_len);
        assert_eq!(input["tail_tokens"], 48);
        assert_eq!(input["reference_revision"], REFERENCE_REVISION);
        assert_eq!(input["model_sha256"], MODEL_SHA256);
        assert_eq!(input["tokenizer_sha256"], TOKENIZER_SHA256);
        assert_eq!(input["kv_dtype"], "f16");
        let manifest_path = reference_dir().join(format!("{prompt_len}.manifest.json"));
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["backend"], "llama.cpp-reference-only");
        assert_eq!(manifest["revision"], REFERENCE_REVISION);
        assert_eq!(manifest["release"], "b11706");
        assert_eq!(
            manifest["release_archive_sha256"],
            "f426ae6d20afeb6970f51cc182dedc3b21cf23ec278192da1af262de43be6115"
        );
        assert_eq!(
            manifest["library_sha256"],
            "8f64b0cb6ccbe75780261eaae2e12485b8b3939809bd57000ba4e99e428e3217"
        );
        assert_eq!(manifest["model_sha256"], MODEL_SHA256);
        assert_eq!(manifest["kv_dtype"], "f16");
        assert_eq!(manifest["ssm_dtype"], "f32");
        assert_eq!(manifest["prefill_chunk"], 512);
        assert_eq!(manifest["flash_attention"], false);
        assert_eq!(
            manifest["input_sha256"],
            nosh_hub::hash::sha256_file(&input_path, |_| {}).unwrap()
        );
        assert_eq!(manifest["vocab"], 248320);
        assert_eq!(manifest["rows"], 49);
        let logits_path = reference_dir().join(format!("{prompt_len}.f32"));
        assert_eq!(
            manifest["logits_sha256"],
            nosh_hub::hash::sha256_file(&logits_path, |_| {}).unwrap()
        );
        let bytes = std::fs::read(logits_path).unwrap();
        assert_eq!(bytes.len(), 49 * 248320 * 4);
        const ROW_BYTES: usize = 248320 * 4;
        let want: Vec<Vec<f32>> = bytes
            .as_chunks::<ROW_BYTES>()
            .0
            .iter()
            .map(|row| {
                row.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|v| f32::from_le_bytes(*v))
                    .collect()
            })
            .collect();
        let (prompt, tail) = ids.split_at(prompt_len);
        let got = along(&mut model, prompt, tail, 512);
        let difference = comparison::divergence(&want, &got, tail);
        eprintln!("Qwen {prompt_len} + 48 teacher-forced reference: {difference}");
        difference.assert_acceptable();
    }
}
