//! Opt-in, private JSONL observations at the engine boundary. Never stdout.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use nosh_llm::{
    CancelHandle, ChatEngine, Event, LlmError, Message, SessionId, SessionSpec, StepOutcome,
    StopReason,
};
use serde_json::{Value, json};

static NEXT_ENGINE: AtomicU64 = AtomicU64::new(1);

pub fn wrap(
    engine: Box<dyn ChatEngine>,
    path: Option<&Path>,
    metadata: Value,
) -> io::Result<Box<dyn ChatEngine>> {
    let Some(path) = path else {
        return Ok(engine);
    };
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts.open(path)?;
    let stat = file.metadata()?;
    if !stat.is_file() {
        return Err(io::Error::other("trace path is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if stat.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other("trace file must be private (mode 0600)"));
        }
    }
    let mut traced = TracedEngine::new(engine, file);
    traced.record(json!({"ev": "engine", "info": metadata}))?;
    Ok(Box::new(traced))
}

struct TracedEngine<W: Write> {
    inner: Box<dyn ChatEngine>,
    writer: W,
    id: u64,
    failed: Option<String>,
}

impl<W: Write> TracedEngine<W> {
    fn new(inner: Box<dyn ChatEngine>, writer: W) -> Self {
        Self {
            inner,
            writer,
            id: NEXT_ENGINE.fetch_add(1, Ordering::Relaxed),
            failed: None,
        }
    }

    fn record(&mut self, mut value: Value) -> io::Result<()> {
        if let Some(error) = &self.failed {
            return Err(io::Error::other(error.clone()));
        }
        value["schema_version"] = json!(1);
        value["engine"] = json!(self.id);
        let result = (|| {
            serde_json::to_writer(&mut self.writer, &value)?;
            self.writer.write_all(b"\n")?;
            self.writer.flush()
        })();
        if let Err(error) = &result {
            self.failed = Some(error.to_string());
        }
        result
    }

    // These trait methods cannot return an I/O error. Report it now and make
    // subsequent fallible operations fail instead of silently losing evidence.
    fn record_infallible(&mut self, value: Value) {
        if let Err(error) = self.record(value) {
            eprintln!("nosh: evaluation trace: {error}");
        }
    }
}

fn message(m: &Message) -> Value {
    match m {
        Message::System(s) => json!({"role": "system", "text": s}),
        Message::User(s) => json!({"role": "user", "text": s}),
        Message::Tool(s) => json!({"role": "tool", "text": s}),
        Message::Assistant {
            content,
            tool_calls,
        } => json!({"role": "assistant", "text": content, "tool_calls": tool_calls}),
    }
}

impl<W: Write> ChatEngine for TracedEngine<W> {
    fn open(&mut self, spec: SessionSpec) -> Result<SessionId, LlmError> {
        let p = spec.sampling;
        let mut event = json!({
            "ev": "open", "system": spec.system, "tools": spec.tools,
            "thinking": spec.thinking, "max_new_tokens": spec.max_new_tokens,
            "sampling": {
                "seed": p.seed, "temperature": p.temperature, "top_p": p.top_p,
                "min_p": p.min_p, "repetition_penalty": p.repetition_penalty,
                "tool_call_temperature": p.tool_call_temperature,
            },
        });
        let sid = self.inner.open(spec)?;
        event["sid"] = json!(sid);
        self.record(event)?;
        Ok(sid)
    }

    fn step(
        &mut self,
        sid: SessionId,
        append: Vec<Message>,
        sink: &mut dyn FnMut(Event),
    ) -> Result<StepOutcome, LlmError> {
        self.record(json!({
            "ev": "step_start", "sid": sid,
            "messages": append.iter().map(message).collect::<Vec<_>>(),
        }))?;
        let result = self.inner.step(sid, append, sink);
        match &result {
            Ok(out) => {
                let u = &out.usage;
                self.record(json!({
                    "ev": "step_end", "sid": sid,
                    "text": out.text, "think": out.think, "tool_calls": out.tool_calls,
                    "errors": out.errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "stop": match out.stop {
                        StopReason::EndOfTurn => "end_of_turn",
                        StopReason::MaxTokens => "max_tokens",
                        StopReason::Cancelled => "cancelled",
                    },
                    "usage": {
                        "prompt_tokens": u.prompt_tokens, "cached_tokens": u.cached_tokens,
                        "completion_tokens": u.completion_tokens,
                        "prefill_s": u.prefill_secs, "decode_s": u.decode_secs,
                        "ttft_s": u.ttft_secs,
                        "context_used": u.context_used, "context_max": u.context_max,
                    },
                }))?;
            }
            Err(error) => {
                self.record(json!({"ev": "step_error", "sid": sid, "error": error.to_string()}))?;
            }
        }
        result
    }

    fn rewind(&mut self, sid: SessionId, keep: usize) -> Result<(), LlmError> {
        self.record(json!({"ev": "rewind", "sid": sid, "keep": keep}))?;
        self.inner.rewind(sid, keep)
    }

    fn message_count(&self, sid: SessionId) -> usize {
        self.inner.message_count(sid)
    }

    fn compact_tool_results(&mut self, sid: SessionId, keep_recent: usize) -> usize {
        let changed = self.inner.compact_tool_results(sid, keep_recent);
        self.record_infallible(json!({
            "ev": "compact", "sid": sid, "keep_recent": keep_recent, "changed": changed,
        }));
        changed
    }

    fn context_usage(&self, sid: SessionId) -> (usize, usize) {
        self.inner.context_usage(sid)
    }

    fn cancel_handle(&self) -> CancelHandle {
        self.inner.cancel_handle()
    }

    fn cancel(&self, sid: SessionId) {
        self.inner.cancel(sid);
    }

    fn close(&mut self, sid: SessionId) {
        self.inner.close(sid);
        self.record_infallible(json!({"ev": "close", "sid": sid}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosh_llm::{MockChatEngine, SamplingParams, mock};
    use std::io::Cursor;

    fn spec() -> SessionSpec {
        SessionSpec {
            system: "system".into(),
            tools: vec![],
            thinking: false,
            sampling: SamplingParams {
                seed: Some(42),
                ..SamplingParams::default()
            },
            max_new_tokens: 16,
        }
    }

    #[test]
    fn forwards_messages_events_usage_and_session_operations() {
        let mock = MockChatEngine::new(vec![vec![
            mock::text("hello"),
            mock::call("run_command", json!({"command": "echo ok"})),
        ]]);
        let received = mock.received();
        let specs = mock.specs();
        let mut engine = TracedEngine::new(Box::new(mock), Cursor::new(Vec::new()));
        let sid = engine.open(spec()).unwrap();
        let mut events = vec![];
        let out = engine
            .step(sid, vec![Message::User("input".into())], &mut |e| {
                events.push(e)
            })
            .unwrap();
        assert_eq!(out.text, "hello");
        assert_eq!(events.len(), 2);
        assert_eq!(specs.lock().unwrap()[0].sampling.seed, Some(42));
        assert_eq!(
            received.lock().unwrap()[0],
            vec![Message::User("input".into())]
        );
        assert_eq!(engine.message_count(sid), 2);
        assert_eq!(engine.context_usage(sid).0, out.usage.context_used);
        assert_eq!(engine.compact_tool_results(sid, 1), 0);
        engine.rewind(sid, 0).unwrap();
        assert_eq!(engine.message_count(sid), 0);
        engine.cancel(sid);
        assert!(engine.cancel_handle().is_cancelled());
        engine.close(sid);
        let data = String::from_utf8(engine.writer.into_inner()).unwrap();
        let rows: Vec<Value> = data
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(rows[1]["messages"][0]["text"], "input");
        assert_eq!(
            rows[2]["usage"]["completion_tokens"],
            out.usage.completion_tokens
        );
        assert_eq!(rows[2]["usage"]["ttft_s"], out.usage.ttft_secs);
        assert_eq!(rows.last().unwrap()["ev"], "close");
        assert!(rows.iter().all(|v| v["schema_version"] == 1));
    }

    #[test]
    fn disabled_wrapper_does_not_create_observations() {
        let mut engine = wrap(Box::new(MockChatEngine::new(vec![])), None, json!({})).unwrap();
        let sid = engine.open(spec()).unwrap();
        assert_eq!(engine.step(sid, vec![], &mut |_| {}).unwrap().text, "done");
    }

    #[test]
    fn trace_failure_is_an_error_before_generation() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("trace disk full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mock = MockChatEngine::new(vec![]);
        let received = mock.received();
        let mut engine = TracedEngine::new(Box::new(mock), Broken);
        assert!(
            engine
                .open(spec())
                .unwrap_err()
                .to_string()
                .contains("trace disk full")
        );
        assert!(engine.step(1, vec![], &mut |_| {}).is_err());
        assert!(received.lock().unwrap().is_empty());
    }

    #[test]
    fn engine_errors_are_not_replaced_with_success() {
        let mut engine = TracedEngine::new(
            Box::new(MockChatEngine::new(vec![])),
            Cursor::new(Vec::new()),
        );
        assert!(matches!(
            engine.step(999, vec![], &mut |_| {}),
            Err(LlmError::UnknownSession(999))
        ));
        let data = String::from_utf8(engine.writer.into_inner()).unwrap();
        assert!(data.contains("step_error"));
    }

    #[cfg(unix)]
    #[test]
    fn trace_files_are_private_and_symlinks_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = std::env::temp_dir().join(format!(
            "nosh-eval-trace-{}-{}",
            std::process::id(),
            NEXT_ENGINE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("trace.jsonl");
        let make = || -> Box<dyn ChatEngine> { Box::new(MockChatEngine::new(vec![])) };
        drop(wrap(make(), Some(&path), json!({})).unwrap());
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o077, 0);
        let original = std::fs::read(&path).unwrap();
        let link = dir.join("link");
        symlink(&path, &link).unwrap();
        assert!(wrap(make(), Some(&link), json!({})).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(wrap(make(), Some(&path), json!({})).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
