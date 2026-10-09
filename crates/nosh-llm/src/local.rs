//! In-process [`ChatEngine`] on the local GGUF model.
//!
//! Each session keeps a token-level log: rendered messages plus the raw token
//! ids the model generated for assistant turns (never decoded and
//! re-encoded). Before every step the longest common prefix with the tokens
//! already in the KV cache is kept and only the rest is prefilled, in chunks
//! that can be cancelled.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use crate::conversation::Conversation;
use crate::model::attn::KvDtype;
use crate::model::llama::{Llama, LoadOptions, PrepackStats};
use crate::sampling::Sampler;
use crate::template;
use crate::tokenizer::Tok;
use crate::toolcall::{FUNCTION_OPEN, Parsed, StreamParser};
use crate::{DeviceSelection, InferenceDevice, LlmError};
use nosh_engine::{
    CancelHandle, ChatEngine, EngineError, Event, Message, SamplingParams, SessionId, SessionSpec,
    StepOutcome, StopReason, ToolChoice, Usage,
};

pub const IM_START: u32 = 130_072;
pub const IM_END: u32 = 130_073;
pub const EOS: u32 = 1;

/// Model files and metadata supplied by the host, independent of their store.
#[derive(Debug, Clone, Copy)]
pub struct ModelSource<'a> {
    pub id: &'a str,
    pub arch: &'a str,
    pub weights: &'a Path,
    pub tokenizer: &'a Path,
    pub eog_ids: &'a [u32],
    pub sampling: SamplingParams,
}

fn mask_tool_choice(logits: &mut [f32], choice: &ToolChoice) {
    if *choice == ToolChoice::None {
        // The parser can enter a call only through this special token.
        logits[FUNCTION_OPEN as usize] = f32::NEG_INFINITY;
    }
}

#[derive(Debug, Clone)]
pub struct LocalEngineOptions {
    pub device: InferenceDevice,
    pub context_length: usize,
    pub prefill_chunk: usize,
    pub seed: Option<u64>,
    pub kv_dtype: KvDtype,
    /// See [`LoadOptions::prepack_weights`].
    pub prepack_weights: bool,
}

impl Default for LocalEngineOptions {
    fn default() -> Self {
        let load = LoadOptions::default();
        Self {
            device: InferenceDevice::default(),
            context_length: 8192,
            prefill_chunk: 512,
            seed: None,
            kv_dtype: load.kv_dtype,
            prepack_weights: load.prepack_weights,
        }
    }
}

impl LocalEngineOptions {
    pub fn select_device(
        &self,
        weights: &Path,
    ) -> Result<(candle_core::Device, DeviceSelection), LlmError> {
        self.device.select(
            weights,
            self.context_length,
            self.prefill_chunk,
            self.kv_dtype,
        )
    }
}

#[derive(Debug, Clone)]
pub struct EngineInfo {
    pub device: InferenceDevice,
    pub device_selection: DeviceSelection,
    pub model_id: String,
    pub arch: String,
    pub layers: usize,
    pub vocab: usize,
    pub context: usize,
    pub threads: usize,
    pub load_secs: f64,
    pub device_init_secs: f64,
    pub model_init_secs: f64,
    pub kv_dtype: KvDtype,
    pub prepack: PrepackStats,
}

pub struct LocalChatEngine {
    model: Llama,
    tok: Tok,
    eog: Vec<u32>,
    newline: Vec<u32>,
    sessions: HashMap<SessionId, Conversation>,
    next_id: SessionId,
    kv_tokens: Vec<u32>,
    cancel: CancelHandle,
    opts: LocalEngineOptions,
    info: EngineInfo,
    default_sampling: SamplingParams,
}

fn load_error_context(selection: &DeviceSelection) -> String {
    let mut context = format!(
        "loading on {} (requested {}): {}",
        selection.actual, selection.requested, selection.reason,
    );
    if matches!(selection.actual, InferenceDevice::Cuda(_)) {
        context.push_str("; device memory is not reserved; no automatic retry on another backend");
    }
    context
}

