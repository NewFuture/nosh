use std::collections::VecDeque;
use std::time::Duration;

use nosh_llm::{
    ChatEngine, Event, LlmError, Message, MockChatEngine, SessionId, SessionSpec, StepOutcome,
    mock::{MockEvent, call, text},
};
use nosh_shell::{EmbeddedShell, ShellOptions, Trigger};

use super::*;
use crate::{Agent, AgentConfig, Environment, RecordUi, Scripted, TaskInput, TaskStatus, ToolSet};

pub(crate) struct ScriptedInput {
    pub answers: VecDeque<Result<String, InputError>>,
    pub seen: Vec<UserQuestion>,
    pub delay: Duration,
}

impl ScriptedInput {
    pub fn new(answers: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            answers: answers.into_iter().map(|text| Ok(text.into())).collect(),
            seen: vec![],
            delay: Duration::ZERO,
        }
    }
}

impl UserInput for ScriptedInput {
    fn available(&self) -> bool {
        true
    }
    fn ask(&mut self, question: &UserQuestion, _: &CancelHandle) -> Result<String, InputError> {
        self.seen.push(question.clone());
        std::thread::sleep(self.delay);
        self.answers.pop_front().expect("unexpected user question")
    }
}

fn question(args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: "ask_user".into(),
        args: args.as_object().unwrap().clone(),
    }
}

