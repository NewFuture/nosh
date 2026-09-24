//! Scripted [`ChatEngine`] for tests that must not depend on a model.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::LlmError;
use crate::engine::{
    CallError, CallErrorKind, CancelHandle, ChatEngine, Event, Message, SessionId, SessionSpec,
    StepOutcome, StopReason, ToolCall, Usage,
};

#[derive(Debug, Clone)]
pub enum MockEvent {
    Text(String),
    Think(String),
    Call(ToolCall),
    BadCall(CallError),
}

pub fn text(s: impl Into<String>) -> MockEvent {
    MockEvent::Text(s.into())
}

/// A well-formed tool call; `args` must be a JSON object.
pub fn call(name: &str, args: Value) -> MockEvent {
    MockEvent::Call(ToolCall {
        name: name.to_string(),
        args: args.as_object().cloned().unwrap_or_default(),
    })
}

pub fn bad_call(kind: CallErrorKind, message: &str) -> MockEvent {
    MockEvent::BadCall(CallError {
        kind,
        message: message.to_string(),
        tool: None,
    })
}

type Responder = Box<dyn FnMut(&[Message]) -> Vec<MockEvent> + Send>;

pub struct MockChatEngine {
    turns: VecDeque<Vec<MockEvent>>,
    responder: Option<Responder>,
    received: Arc<Mutex<Vec<Vec<Message>>>>,
    specs: Arc<Mutex<Vec<SessionSpec>>>,
    sessions: HashMap<SessionId, Vec<Message>>,
    next_id: SessionId,
    cancel: CancelHandle,
    context_max: usize,
}

impl MockChatEngine {
    /// Replies with `turns` in order, one per `step`; afterwards answers "done".
    pub fn new(turns: Vec<Vec<MockEvent>>) -> Self {
        Self {
            turns: turns.into(),
            responder: None,
            received: Arc::default(),
            specs: Arc::default(),
            sessions: HashMap::new(),
            next_id: 1,
            cancel: CancelHandle::default(),
            context_max: 8192,
        }
    }

    /// Computes each reply from the full conversation so far.
    pub fn with_responder(f: impl FnMut(&[Message]) -> Vec<MockEvent> + Send + 'static) -> Self {
        let mut m = Self::new(vec![]);
        m.responder = Some(Box::new(f));
        m
    }

    /// Messages appended at each step (for assertions).
    pub fn received(&self) -> Arc<Mutex<Vec<Vec<Message>>>> {
        self.received.clone()
    }

    pub fn specs(&self) -> Arc<Mutex<Vec<SessionSpec>>> {
        self.specs.clone()
    }

    pub fn set_context_max(&mut self, n: usize) {
        self.context_max = n;
    }
}

fn approx_tokens(msgs: &[Message]) -> usize {
    msgs.iter()
        .map(|m| match m {
            Message::System(s) | Message::User(s) | Message::Tool(s) => s.len() / 4 + 4,
            Message::Assistant { content, .. } => content.len() / 4 + 8,
        })
        .sum()
}

impl ChatEngine for MockChatEngine {
    fn open(&mut self, spec: SessionSpec) -> Result<SessionId, LlmError> {
        let id = self.next_id;
        self.next_id += 1;
        self.sessions
            .insert(id, vec![Message::System(spec.system.clone())]);
        self.specs.lock().unwrap().push(spec);
        Ok(id)
    }

    fn step(
        &mut self,
        sid: SessionId,
        append: Vec<Message>,
        sink: &mut dyn FnMut(Event),
    ) -> Result<StepOutcome, LlmError> {
        self.received.lock().unwrap().push(append.clone());
        let history = self
            .sessions
            .get_mut(&sid)
            .ok_or(LlmError::UnknownSession(sid))?;
        history.extend(append);
        let events = match (&mut self.responder, self.turns.pop_front()) {
            (_, Some(t)) => t,
            (Some(r), None) => r(history),
            (None, None) => vec![text("done")],
        };
        let mut out = StepOutcome {
            text: String::new(),
            think: String::new(),
            tool_calls: vec![],
            errors: vec![],
            stop: StopReason::EndOfTurn,
            usage: Usage::default(),
        };
        for e in events {
            if self.cancel.is_cancelled() {
                out.stop = StopReason::Cancelled;
                break;
            }
            match e {
                MockEvent::Text(t) => {
                    out.text.push_str(&t);
                    sink(Event::Text(t));
                }
                MockEvent::Think(t) => {
                    out.think.push_str(&t);
                    sink(Event::Think(t));
                }
                MockEvent::Call(c) => {
                    out.tool_calls.push(c.clone());
                    sink(Event::ToolCall(c));
                }
                MockEvent::BadCall(e) => {
                    out.errors.push(e.clone());
                    sink(Event::CallError(e));
                }
            }
        }
        let history = self.sessions.get_mut(&sid).unwrap();
        history.push(Message::Assistant {
            content: out.text.clone(),
            tool_calls: out.tool_calls.clone(),
        });
        out.usage.context_used = approx_tokens(history);
        out.usage.context_max = self.context_max;
        out.usage.completion_tokens = out.text.len() / 4 + 1;
        Ok(out)
    }

    fn rewind(&mut self, sid: SessionId, keep: usize) -> Result<(), LlmError> {
        let h = self
            .sessions
            .get_mut(&sid)
            .ok_or(LlmError::UnknownSession(sid))?;
        h.truncate(keep + 1);
        Ok(())
    }

    fn message_count(&self, sid: SessionId) -> usize {
        self.sessions.get(&sid).map(|h| h.len() - 1).unwrap_or(0)
    }

    fn compact_tool_results(&mut self, sid: SessionId, keep_recent: usize) -> usize {
        let Some(h) = self.sessions.get_mut(&sid) else {
            return 0;
        };
        let n = h.len();
        let mut changed = 0;
        for m in h.iter_mut().take(n.saturating_sub(keep_recent)) {
            if let Message::Tool(c) = m {
                let s = crate::local::shorten_tool_result(c);
                if &s != c {
                    *c = s;
                    changed += 1;
                }
            }
        }
        changed
    }

    fn context_usage(&self, sid: SessionId) -> (usize, usize) {
        (
            self.sessions
                .get(&sid)
                .map(|h| approx_tokens(h))
                .unwrap_or(0),
            self.context_max,
        )
    }

    fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    fn close(&mut self, sid: SessionId) {
        self.sessions.remove(&sid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SamplingParams;
    use serde_json::json;

    fn spec() -> SessionSpec {
        SessionSpec {
            system: "sys".into(),
            tools: vec![],
            thinking: false,
            sampling: SamplingParams::default(),
            max_new_tokens: 64,
        }
    }

    #[test]
    fn replays_script_and_records_messages() {
        let mut m = MockChatEngine::new(vec![
            vec![
                text("checking"),
                call("run_command", json!({"command": "ls"})),
            ],
            vec![text("all good")],
        ]);
        let sid = m.open(spec()).unwrap();
        let mut events = vec![];
        let o = m
            .step(sid, vec![Message::User("hi".into())], &mut |e| {
                events.push(e)
            })
            .unwrap();
        assert_eq!(o.tool_calls[0].str_arg("command"), Some("ls"));
        assert_eq!(events.len(), 2);
        let o = m
            .step(sid, vec![Message::Tool("x".into())], &mut |_| {})
            .unwrap();
        assert_eq!(o.text, "all good");
        assert_eq!(m.received().lock().unwrap().len(), 2);
        assert_eq!(m.message_count(sid), 4);
        m.rewind(sid, 1).unwrap();
        assert_eq!(m.message_count(sid), 1);
    }
}