impl LocalChatEngine {
    pub fn load(model: ModelSource<'_>, opts: LocalEngineOptions) -> Result<Self, LlmError> {
        let t0 = Instant::now();
        // The environment is set up by the binary (`configure_thread_env`).
        let threads = barrier_threads();
        let load = LoadOptions {
            kv_dtype: opts.kv_dtype,
            prepack_weights: opts.prepack_weights,
        };
        let (device, device_selection) = opts.select_device(model.weights)?;
        let device_init_secs = t0.elapsed().as_secs_f64();
        let model_start = Instant::now();
        let mut tok = Tok::load(model.tokenizer)?;
        let llama = Llama::load(model.weights, opts.context_length, load, &device)
            .map_err(|error| error.context(load_error_context(&device_selection)))?;
        let cfg = llama.config().clone();
        if cfg.arch != model.arch {
            return Err(LlmError::Config(format!(
                "GGUF architecture {} does not match model metadata ({})",
                cfg.arch, model.arch
            )));
        }
        if tok.vocab_size() != cfg.vocab {
            return Err(LlmError::Config(format!(
                "tokenizer vocab {} does not match model vocab {}",
                tok.vocab_size(),
                cfg.vocab
            )));
        }
        let newline = tok.encode("\n", false)?;
        let default_sampling = SamplingParams {
            seed: opts.seed,
            ..model.sampling
        };
        let info = EngineInfo {
            device: device_selection.actual,
            device_selection,
            model_id: model.id.to_owned(),
            arch: cfg.arch.clone(),
            layers: cfg.n_layer,
            vocab: cfg.vocab,
            context: llama.max_context(),
            threads,
            load_secs: t0.elapsed().as_secs_f64(),
            device_init_secs,
            model_init_secs: model_start.elapsed().as_secs_f64(),
            kv_dtype: llama.kv_dtype(),
            prepack: llama.prepack_stats(),
        };
        Ok(Self {
            model: llama,
            tok,
            eog: model.eog_ids.to_vec(),
            newline,
            sessions: HashMap::new(),
            next_id: 1,
            kv_tokens: Vec::new(),
            cancel: CancelHandle::default(),
            opts,
            info,
            default_sampling,
        })
    }

    pub fn info(&self) -> &EngineInfo {
        &self.info
    }

    /// Host-supplied sampling defaults with the seed from the engine options.
    pub fn default_sampling(&self) -> SamplingParams {
        self.default_sampling
    }

    pub fn tokenizer(&mut self) -> &mut Tok {
        &mut self.tok
    }

    /// Feeds `tokens` after the longest prefix already cached; returns last logits.
    fn prefill(
        &mut self,
        full: &[u32],
        sink: &mut dyn FnMut(Event),
        usage: &mut Usage,
    ) -> Result<Option<candle_core::Tensor>, LlmError> {
        let mut lcp = self
            .kv_tokens
            .iter()
            .zip(full)
            .take_while(|(a, b)| a == b)
            .count();
        if lcp == full.len() {
            lcp = lcp.saturating_sub(1);
        }
        self.model.truncate(lcp);
        self.kv_tokens.truncate(lcp);
        usage.cached_tokens = lcp;
        let todo = &full[lcp..];
        usage.prompt_tokens = todo.len();
        let t0 = Instant::now();
        let mut logits = None;
        let chunk = self.opts.prefill_chunk.max(1);
        for (i, c) in todo.chunks(chunk).enumerate() {
            if self.cancel.is_cancelled() {
                usage.prefill_secs = t0.elapsed().as_secs_f64();
                return Ok(None);
            }
            logits = Some(self.model.forward(c)?);
            self.kv_tokens.extend_from_slice(c);
            if todo.len() > chunk {
                sink(Event::Prefill {
                    done: ((i + 1) * chunk).min(todo.len()),
                    total: todo.len(),
                });
            }
        }
        usage.prefill_secs = t0.elapsed().as_secs_f64();
        Ok(logits)
    }
}