fn shell(path: &std::path::Path) -> EmbeddedShell {
    EmbeddedShell::new(ShellOptions {
        working_dir: Some(path.into()),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn question_schema_parses_xml_choices_and_enforces_host_boundaries() {
    let raw = "<function name=\"ask_user\"><param name=\"question\">Format?</param><param name=\"choices\">[\"tar.gz\",\"zip\"]</param></function>";
    let call = nosh_llm::toolcall::parse_call(raw, &[spec()]).unwrap();
    assert_eq!(
        UserQuestion::parse(&call).unwrap().choices,
        ["tar.gz", "zip"]
    );
    for args in [
        json!({"question":""}),
        json!({"question":13}),
        json!({"question":"x".repeat(4097)}),
        json!({"question":"x","choices":null}),
        json!({"question":"x","choices":"yes"}),
        json!({"question":"x","choices":[1]}),
        json!({"question":"x","choices":[""]}),
        json!({"question":"x","choices":["one"," one "]}),
        json!({"question":"x","choices":["a\nb"]}),
        json!({"question":"x","choices":["\u{202e}"]}),
        json!({"question":"x","choices":["x".repeat(513)]}),
        json!({"question":"x","choices":(0..21).map(|i| i.to_string()).collect::<Vec<_>>()}),
        json!({"question":"x","allow_freeform":false}),
    ] {
        assert!(
            UserQuestion::parse(&question(args.clone())).is_err(),
            "{args}"
        );
    }
    for args in [
        json!({"question":"x"}),
        json!({"question":"x","choices":[]}),
    ] {
        assert!(
            UserQuestion::parse(&question(args))
                .unwrap()
                .choices
                .is_empty()
        );
    }
}

#[test]
fn choice_text_and_custom_answers_are_returned_verbatim() {
    let call = question(json!({"question":"Format?","choices":["tar.gz","zip"]}));
    for answer in [
        "zip",
        "7z",
        "1",
        "  custom  ",
        "\u{4e2d}\u{6587} \u{1f469}\u{200d}\u{1f4bb}",
        "line one\nline two",
    ] {
        let mut input = ScriptedInput::new([answer]);
        assert_eq!(
            ask(&mut input, &call, &CancelHandle::default())
                .unwrap()
                .answer,
            answer
        );
        assert_eq!(input.seen[0].choices, ["tar.gz", "zip"]);
    }
}

#[test]
fn agent_asks_in_full_and_read_only_sessions_without_conferring_approval() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    for tools in [ToolSet::Full, ToolSet::ReadOnly] {
        let engine = MockChatEngine::new(vec![
            vec![call(
                "ask_user",
                json!({"question":"Name?","choices":["one","two"]}),
            )],
            vec![call("exec", json!({"command":"touch not-approved"}))],
            vec![text("Not executed.")],
        ]);
        let received = engine.received();
        let specs = engine.specs();
        let mut agent = Agent::new(
            Box::new(engine),
            AgentConfig {
                mode: nosh_permissions::ApprovalMode::Confirm,
                ..Default::default()
            },
            Environment::default(),
            tools,
        )
        .with_user_input(Box::new(ScriptedInput::new(["custom"])));
        let mut approval = Scripted::new([]);
        agent.run_task(
            &mut shell,
            TaskInput::new(Trigger::Hash, "choose a name"),
            &mut approval,
            &mut RecordUi::default(),
        );
        assert!(
            matches!(&received.lock().unwrap()[1][0], Message::UserAnswer(answer)
                if serde_json::from_str::<serde_json::Value>(answer).unwrap()
                    == json!({"question":"Name?","choices":["one","two"],"answer":"custom"}))
        );
        assert_eq!(specs.lock().unwrap().len(), 1);
        assert!(
            specs.lock().unwrap()[0]
                .tools
                .iter()
                .any(|tool| tool.name == "ask_user")
        );
        assert_eq!(approval.seen.len(), usize::from(tools == ToolSet::Full));
        assert!(!dir.path().join("not-approved").exists());
    }
}

#[test]
fn agent_question_cancellation_and_unavailable_input_have_distinct_statuses() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    for (error, status) in [
        (InputError::Cancelled, TaskStatus::Cancelled),
        (InputError::Unavailable("closed".into()), TaskStatus::Failed),
    ] {
        let mut input = ScriptedInput::new([]);
        input.answers.push_back(Err(error));
        let engine = MockChatEngine::new(vec![
            vec![call("ask_user", json!({"question":"Which?"}))],
            vec![text("done")],
        ]);
        let received = engine.received();
        let mut agent = Agent::new(
            Box::new(engine),
            AgentConfig::default(),
            Environment::default(),
            ToolSet::Full,
        )
        .with_user_input(Box::new(input));
        let outcome = agent.run_task(
            &mut shell,
            TaskInput::new(Trigger::Hash, "choose"),
            &mut Scripted::new([]),
            &mut RecordUi::default(),
        );
        assert_eq!(outcome.status, status);
        assert_eq!(received.lock().unwrap().len(), 1);
    }
}

#[test]
fn malformed_questions_do_not_prompt_and_can_be_corrected_within_the_step_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    let engine = MockChatEngine::new(vec![
        vec![call(
            "ask_user",
            json!({"question":"Format?","choices":[1]}),
        )],
        vec![call(
            "ask_user",
            json!({"question":"Format?","choices":["zip"]}),
        )],
        vec![text("echo ready")],
    ]);
    let received = engine.received();
    let mut agent = Agent::new(
        Box::new(engine),
        AgentConfig::default(),
        Environment::default(),
        ToolSet::Full,
    )
    .with_user_input(Box::new(ScriptedInput::new(["custom"])));
    let result = agent.run_task(
        &mut shell,
        TaskInput::new(Trigger::Hash, "choose"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(result.status, TaskStatus::Completed);
    assert_eq!(result.steps, 3);
    assert!(
        matches!(&received.lock().unwrap()[2][0], Message::UserAnswer(answer)
            if serde_json::from_str::<serde_json::Value>(answer).unwrap()
                == json!({"question":"Format?","choices":["zip"],"answer":"custom"}))
    );
    assert!(
        matches!(&received.lock().unwrap()[1][0], Message::Tool(error) if error.starts_with("error:"))
    );
}

#[test]
fn agent_reopens_session_when_user_input_capability_changes() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct Input(Arc<AtomicBool>);
    impl UserInput for Input {
        fn available(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
        fn ask(&mut self, _: &UserQuestion, _: &CancelHandle) -> Result<String, InputError> {
            panic!("no question expected");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    let available = Arc::new(AtomicBool::new(true));
    let engine = MockChatEngine::new(vec![vec![text("first")], vec![text("second")]]);
    let specs = engine.specs();
    let mut agent = Agent::new(
        Box::new(engine),
        AgentConfig::default(),
        Environment::default(),
        ToolSet::Full,
    )
    .with_user_input(Box::new(Input(available.clone())));
    for can_ask in [true, false] {
        available.store(can_ask, Ordering::SeqCst);
        agent.run_task(
            &mut shell,
            TaskInput::new(Trigger::Hash, "hello"),
            &mut Scripted::new([]),
            &mut RecordUi::default(),
        );
    }
    let specs = specs.lock().unwrap();
    assert_eq!(specs.len(), 2);
    assert!(specs[0].tools.iter().any(|tool| tool.name == "ask_user"));
    assert!(specs[1].tools.iter().all(|tool| tool.name != "ask_user"));
}

#[test]
fn mixed_question_turns_execute_neither_the_question_nor_other_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    let turn = vec![
        call("ask_user", json!({"question":"Which?"})),
        call("exec", json!({"command":"touch mixed"})),
    ];
    let engine = MockChatEngine::new(vec![turn, vec![text("not executed")]]);
    let received = engine.received();
    let mut agent = Agent::new(
        Box::new(engine),
        AgentConfig::default(),
        Environment::default(),
        ToolSet::Full,
    )
    .with_user_input(Box::new(ScriptedInput::new([])));
    agent.run_task(
        &mut shell,
        TaskInput::new(Trigger::Hash, "choose"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert!(format!("{:?}", received.lock().unwrap()[1]).contains("no tools were executed"));
    assert!(!dir.path().join("mixed").exists());
}

#[test]
fn malformed_question_turns_skip_other_tools_and_can_retry_the_question() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note"), "must-not-be-read").unwrap();
    let mut shell = shell(dir.path());
    for raw in [
        "<function name=\"ask_user\"></function>",
        "<function name=\"ask_user\"><param name=\"question\">Which?</param><param name=\"choices\">not-an-array</param></function>",
    ] {
        let error = nosh_llm::toolcall::parse_call(raw, &[spec()]).unwrap_err();
        assert_eq!(error.tool.as_deref(), Some("ask_user"));
        for other in [
            call("exec", json!({"command":"printf changed > mixed"})),
            call("read_file", json!({"path":"note"})),
        ] {
            for question_first in [true, false] {
                let mut events = vec![MockEvent::BadCall(error.clone()), other.clone()];
                if !question_first {
                    events.reverse();
                }
                let engine = MockChatEngine::new(vec![
                    events,
                    vec![call("ask_user", json!({"question":"Which?"}))],
                    vec![text("done")],
                ]);
                let received = engine.received();
                let mut agent = Agent::new(
                    Box::new(engine),
                    AgentConfig {
                        mode: nosh_permissions::ApprovalMode::Yolo,
                        ..Default::default()
                    },
                    Environment::default(),
                    ToolSet::Full,
                )
                .with_user_input(Box::new(ScriptedInput::new(["second"])));
                let mut approval = Scripted::new([]);
                let outcome = agent.run_task(
                    &mut shell,
                    TaskInput::new(Trigger::Hash, "Ask which target to use before acting."),
                    &mut approval,
                    &mut RecordUi::default(),
                );
                assert_eq!(outcome.status, TaskStatus::Completed);
                assert_eq!(outcome.steps, 3);
                assert_eq!(outcome.commands_run, 0);
                assert!(approval.seen.is_empty());
                assert!(!dir.path().join("mixed").exists());
                let received = received.lock().unwrap();
                let feedback = format!("{:?}", received[1]);
                assert!(feedback.contains("no tools were executed"));
                assert!(!feedback.contains("must-not-be-read"));
                assert!(matches!(&received[2][0], Message::UserAnswer(_)));
            }
        }
    }
}

struct FailAfterAnswer {
    inner: MockChatEngine,
    failed: bool,
}

impl ChatEngine for FailAfterAnswer {
    fn open(&mut self, spec: SessionSpec) -> Result<SessionId, LlmError> {
        self.inner.open(spec)
    }

    fn step(
        &mut self,
        sid: SessionId,
        append: Vec<Message>,
        sink: &mut dyn FnMut(Event),
    ) -> Result<StepOutcome, LlmError> {
        if !self.failed
            && append
                .iter()
                .any(|message| matches!(message, Message::UserAnswer(_)))
        {
            self.failed = true;
            return Err(LlmError::Config("injected failure after the answer".into()));
        }
        self.inner.step(sid, append, sink)
    }

    fn rewind(&mut self, sid: SessionId, keep: usize) -> Result<(), LlmError> {
        self.inner.rewind(sid, keep)
    }

    fn message_count(&self, sid: SessionId) -> usize {
        self.inner.message_count(sid)
    }

    fn compact_tool_results(&mut self, sid: SessionId, keep: usize) -> Result<usize, LlmError> {
        self.inner.compact_tool_results(sid, keep)
    }

    fn context_usage(&self, sid: SessionId) -> (usize, usize) {
        self.inner.context_usage(sid)
    }

    fn cancel_handle(&self) -> CancelHandle {
        self.inner.cancel_handle()
    }

    fn close(&mut self, sid: SessionId) {
        self.inner.close(sid);
    }
}

#[test]
fn recovered_user_answer_keeps_question_and_choices_after_automatic_reset() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    let question = "Which copy should remain?";
    let answer = "  second  ";
    let engine = MockChatEngine::new(vec![
        vec![call(
            "ask_user",
            json!({"question":question,"choices":["alpha","beta"]}),
        )],
        vec![text("done")],
    ]);
    let received = engine.received();
    let specs = engine.specs();
    let mut agent = Agent::new(
        Box::new(FailAfterAnswer {
            inner: engine,
            failed: false,
        }),
        AgentConfig {
            idle_reset: Duration::ZERO,
            ..Default::default()
        },
        Environment::default(),
        ToolSet::Full,
    )
    .with_user_input(Box::new(ScriptedInput::new([answer])));
    let first = agent.run_task(
        &mut shell,
        TaskInput::new(Trigger::Hash, "Ask before proceeding."),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(first.status, TaskStatus::Failed);
    let second = agent.run_task(
        &mut shell,
        TaskInput::new(Trigger::Hash, "Continue according to my answer."),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(second.status, TaskStatus::Completed);
    assert_eq!(specs.lock().unwrap().len(), 2);
    let received = received.lock().unwrap();
    let [Message::UserAnswer(reply), ..] = received.last().unwrap().as_slice() else {
        panic!("missing recovered answer");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(reply).unwrap(),
        json!({"question":question,"choices":["alpha","beta"],"answer":answer})
    );
}
