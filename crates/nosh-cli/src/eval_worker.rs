//! Evaluation-only, one client at a time. Shells and tool hosts stay in the CLI.
//! The connection owns its conversations; only model state and the exact-token
//! prefix cache survive disconnect. No network listener, retries or fallback.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use nosh_llm::{
    CancelHandle, ChatEngine, Event, LlmError, Message, SessionId, SessionSpec, StepOutcome,
    ToolChoice,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const VERSION: u32 = 1;
const FRAME_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Hello {
        version: u32,
        config: Value,
    },
    Status,
    Open {
        spec: SessionSpec,
    },
    Step {
        sid: SessionId,
        append: Vec<Message>,
    },
    Choice {
        sid: SessionId,
        choice: ToolChoice,
    },
    Rewind {
        sid: SessionId,
        keep: usize,
    },
    Compact {
        sid: SessionId,
        keep_recent: usize,
    },
    Close {
        sid: SessionId,
    },
    Cancel,
}

#[derive(Debug, Serialize, Deserialize)]
enum Fault {
    ContextFull { used: usize, max: usize },
    UnknownSession(SessionId),
    Failed(String),
}

impl From<LlmError> for Fault {
    fn from(error: LlmError) -> Self {
        match error {
            LlmError::ContextFull { used, max } => Self::ContextFull { used, max },
            LlmError::UnknownSession(sid) => Self::UnknownSession(sid),
            error => Self::Failed(error.to_string()),
        }
    }
}