impl ChatEngine for LocalChatEngine {
    fn set_tool_choice(&mut self, sid: SessionId, choice: ToolChoice) -> Result<(), EngineError> {
        let conversation = self
            .sessions
            .get_mut(&sid)
            .ok_or(EngineError::UnknownSession(sid))?;
        if matches!(choice, ToolChoice::Required | ToolChoice::Named(_))
            && (conversation.spec.tools.is_empty() || conversation.spec.thinking)
        {
            return Err(EngineError::Config(
                "required tool choice needs tools and thinking disabled".into(),
            ));
        }
        if let ToolChoice::Named(name) = &choice
            && (!conversation
                .spec
                .tools
                .iter()
                .any(|tool| tool.name == *name)
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c)))
        {
            return Err(EngineError::Config("named tool is not available".into()));
        }
        conversation.tool_choice = choice;
        Ok(())
    }

    fn open(&mut self, spec: SessionSpec) -> Result<SessionId, EngineError> {
        let conversation = Conversation::new(spec, &mut self.tok)?;
        let id = self.next_id;
        self.next_id += 1;
        self.sessions.insert(id, conversation);
        Ok(id)
    }

    fn step(
        &mut self,
        sid: SessionId,
        append: Vec<Message>,
        sink: &mut dyn FnMut(Event),
    ) -> Result<StepOutcome, EngineError> {
        let t_start = Instant::now();
        let conversation = self
            .sessions
            .get_mut(&sid)
            .ok_or(EngineError::UnknownSession(sid))?;
        conversation.append(append, &mut self.tok)?;
        let sampling = conversation.spec.sampling;
        let thinking = conversation.spec.thinking;
        let max_new_tokens = conversation.spec.max_new_tokens;
        let tools = conversation.spec.tools.clone();
        let choice = std::mem::take(&mut conversation.tool_choice);
        let forced_prefix = self
            .tok
            .encode_segments(&template::tool_choice_prefix(&choice))?;
        let mut gen_prompt = self
            .tok
            .encode_segments(&[template::generation_prompt(Some(thinking))])?;
        gen_prompt.extend_from_slice(&forced_prefix);
        let full = conversation.tokens(&gen_prompt);
        let max_ctx = self.model.max_context();
        let mut usage = Usage {
            context_max: max_ctx,
            ..Usage::default()
        };
        if full.len() + 8 > max_ctx {
            return Err(EngineError::ContextFull {
                used: full.len(),
                max: max_ctx,
            });
        }

        let mut outcome = StepOutcome {
            text: String::new(),
            think: String::new(),
            tool_calls: Vec::new(),
            errors: Vec::new(),
            stop: StopReason::Cancelled,
            usage: Usage::default(),
        };
        let Some(mut logits) = self.prefill(&full, sink, &mut usage)? else {
            usage.context_used = self.kv_tokens.len();
            outcome.usage = usage;
            return Ok(outcome);
        };

        let mut sampler = Sampler::new(sampling);
        let mut parser = StreamParser::new(tools, thinking);
        for id in forced_prefix {
            dispatch(parser.push(id, &self.tok), &mut outcome, sink);
        }
        let mut generated: Vec<u32> = Vec::new();
        let t_decode = Instant::now();
        let trace = std::env::var_os("NOSH_TRACE_DECODE").is_some();
        let mut t_win = Instant::now();
        let (mut t_fwd, mut t_samp) = (0f64, 0f64);
        let stop = loop {
            if self.cancel.is_cancelled() {
                break StopReason::Cancelled;
            }
            let ts = Instant::now();
            let mut l: Vec<f32> = logits.to_vec1().map_err(LlmError::from)?;
            mask_tool_choice(&mut l, &choice);
            let id = sampler.sample(&mut l, parser.in_call());
            sampler.observe(id);
            t_samp += ts.elapsed().as_secs_f64();
            if generated.is_empty() {
                usage.ttft_secs = t_start.elapsed().as_secs_f64();
            }
            generated.push(id);
            if self.eog.contains(&id) {
                break StopReason::EndOfTurn;
            }
            dispatch(parser.push(id, &self.tok), &mut outcome, sink);
            if matches!(choice, ToolChoice::Required | ToolChoice::Named(_))
                && (!outcome.tool_calls.is_empty() || !outcome.errors.is_empty())
            {
                break StopReason::EndOfTurn;
            }
            if generated.len() >= max_new_tokens || self.kv_tokens.len() + 2 >= max_ctx {
                break StopReason::MaxTokens;
            }
            let tf = Instant::now();
            logits = self.model.forward(&[id]).map_err(LlmError::from)?;
            t_fwd += tf.elapsed().as_secs_f64();
            self.kv_tokens.push(id);
            if trace && generated.len().is_multiple_of(16) {
                eprintln!(
                    "[decode kv={} {:.1} ms/tok (forward {:.1} ms, sample {:.1} ms)]",
                    self.kv_tokens.len(),
                    t_win.elapsed().as_secs_f64() * 1000.0 / 16.0,
                    t_fwd * 1000.0 / 16.0,
                    t_samp * 1000.0 / 16.0
                );
                t_win = Instant::now();
                t_fwd = 0.0;
                t_samp = 0.0;
            }
        };
        dispatch(parser.finish(), &mut outcome, sink);
        usage.decode_secs = t_decode.elapsed().as_secs_f64();
        usage.completion_tokens = generated.len();

        // Close the turn the way the template does: `<|im_end|>\n`.
        if matches!(generated.last(), Some(&EOS)) {
            generated.pop();
        }
        if generated.last() != Some(&IM_END) {
            generated.push(IM_END);
        }
        let mut raw = gen_prompt;
        raw.extend_from_slice(&generated);
        raw.extend_from_slice(&self.newline);
        let conversation = self
            .sessions
            .get_mut(&sid)
            .ok_or(EngineError::UnknownSession(sid))?;
        conversation.push_assistant(raw);
        usage.context_used = conversation.token_count();
        outcome.stop = stop;
        outcome.usage = usage;
        Ok(outcome)
    }

    fn rewind(&mut self, sid: SessionId, keep: usize) -> Result<(), EngineError> {
        self.sessions
            .get_mut(&sid)
            .ok_or(EngineError::UnknownSession(sid))?
            .rewind(keep, &mut self.tok)
            .map_err(Into::into)
    }

    fn message_count(&self, sid: SessionId) -> usize {
        self.sessions
            .get(&sid)
            .map(Conversation::message_count)
            .unwrap_or(0)
    }

    fn compact_tool_results(
        &mut self,
        sid: SessionId,
        keep_recent: usize,
    ) -> Result<usize, EngineError> {
        self.sessions
            .get_mut(&sid)
            .ok_or(EngineError::UnknownSession(sid))?
            .compact_tool_results(keep_recent, &mut self.tok)
            .map_err(Into::into)
    }

    fn context_usage(&self, sid: SessionId) -> (usize, usize) {
        let used = self
            .sessions
            .get(&sid)
            .map(Conversation::token_count)
            .unwrap_or(0);
        (used, self.model.max_context())
    }

    fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    fn close(&mut self, sid: SessionId) {
        self.sessions.remove(&sid);
    }
}

