//! In-process [`ChatEngine`] on the local GGUF model.
//!
//! Each session keeps a token-level log: rendered messages plus the raw token
//! ids the model generated for assistant turns (never decoded and
//! re-encoded). Before every step the longest common prefix with the tokens
//! already in the KV cache is kept and only the rest is prefilled, in chunks
//! that can be cancelled.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use candle_core::Device;

use crate::LlmError;
use crate::engine::{
    CancelHandle, ChatEngine, Event, Message, SamplingParams, SessionId, SessionSpec, StepOutcome,
    StopReason, Usage,
};
use crate::model::attn::KvDtype;
use crate::model::llama::{Llama, LoadOptions, PrepackStats};
use crate::sampling::Sampler;
use crate::template;
use crate::tokenizer::Tok;
use crate::toolcall::{Parsed, StreamParser};

pub const IM_START: u32 = 130_072;
pub const IM_END: u32 = 130_073;
pub const EOS: u32 = 1;

#[derive(Debug, Clone)]
pub struct LocalEngineOptions {
    pub context_length: usize,
    pub prefill_chunk: usize,
    pub seed: Option<u64>,
    pub kv_dtype: KvDtype,
    /// See [`LoadOptions::prepack_q4k`].
    pub prepack_q4k: bool,
}

impl Default for LocalEngineOptions {
    fn default() -> Self {
        let load = LoadOptions::default();
        Self {
            context_length: 8192,
            prefill_chunk: 512,
            seed: None,
            kv_dtype: load.kv_dtype,
            prepack_q4k: load.prepack_q4k,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineInfo {
    pub model_id: String,
    pub arch: String,
    pub layers: usize,
    pub vocab: usize,
    pub context: usize,
    pub threads: usize,
    pub load_secs: f64,
    pub kv_dtype: KvDtype,
    pub prepack: PrepackStats,
}

#[derive(Debug, Clone)]
enum Entry {
    User(String),
    Tools(Vec<String>),
    /// Generation prompt + generated ids + `<|im_end|>\n`.
    Assistant {
        raw: Vec<u32>,
    },
}

impl Entry {
    fn message_count(&self) -> usize {
        match self {
            Entry::Tools(t) => t.len(),
            _ => 1,
        }
    }
}

struct Session {
    spec: SessionSpec,
    prefix: Vec<u32>,
    entries: Vec<(Entry, Vec<u32>)>,
}

impl Session {
    fn tokens(&self) -> Vec<u32> {
        let mut t = self.prefix.clone();
        for (_, toks) in &self.entries {
            t.extend_from_slice(toks);
        }
        t
    }

    fn len(&self) -> usize {
        self.prefix.len() + self.entries.iter().map(|(_, t)| t.len()).sum::<usize>()
    }
}

pub struct LocalChatEngine {
    model: Llama,
    tok: Tok,
    eog: Vec<u32>,
    newline: Vec<u32>,
    sessions: HashMap<SessionId, Session>,
    next_id: SessionId,
    kv_tokens: Vec<u32>,
    cancel: CancelHandle,
    opts: LocalEngineOptions,
    info: EngineInfo,
    default_sampling: SamplingParams,
}

impl LocalChatEngine {
    pub fn load(
        model: &nosh_hub::ResolvedModel,
        opts: LocalEngineOptions,
    ) -> Result<Self, LlmError> {
        let t0 = Instant::now();
        // The environment is set up by the binary (`configure_thread_env`).
        let threads = barrier_threads();
        let device = Device::Cpu;
        let mut tok = Tok::load(&model.tokenizer)?;
        let llama = Llama::load(
            &model.weights,
            opts.context_length,
            LoadOptions {
                kv_dtype: opts.kv_dtype,
                prepack_q4k: opts.prepack_q4k,
            },
            &device,
        )?;
        let cfg = llama.config().clone();
        if cfg.arch != model.entry.arch {
            return Err(LlmError::Config(format!(
                "GGUF architecture {} does not match registry ({})",
                cfg.arch, model.entry.arch
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
        let s = &model.entry.sampling;
        let default_sampling = SamplingParams {
            temperature: s.temperature,
            top_p: s.top_p,
            min_p: s.min_p,
            seed: opts.seed,
            ..SamplingParams::default()
        };
        let info = EngineInfo {
            model_id: model.entry.id.clone(),
            arch: cfg.arch.clone(),
            layers: cfg.n_layer,
            vocab: cfg.vocab,
            context: llama.max_context(),
            threads,
            load_secs: t0.elapsed().as_secs_f64(),
            kv_dtype: llama.kv_dtype(),
            prepack: llama.prepack_stats(),
        };
        Ok(Self {
            model: llama,
            tok,
            eog: model.entry.eog_ids.clone(),
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

    /// Registry sampling defaults (temperature / top-p / min-p) with the seed.
    pub fn default_sampling(&self) -> SamplingParams {
        self.default_sampling
    }

    pub fn tokenizer(&mut self) -> &mut Tok {
        &mut self.tok
    }

    fn encode_entry(&mut self, e: &Entry) -> Result<Vec<u32>, LlmError> {
        match e {
            Entry::User(u) => self.tok.encode_segments(&template::render_user(u)),
            Entry::Tools(t) => {
                let refs: Vec<&str> = t.iter().map(String::as_str).collect();
                self.tok
                    .encode_segments(&template::render_tool_results(&refs))
            }
            Entry::Assistant { raw, .. } => Ok(raw.clone()),
        }
    }

    fn push_messages(&mut self, sid: SessionId, append: Vec<Message>) -> Result<(), LlmError> {
        let mut new_entries: Vec<Entry> = Vec::new();
        for m in append {
            match m {
                Message::System(_) => {
                    return Err(LlmError::Config("system messages are set at open()".into()));
                }
                Message::User(u) => new_entries.push(Entry::User(u)),
                Message::Tool(t) => match new_entries.last_mut() {
                    Some(Entry::Tools(v)) => v.push(t),
                    _ => new_entries.push(Entry::Tools(vec![t])),
                },
                Message::Assistant {
                    content,
                    tool_calls,
                } => {
                    let raw = self
                        .tok
                        .encode_segments(&template::render_assistant(&content, &tool_calls))?;
                    new_entries.push(Entry::Assistant { raw });
                }
            }
        }
        for e in new_entries {
            let toks = self.encode_entry(&e)?;
            self.sessions
                .get_mut(&sid)
                .ok_or(LlmError::UnknownSession(sid))?
                .entries
                .push((e, toks));
        }
        Ok(())
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
    fn open(&mut self, spec: SessionSpec) -> Result<SessionId, LlmError> {
        let segs = template::render_system(Some(&spec.system), &spec.tools);
        let prefix = self.tok.encode_segments(&segs)?;
        let id = self.next_id;
        self.next_id += 1;
        self.sessions.insert(
            id,
            Session {
                spec,
                prefix,
                entries: Vec::new(),
            },
        );
        Ok(id)
    }

    fn step(
        &mut self,
        sid: SessionId,
        append: Vec<Message>,
        sink: &mut dyn FnMut(Event),
    ) -> Result<StepOutcome, LlmError> {
        let t_start = Instant::now();
        self.push_messages(sid, append)?;
        let (spec, mut full) = {
            let s = self
                .sessions
                .get(&sid)
                .ok_or(LlmError::UnknownSession(sid))?;
            (s.spec.clone(), s.tokens())
        };
        let gen_prompt = self
            .tok
            .encode_segments(&[template::generation_prompt(Some(spec.thinking))])?;
        full.extend_from_slice(&gen_prompt);
        let max_ctx = self.model.max_context();
        let mut usage = Usage {
            context_max: max_ctx,
            ..Usage::default()
        };
        if full.len() + 8 > max_ctx {
            return Err(LlmError::ContextFull {
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

        let mut sampler = Sampler::new(spec.sampling);
        let mut parser = StreamParser::new(spec.tools.clone(), spec.thinking);
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
            let mut l: Vec<f32> = logits.to_vec1()?;
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
            if generated.len() >= spec.max_new_tokens || self.kv_tokens.len() + 2 >= max_ctx {
                break StopReason::MaxTokens;
            }
            let tf = Instant::now();
            logits = self.model.forward(&[id])?;
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
        let entry = Entry::Assistant { raw: raw.clone() };
        if let Some(s) = self.sessions.get_mut(&sid) {
            s.entries.push((entry, raw));
            usage.context_used = s.len();
        }
        outcome.stop = stop;
        outcome.usage = usage;
        Ok(outcome)
    }

    fn rewind(&mut self, sid: SessionId, keep: usize) -> Result<(), LlmError> {
        let s = self
            .sessions
            .get_mut(&sid)
            .ok_or(LlmError::UnknownSession(sid))?;
        let mut count = 0;
        let mut cut = s.entries.len();
        let mut split: Option<(usize, usize)> = None;
        for (i, (e, _)) in s.entries.iter().enumerate() {
            let n = e.message_count();
            if count + n > keep {
                cut = i;
                if count < keep {
                    split = Some((i, keep - count));
                }
                break;
            }
            count += n;
        }
        let tail = s.entries.split_off(cut);
        if let Some((_, n)) = split
            && let Some((Entry::Tools(t), _)) = tail.into_iter().next()
        {
            let e = Entry::Tools(t.into_iter().take(n).collect());
            let toks = self.encode_entry(&e)?;
            self.sessions.get_mut(&sid).unwrap().entries.push((e, toks));
        }
        Ok(())
    }

    fn message_count(&self, sid: SessionId) -> usize {
        self.sessions
            .get(&sid)
            .map(|s| s.entries.iter().map(|(e, _)| e.message_count()).sum())
            .unwrap_or(0)
    }

    fn compact_tool_results(&mut self, sid: SessionId, keep_recent: usize) -> usize {
        let Some(s) = self.sessions.get(&sid) else {
            return 0;
        };
        let total: usize = s.entries.iter().map(|(e, _)| e.message_count()).sum();
        let mut seen = 0;
        let mut changes = Vec::new();
        for (i, (e, _)) in s.entries.iter().enumerate() {
            let n = e.message_count();
            if seen + n + keep_recent <= total
                && let Entry::Tools(t) = e
            {
                let shortened: Vec<String> = t.iter().map(|c| shorten_tool_result(c)).collect();
                if &shortened != t {
                    changes.push((i, Entry::Tools(shortened)));
                }
            }
            seen += n;
        }
        let count = changes.len();
        for (i, e) in changes {
            if let Ok(toks) = self.encode_entry(&e) {
                self.sessions.get_mut(&sid).unwrap().entries[i] = (e, toks);
            }
        }
        count
    }

    fn context_usage(&self, sid: SessionId) -> (usize, usize) {
        let used = self.sessions.get(&sid).map(Session::len).unwrap_or(0);
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

/// One-line stand-in for an old tool result (keeps the status header).
pub fn shorten_tool_result(content: &str) -> String {
    const KEEP: usize = 200;
    if content.chars().count() <= KEEP + 40 {
        return content.to_string();
    }
    let head: String = content.chars().take(KEEP).collect();
    format!(
        "{head}\n[… older output omitted to save context ({} chars)]",
        content.chars().count()
    )
}

/// Resident set size of this process in MB (Linux), current and peak.
pub fn rss_mb() -> Option<(f64, f64)> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| -> Option<f64> {
        s.lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse::<f64>()
            .ok()
            .map(|kb| kb / 1024.0)
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_reads_the_thread_count_without_changing_the_environment() {
        let before: Vec<_> = std::env::vars_os().collect();
        assert!(barrier_threads() >= 1);
        assert!(env_overrides().is_empty(), "only a binary's main sets them");
        assert_eq!(std::env::vars_os().collect::<Vec<_>>(), before);
    }
}