impl From<Fault> for LlmError {
    fn from(error: Fault) -> Self {
        match error {
            Fault::ContextFull { used, max } => Self::ContextFull { used, max },
            Fault::UnknownSession(sid) => Self::UnknownSession(sid),
            Fault::Failed(error) => Self::Config(format!("evaluation worker: {error}")),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    messages: usize,
    context: (usize, usize),
}

#[derive(Debug, Serialize, Deserialize)]
struct Answer {
    sid: SessionId,
    state: State,
    outcome: Option<StepOutcome>,
    changed: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct Failure {
    error: Fault,
    session: Option<(SessionId, State)>,
}

#[derive(Debug, Serialize, Deserialize)]
enum Reply {
    Hello {
        version: u32,
        info: Value,
        description: String,
    },
    Status(Value),
    Event(Event),
    Done(Result<Answer, Failure>),
}

fn write_frame(stream: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() >= FRAME_LIMIT {
        return Err(io::Error::other("evaluation frame exceeds limit"));
    }
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()
}

fn read_request(reader: &mut BufReader<UnixStream>) -> io::Result<Request> {
    let mut bytes = Vec::new();
    reader
        .take(FRAME_LIMIT as u64)
        .read_until(b'\n', &mut bytes)?;
    if bytes.last() != Some(&b'\n') {
        return Err(io::Error::other(
            "evaluation client disconnected or frame exceeds limit",
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn configuration(setup: &crate::engine::EngineSetup) -> Result<Value, String> {
    let model = crate::engine::locate(setup)
        .map_err(|error| error.to_string())?
        .ok_or("evaluation worker requires an installed model")?;
    let opts = nosh_llm::LocalEngineOptions::default();
    Ok(json!({
        "model": model.entry.id,
        "weights": model.weights.canonicalize().map_err(|e| e.to_string())?,
        "tokenizer": model.tokenizer.canonicalize().map_err(|e| e.to_string())?,
        "device": setup.device.clone()?.to_string(),
        "context_length": setup.context_length,
        "kv_dtype": format!("{:?}", opts.kv_dtype),
        "prefill_chunk": opts.prefill_chunk,
        "prepack_weights": opts.prepack_weights,
        "threads": std::env::var("CANDLE_NUM_THREADS").map_err(|e| e.to_string())?,
        "rayon_threads": std::env::var("RAYON_NUM_THREADS").map_err(|e| e.to_string())?,
        "cuda_visible_devices": std::env::var_os("CUDA_VISIBLE_DEVICES"),
        "cuda_device_order": std::env::var_os("CUDA_DEVICE_ORDER"),
    }))
}

type Connected = (Box<dyn ChatEngine>, String, Value);

pub fn connect(path: &Path, setup: &crate::engine::EngineSetup) -> Result<Connected, String> {
    if std::env::var_os("NOSH_EVAL_TRACE").is_none() {
        return Err("evaluation worker requires NOSH_EVAL_TRACE".into());
    }
    let mut proxy =
        Proxy::new(UnixStream::connect(path).map_err(|e| format!("evaluation worker: {e}"))?)
            .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let reply = proxy
        .exchange(
            &Request::Hello {
                version: VERSION,
                config: configuration(setup)?,
            },
            &mut |_| {},
            false,
        )
        .map_err(|e| e.to_string())?;
    match reply {
        Reply::Hello {
            version: VERSION,
            mut info,
            description,
        } => {
            info["connect_s"] = json!(started.elapsed().as_secs_f64());
            Ok((Box::new(proxy), description, info))
        }
        _ => Err("invalid evaluation worker handshake".into()),
    }
}

struct Proxy {
    stream: UnixStream,
    pending: Vec<u8>,
    sessions: HashMap<SessionId, State>,
    cancel: CancelHandle,
    failed: Option<String>,
}

impl Proxy {
    fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_read_timeout(Some(Duration::from_millis(20)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self {
            stream,
            pending: Vec::new(),
            sessions: HashMap::new(),
            cancel: CancelHandle::default(),
            failed: None,
        })
    }

    fn exchange(
        &mut self,
        request: &Request,
        sink: &mut dyn FnMut(Event),
        step: bool,
    ) -> Result<Reply, LlmError> {
        if let Some(error) = &self.failed {
            return Err(LlmError::Config(error.clone()));
        }
        let result = (|| -> io::Result<Reply> {
            write_frame(&mut self.stream, request)?;
            let mut cancelled = false;
            loop {
                if step && self.cancel.is_cancelled() && !cancelled {
                    write_frame(&mut self.stream, &Request::Cancel)?;
                    cancelled = true;
                }
                if let Some(end) = self.pending.iter().position(|&byte| byte == b'\n') {
                    if end >= FRAME_LIMIT {
                        return Err(io::Error::other("evaluation reply exceeds limit"));
                    }
                    let reply: Reply = serde_json::from_slice(&self.pending[..end])?;
                    self.pending.drain(..=end);
                    if let Reply::Event(event) = reply {
                        sink(event);
                        continue;
                    }
                    return Ok(reply);
                }
                if self.pending.len() >= FRAME_LIMIT {
                    return Err(io::Error::other("evaluation reply exceeds limit"));
                }
                let mut buffer = [0; 8192];
                match self.stream.read(&mut buffer) {
                    Ok(0) => return Err(io::Error::other("evaluation worker disconnected")),
                    Ok(count) => self.pending.extend_from_slice(&buffer[..count]),
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock
                                | io::ErrorKind::TimedOut
                                | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(e),
                }
            }
        })();
        match result {
            Ok(Reply::Done(Err(failure))) => {
                if let Some((sid, state)) = failure.session {
                    self.sessions.insert(sid, state);
                }
                Err(failure.error.into())
            }
            Ok(reply) => Ok(reply),
            Err(error) => {
                let error = format!("evaluation worker transport: {error}");
                self.failed = Some(error.clone());
                let _ = self.stream.shutdown(Shutdown::Both);
                Err(LlmError::Config(error))
            }
        }
    }

    fn command(
        &mut self,
        request: Request,
        sink: &mut dyn FnMut(Event),
    ) -> Result<Answer, LlmError> {
        let step = matches!(request, Request::Step { .. });
        match self.exchange(&request, sink, step)? {
            Reply::Done(Ok(answer)) => {
                self.sessions.insert(answer.sid, answer.state.clone());
                Ok(answer)
            }
            _ => Err(LlmError::Config(
                "invalid evaluation worker response".into(),
            )),
        }
    }
}

impl ChatEngine for Proxy {
    fn open(&mut self, spec: SessionSpec) -> Result<SessionId, LlmError> {
        Ok(self.command(Request::Open { spec }, &mut |_| {})?.sid)
    }
    fn step(
        &mut self,
        sid: SessionId,
        append: Vec<Message>,
        sink: &mut dyn FnMut(Event),
    ) -> Result<StepOutcome, LlmError> {
        self.command(Request::Step { sid, append }, sink)?
            .outcome
            .ok_or_else(|| LlmError::Config("evaluation worker omitted step outcome".into()))
    }
    fn set_tool_choice(&mut self, sid: SessionId, choice: ToolChoice) -> Result<(), LlmError> {
        self.command(Request::Choice { sid, choice }, &mut |_| {})
            .map(|_| ())
    }
    fn rewind(&mut self, sid: SessionId, keep: usize) -> Result<(), LlmError> {
        self.command(Request::Rewind { sid, keep }, &mut |_| {})
            .map(|_| ())
    }
    fn compact_tool_results(
        &mut self,
        sid: SessionId,
        keep_recent: usize,
    ) -> Result<usize, LlmError> {
        Ok(self
            .command(Request::Compact { sid, keep_recent }, &mut |_| {})?
            .changed)
    }
    fn message_count(&self, sid: SessionId) -> usize {
        self.sessions.get(&sid).map_or(0, |state| state.messages)
    }
    fn context_usage(&self, sid: SessionId) -> (usize, usize) {
        self.sessions
            .get(&sid)
            .map_or((0, 0), |state| state.context)
    }
    fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }
    fn close(&mut self, sid: SessionId) {
        if let Err(error) = self.command(Request::Close { sid }, &mut |_| {}) {
            self.failed = Some(error.to_string());
            eprintln!("nosh: evaluation worker close: {error}");
        }
        self.sessions.remove(&sid);
    }
}

fn answer(engine: &dyn ChatEngine, sid: SessionId, actual: SessionId) -> Answer {
    Answer {
        sid,
        state: State {
            messages: engine.message_count(actual),
            context: engine.context_usage(actual),
        },
        outcome: None,
        changed: 0,
    }
}

#[derive(Default)]
struct Totals {
    connections: usize,
    closed_sessions: usize,
}

fn serve(
    engine: &mut dyn ChatEngine,
    mut stream: UnixStream,
    config: &Value,
    info: &Value,
    description: &str,
    totals: &mut Totals,
) -> io::Result<bool> {
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    match read_request(&mut reader)? {
        Request::Status => {
            write_frame(
                &mut stream,
                &Reply::Status(json!({
                    "pid": std::process::id(), "connections": totals.connections,
                    "closed_sessions": totals.closed_sessions, "active_sessions": 0,
                    "rss_mib": nosh_llm::rss_mb(),
                })),
            )?;
            return Ok(false);
        }
        Request::Hello {
            version: VERSION,
            config: requested,
        } if requested == *config => {}
        _ => {
            write_frame(
                &mut stream,
                &Reply::Done(Err(Failure {
                    error: Fault::Failed("worker protocol/configuration mismatch".into()),
                    session: None,
                })),
            )?;
            return Ok(false);
        }
    }
    totals.connections += 1;
    let mut metadata = info.clone();
    metadata["load_s"] = json!(0);
    metadata["device_init_s"] = json!(0);
    metadata["model_init_s"] = json!(0);
    metadata["execution_mode"] = json!("resident");
    metadata["worker_pid"] = json!(std::process::id());
    metadata["worker_connection"] = json!(totals.connections);
    metadata["worker_config"] = config.clone();
    write_frame(
        &mut stream,
        &Reply::Hello {
            version: VERSION,
            info: metadata,
            description: description.into(),
        },
    )?;
    let cancel = engine.cancel_handle();
    // Only reached after the preceding connection's inference and reader joined.
    cancel.reset();
    let mut sessions = HashMap::new();
    let mut next_id = 1;
    let result = std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader_cancel = cancel.clone();
        let in_step = Arc::new(AtomicBool::new(false));
        let reader_in_step = in_step.clone();
        let reader = scope.spawn(move || {
            loop {
                match read_request(&mut reader) {
                    Ok(Request::Cancel) => reader_cancel.cancel(),
                    Ok(request) => {
                        if matches!(request, Request::Step { .. }) {
                            if reader_in_step.swap(true, Ordering::SeqCst) {
                                eprintln!("nosh: overlapping evaluation steps rejected");
                                reader_cancel.cancel();
                                break;
                            }
                            reader_cancel.reset();
                        }
                        if let Err(error) = sender.try_send(request) {
                            eprintln!("nosh: evaluation client queue: {error}");
                            reader_cancel.cancel();
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("nosh: evaluation client ended: {error}");
                        reader_cancel.cancel();
                        break;
                    }
                }
            }
        });
        let result = (|| -> io::Result<bool> {
            while let Ok(request) = receiver.recv() {
                let is_step = matches!(request, Request::Step { .. });
                let request_sid = match &request {
                    Request::Step { sid, .. }
                    | Request::Choice { sid, .. }
                    | Request::Rewind { sid, .. }
                    | Request::Compact { sid, .. }
                    | Request::Close { sid } => Some(*sid),
                    _ => None,
                };
                let mut output_error = None;
                let mut fatal = false;
                let response = (|| -> Result<Answer, LlmError> {
                    if let Request::Open { spec } = request {
                        let actual = engine.open(spec)?;
                        let sid = next_id;
                        next_id += 1;
                        sessions.insert(sid, actual);
                        return Ok(answer(engine, sid, actual));
                    }
                    let sid = match &request {
                        Request::Step { sid, .. }
                        | Request::Choice { sid, .. }
                        | Request::Rewind { sid, .. }
                        | Request::Compact { sid, .. }
                        | Request::Close { sid } => *sid,
                        _ => return Err(LlmError::Config("unexpected worker operation".into())),
                    };
                    let actual = *sessions.get(&sid).ok_or(LlmError::UnknownSession(sid))?;
                    let mut response = answer(engine, sid, actual);
                    match request {
                        Request::Step { append, .. } => {
                            let out = engine.step(actual, append, &mut |event| {
                                if output_error.is_none()
                                    && let Err(error) =
                                        write_frame(&mut stream, &Reply::Event(event))
                                {
                                    output_error = Some(error);
                                    cancel.cancel();
                                }
                            });
                            // A failed forward can leave partially updated model KV state.
                            fatal = out.as_ref().err().is_some_and(|error| {
                                !matches!(
                                    error,
                                    LlmError::ContextFull { .. } | LlmError::UnknownSession(_)
                                )
                            });
                            response.outcome = Some(out?);
                        }
                        Request::Choice { choice, .. } => engine.set_tool_choice(actual, choice)?,
                        Request::Rewind { keep, .. } => engine.rewind(actual, keep)?,
                        Request::Compact { keep_recent, .. } => {
                            response.changed = engine.compact_tool_results(actual, keep_recent)?
                        }
                        Request::Close { .. } => {
                            engine.close(actual);
                            sessions.remove(&sid);
                            totals.closed_sessions += 1;
                        }
                        _ => unreachable!(),
                    }
                    response.state = answer(engine, sid, actual).state;
                    Ok(response)
                })();
                let response = response.map_err(|error| Failure {
                    error: error.into(),
                    session: request_sid.and_then(|sid| {
                        sessions
                            .get(&sid)
                            .map(|actual| (sid, answer(engine, sid, *actual).state))
                    }),
                });
                if fatal {
                    if let Err(error) = write_frame(&mut stream, &Reply::Done(response)) {
                        eprintln!("nosh: evaluation fatal error delivery: {error}");
                    }
                    return Ok(true);
                }
                if is_step {
                    in_step.store(false, Ordering::SeqCst);
                }
                if let Some(error) = output_error {
                    return Err(error);
                }
                write_frame(&mut stream, &Reply::Done(response))?;
            }
            Ok(false)
        })();
        let _ = stream.shutdown(Shutdown::Both);
        if reader.join().is_err() {
            return Err(io::Error::other("evaluation reader panicked"));
        }
        result
    });
    for (_, sid) in sessions {
        engine.close(sid);
        totals.closed_sessions += 1;
    }
    result
}

pub fn run(path: &Path, setup: &crate::engine::EngineSetup) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("worker socket needs a private directory")?;
    let stat = parent.metadata().map_err(|e| e.to_string())?;
    // SAFETY: geteuid has no preconditions.
    if !stat.is_dir() || stat.mode() & 0o077 != 0 || stat.uid() != unsafe { libc::geteuid() } {
        return Err("worker socket directory must be owned by this user and mode 0700".into());
    }
    let config = configuration(setup)?;
    let (mut engine, description, info) =
        crate::engine::load_local(setup, nosh_core::LoadMode::Background)?;
    let listener = UnixListener::bind(path).map_err(|e| e.to_string())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    write_frame(
        &mut io::stdout(),
        &json!({
            "ev": "ready", "version": VERSION, "pid": std::process::id(),
            "info": info, "config": config, "warmup_generations": 0,
        }),
    )
    .map_err(|e| e.to_string())?;
    let mut totals = Totals::default();
    for connection in listener.incoming() {
        let stream = connection.map_err(|e| e.to_string())?;
        match serve(
            &mut engine,
            stream,
            &config,
            &info,
            &description,
            &mut totals,
        ) {
            Ok(true) => return Err("fatal model error; worker cannot safely reuse KV".into()),
            Ok(false) => {}
            Err(error) => eprintln!("nosh: evaluation connection failed after cleanup: {error}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosh_llm::{MockChatEngine, SamplingParams, StopReason, mock};
    use std::sync::Mutex;

    fn spec(seed: u64) -> SessionSpec {
        SessionSpec {
            label: "test".into(),
            system: "unchanged system".into(),
            tools: vec![],
            thinking: false,
            sampling: SamplingParams {
                seed: Some(seed),
                ..Default::default()
            },
            max_new_tokens: 8,
        }
    }

    fn hello(client: &mut Proxy, config: Value) -> Result<Reply, LlmError> {
        client.exchange(
            &Request::Hello {
                version: VERSION,
                config,
            },
            &mut |_| {},
            false,
        )
    }

    #[test]
    fn isolated_connections_keep_specs_events_ids_and_cleanup() {
        let mut engine = MockChatEngine::with_responder(|history| {
            assert_eq!(history.len(), 2, "no previous case conversation");
            vec![mock::text("answer")]
        });
        let specs = engine.specs();
        let received = engine.received();
        let mut totals = Totals::default();
        for (index, seed) in [0, 1, 0].into_iter().enumerate() {
            let (client, server) = UnixStream::pair().unwrap();
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    serve(
                        &mut engine,
                        server,
                        &json!({}),
                        &json!({"load_s": 9}),
                        "mock",
                        &mut totals,
                    )
                });
                let mut client = Proxy::new(client).unwrap();
                let Reply::Hello { info, .. } = hello(&mut client, json!({})).unwrap() else {
                    panic!()
                };
                assert_eq!(info["load_s"], 0);
                assert_eq!(info["worker_connection"], index + 1);
                assert!(matches!(
                    client.rewind(1, 0),
                    Err(LlmError::UnknownSession(1))
                ));
                let sid = client.open(spec(seed)).unwrap();
                assert_eq!(sid, 1, "connection-local identity");
                assert!(matches!(
                    client.rewind(2, 0),
                    Err(LlmError::UnknownSession(2))
                ));
                let mut events = Vec::new();
                assert_eq!(
                    client
                        .step(
                            sid,
                            vec![Message::User("only this case".into())],
                            &mut |e| events.push(e)
                        )
                        .unwrap()
                        .text,
                    "answer"
                );
                assert!(!events.is_empty());
                assert!(client.context_usage(sid).0 > 0);
                assert_eq!(client.compact_tool_results(sid, 1).unwrap(), 0);
                client.rewind(sid, 1).unwrap();
                assert_eq!(client.message_count(sid), 1);
                if seed == 1 {
                    client.close(sid);
                }
                drop(client);
                assert!(!worker.join().unwrap().unwrap());
            });
        }
        assert_eq!(totals.connections, 3);
        assert_eq!(totals.closed_sessions, 3);
        assert_eq!(
            specs
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.sampling.seed)
                .collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(0)]
        );
        assert_eq!(received.lock().unwrap().len(), 3);
        for sid in 1..=3 {
            assert!(matches!(
                engine.step(sid, vec![], &mut |_| {}),
                Err(LlmError::UnknownSession(_))
            ));
        }
    }

    #[test]
    fn configuration_mismatch_and_worker_loss_are_errors() {
        let (client, server) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                serve(
                    &mut MockChatEngine::new(vec![]),
                    server,
                    &json!({"threads": "4"}),
                    &json!({}),
                    "mock",
                    &mut Totals::default(),
                )
                .unwrap()
            });
            let mut client = Proxy::new(client).unwrap();
            assert!(
                hello(&mut client, json!({"threads": "8"}))
                    .unwrap_err()
                    .to_string()
                    .contains("mismatch")
            );
            worker.join().unwrap();
            assert!(client.open(spec(0)).is_err());
            assert!(client.failed.is_some());
        });
    }

    #[test]
    fn disconnect_cancels_and_joins_before_next_case_can_reset() {
        let slot = Arc::new(Mutex::new(None::<CancelHandle>));
        let handle = slot.clone();
        let finished = Arc::new(Mutex::new(false));
        let done = finished.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let mut first = true;
        let mut engine = MockChatEngine::with_responder(move |_| {
            if !first {
                return vec![mock::text("recovered")];
            }
            first = false;
            started_tx.send(()).unwrap();
            let cancel = handle.lock().unwrap().clone().unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !cancel.is_cancelled() {
                assert!(Instant::now() < deadline, "disconnect did not cancel");
                std::thread::sleep(Duration::from_millis(1));
            }
            *done.lock().unwrap() = true;
            vec![mock::text("cancelled")]
        });
        *slot.lock().unwrap() = Some(engine.cancel_handle());
        let (client, server) = UnixStream::pair().unwrap();
        let mut totals = Totals::default();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                serve(
                    &mut engine,
                    server,
                    &json!({}),
                    &json!({}),
                    "mock",
                    &mut totals,
                )
            });
            let mut client = Proxy::new(client).unwrap();
            hello(&mut client, json!({})).unwrap();
            let sid = client.open(spec(0)).unwrap();
            write_frame(
                &mut client.stream,
                &Request::Step {
                    sid,
                    append: vec![],
                },
            )
            .unwrap();
            started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            drop(client);
            // A disconnected reader cancels independently of streaming output.
            let _ = worker.join().unwrap();
            assert!(*finished.lock().unwrap());
        });
        assert_eq!(totals.closed_sessions, 1);
        assert!(engine.cancel_handle().is_cancelled());
        let (client, server) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                serve(
                    &mut engine,
                    server,
                    &json!({}),
                    &json!({}),
                    "mock",
                    &mut totals,
                )
            });
            let mut client = Proxy::new(client).unwrap();
            hello(&mut client, json!({})).unwrap();
            let sid = client.open(spec(1)).unwrap();
            assert_eq!(sid, 1);
            assert_eq!(
                client.step(sid, vec![], &mut |_| {}).unwrap().text,
                "recovered"
            );
            drop(client);
            worker.join().unwrap().unwrap();
        });
        assert_eq!(totals.closed_sessions, 2);
    }

    #[test]
    fn failed_append_updates_state_and_preserves_typed_context_error() {
        let (client, server) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let mut reader = BufReader::new(server.try_clone().unwrap());
                assert!(matches!(
                    read_request(&mut reader).unwrap(),
                    Request::Step { sid: 1, .. }
                ));
                write_frame(
                    &mut &server,
                    &Reply::Done(Err(Failure {
                        error: Fault::ContextFull { used: 99, max: 80 },
                        session: Some((
                            1,
                            State {
                                messages: 4,
                                context: (99, 80),
                            },
                        )),
                    })),
                )
                .unwrap();
            });
            let mut client = Proxy::new(client).unwrap();
            assert!(matches!(
                client.step(1, vec![Message::User("append".into())], &mut |_| {}),
                Err(LlmError::ContextFull { used: 99, max: 80 })
            ));
            assert_eq!(client.message_count(1), 4);
            assert_eq!(client.context_usage(1), (99, 80));
            worker.join().unwrap();
        });
    }

    #[test]
    fn explicit_cancel_is_delivered_without_a_streaming_event() {
        let slot = Arc::new(Mutex::new(None::<CancelHandle>));
        let handle = slot.clone();
        let mut first = true;
        let mut mock = MockChatEngine::with_responder(move |_| {
            if first {
                first = false;
                let cancel = handle.lock().unwrap().clone().unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                while !cancel.is_cancelled() {
                    assert!(Instant::now() < deadline, "cancel was not forwarded");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            vec![mock::text("done")]
        });
        *slot.lock().unwrap() = Some(mock.cancel_handle());
        let (client, server) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                serve(
                    &mut mock,
                    server,
                    &json!({}),
                    &json!({}),
                    "mock",
                    &mut Totals::default(),
                )
            });
            let mut client = Proxy::new(client).unwrap();
            hello(&mut client, json!({})).unwrap();
            let sid = client.open(spec(0)).unwrap();
            client.cancel.cancel();
            let out = client.step(sid, vec![], &mut |_| {}).unwrap();
            assert_eq!(out.stop, StopReason::Cancelled);
            client.cancel.reset();
            assert_eq!(
                client.step(sid, vec![], &mut |_| {}).unwrap().stop,
                StopReason::EndOfTurn
            );
            drop(client);
            worker.join().unwrap().unwrap();
        });
    }
}