/// Environment variables nosh sets for its own inference threads, with the
/// values they had before (so a shell can keep them out of child processes).
static ENV_OVERRIDES: OnceLock<Vec<(&'static str, Option<String>)>> = OnceLock::new();

/// Candle runs quantized matmuls on its own barrier pool (physical cores);
/// rayon-parallel ops in between (attention GEMMs, softmax, norms) fight that
/// pool for the same cores and made decode ~3× slower in measurements. Keep
/// rayon to one thread and give the barrier pool the physical cores.
/// Returns the barrier-pool thread count.
fn barrier_threads() -> usize {
    std::env::var("CANDLE_NUM_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(num_cpus::get_physical)
        .max(1)
}

/// Puts nosh's inference thread counts into the process environment, where
/// candle and rayon read them: `CANDLE_NUM_THREADS` (the physical cores
/// unless set) for candle's barrier pool and `RAYON_NUM_THREADS` (1, or
/// `NOSH_RAYON_THREADS`) for rayon. The previous values are kept for
/// [`env_overrides`]. A binary calls this first thing in `main`.
///
/// # Safety
///
/// Changing the environment is only sound while no other thread runs (one
/// could be reading it), so no thread may have been started yet.
pub unsafe fn configure_thread_env() {
    #[cfg(target_os = "linux")]
    debug_assert_eq!(
        std::fs::read_dir("/proc/self/task").map_or(1, |d| d.count()),
        1,
        "configure_thread_env must run before any thread starts"
    );
    ENV_OVERRIDES.get_or_init(|| {
        let n = barrier_threads();
        let saved = vec![
            (
                "CANDLE_NUM_THREADS",
                std::env::var("CANDLE_NUM_THREADS").ok(),
            ),
            ("RAYON_NUM_THREADS", std::env::var("RAYON_NUM_THREADS").ok()),
        ];
        // SAFETY: the caller guarantees that no other thread exists.
        unsafe {
            std::env::set_var("CANDLE_NUM_THREADS", n.to_string());
            std::env::set_var(
                "RAYON_NUM_THREADS",
                std::env::var("NOSH_RAYON_THREADS").unwrap_or_else(|_| "1".into()),
            );
        }
        saved
    });
}

/// `(name, original value)` for every process variable the engine overrode.
pub fn env_overrides() -> &'static [(&'static str, Option<String>)] {
    ENV_OVERRIDES.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Forwards parsed output to the sink and accumulates it in the outcome.
pub(crate) fn dispatch(
    parsed: Vec<Parsed>,
    outcome: &mut StepOutcome,
    sink: &mut dyn FnMut(Event),
) {
    for p in parsed {
        match p {
            Parsed::Text(t) => {
                outcome.text.push_str(&t);
                sink(Event::Text(t));
            }
            Parsed::Think(t) => {
                outcome.think.push_str(&t);
                sink(Event::Think(t));
            }
            Parsed::Call(Ok(c)) => {
                outcome.tool_calls.push(c.clone());
                sink(Event::ToolCall(c));
            }
            Parsed::Call(Err(e)) => {
                outcome.errors.push(e.clone());
                sink(Event::CallError(e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_tool_choice_masks_calls_throughout_greedy_and_sampled_text() {
        let text_id = 24;
        for temperature in [0.0, 1.0] {
            for seed in 0..16 {
                for program in [
                    "printf '%s\\n' ready",
                    "for f in *.txt; do\n cat \"$f\"\ndone",
                    "if test -d src; then\n ls src\nfi",
                    "[None]",
                ] {
                    let mut sampler = Sampler::new(SamplingParams {
                        temperature,
                        top_p: 1.0,
                        seed: Some(seed),
                        ..SamplingParams::default()
                    });
                    let mut parser = StreamParser::new(vec![], false);
                    let mut parsed = Vec::new();
                    for byte in program.bytes() {
                        let mut logits = vec![f32::NEG_INFINITY; 32];
                        logits[FUNCTION_OPEN as usize] = 100.0;
                        logits[text_id as usize] = 0.0;
                        mask_tool_choice(&mut logits, &ToolChoice::None);
                        let id = sampler.sample(&mut logits, parser.in_call());
                        sampler.observe(id);
                        assert_eq!(id, text_id);
                        parsed.extend(parser.push_bytes(id, &[byte]));
                    }
                    parsed.extend(parser.finish());
                    assert!(parsed.iter().all(|part| matches!(part, Parsed::Text(_))));
                    let actual: String = parsed
                        .iter()
                        .map(|part| match part {
                            Parsed::Text(text) => text.as_str(),
                            _ => unreachable!(),
                        })
                        .collect();
                    assert_eq!(actual, program);
                }
            }
        }
    }

    #[test]
    fn none_tool_choice_changes_only_the_call_opening_logit() {
        let logits: Vec<f32> = (0..32).map(|id| id as f32).collect();
        for choice in [
            ToolChoice::Auto,
            ToolChoice::Required,
            ToolChoice::Named("read_file".into()),
            ToolChoice::None,
        ] {
            let mut masked = logits.clone();
            mask_tool_choice(&mut masked, &choice);
            for (id, (&before, &after)) in logits.iter().zip(&masked).enumerate() {
                assert_eq!(
                    after,
                    if choice == ToolChoice::None && id == FUNCTION_OPEN as usize {
                        f32::NEG_INFINITY
                    } else {
                        before
                    }
                );
            }
        }
        assert!(template::tool_choice_prefix(&ToolChoice::None).is_empty());
        assert_eq!(ToolChoice::default(), ToolChoice::Auto);
        assert_eq!(
            serde_json::to_value(ToolChoice::None).unwrap(),
            serde_json::json!({"type":"none"})
        );
        assert_eq!(
            serde_json::from_value::<ToolChoice>(serde_json::json!({"type":"none"})).unwrap(),
            ToolChoice::None
        );
    }

    #[test]
    fn literal_tool_markup_is_still_text_not_a_decoded_call() {
        let text = "<function name=\"read_file\"><param name=\"path\">note.txt</param></function>";
        let mut parser = StreamParser::new(vec![], false);
        let mut parsed = parser.push_bytes(24, text.as_bytes());
        parsed.extend(parser.finish());
        assert_eq!(parsed, [Parsed::Text(text.into())]);
    }

    #[test]
    fn load_failure_context_only_describes_cuda_memory_for_cuda() {
        for requested in [InferenceDevice::Cpu, InferenceDevice::Auto] {
            let selection = DeviceSelection {
                requested,
                actual: InferenceDevice::Cpu,
                reason: "CPU selection reason".into(),
                required_cuda_bytes: None,
                free_cuda_bytes: None,
            };
            let context = load_error_context(&selection);
            assert!(context.contains(&format!("loading on cpu (requested {requested})")));
            assert!(context.contains(&selection.reason));
            assert!(!context.contains("memory is not reserved"));
            assert!(!context.contains("retry"));
        }
        for requested in [InferenceDevice::Auto, InferenceDevice::Cuda(1)] {
            let selection = DeviceSelection {
                requested,
                actual: InferenceDevice::Cuda(1),
                reason: "CUDA selection reason".into(),
                required_cuda_bytes: None,
                free_cuda_bytes: None,
            };
            let context = load_error_context(&selection);
            assert!(context.contains(&format!("loading on cuda:1 (requested {requested})")));
            assert!(context.contains(&selection.reason));
            assert!(context.contains("memory is not reserved"));
            assert!(context.contains("no automatic retry"));
        }
    }

    #[test]
    fn unsupported_cuda_kv_fails_before_device_or_model_io() {
        let model = ModelSource {
            id: "missing-model",
            arch: "llama",
            weights: Path::new("missing-model.gguf"),
            tokenizer: Path::new("missing-tokenizer.json"),
            eog_ids: &[EOS],
            sampling: SamplingParams::default(),
        };
        let error = LocalChatEngine::load(
            model,
            LocalEngineOptions {
                device: InferenceDevice::Cuda(0),
                kv_dtype: KvDtype::F32,
                ..LocalEngineOptions::default()
            },
        )
        .err()
        .expect("unsupported combination")
        .to_string();
        assert!(error.contains("only f16 KV"), "{error}");
    }

    #[test]
    fn loading_reads_the_thread_count_without_changing_the_environment() {
        let before: Vec<_> = std::env::vars_os().collect();
        assert!(barrier_threads() >= 1);
        assert!(env_overrides().is_empty(), "only a binary's main sets them");
        assert_eq!(std::env::vars_os().collect::<Vec<_>>(), before);
    }
}
