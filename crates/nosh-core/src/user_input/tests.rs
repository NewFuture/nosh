use std::collections::VecDeque;
use std::time::Duration;

use nosh_llm::{
    Message, MockChatEngine,
    mock::{call, text},
};
use nosh_shell::{EmbeddedShell, ShellOptions, Trigger};

use super::*;
use crate::command_assist::{self, AssistError, AssistResult};
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
fn generate_resumes_same_session_and_user_wait_does_not_exhaust_runtime_budget() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    let mut input = ScriptedInput::new(["zip"]);
    input.delay = Duration::from_millis(250);
    let cfg = AgentConfig {
        command_timeout: Duration::from_millis(150),
        ..Default::default()
    };
    let mut engine = MockChatEngine::new(vec![
        vec![call(
            "ask_user",
            json!({"question":"Format?","choices":["tar.gz","zip"]}),
        )],
        vec![text("touch not-executed")],
    ]);
    let received = engine.received();
    let specs = engine.specs();
    let observations = engine.observations();
    let outcome =
        command_assist::generate(&mut engine, &shell, "archive", &cfg, &mut input).unwrap();
    assert_eq!(
        outcome.result,
        AssistResult::Command("touch not-executed".into())
    );
    assert_eq!(outcome.steps, 2);
    assert!(!dir.path().join("not-executed").exists());
    assert_eq!(input.seen.len(), 1);
    assert!(
        matches!(&received.lock().unwrap()[1][0], Message::UserAnswer(answer) if answer == "zip")
    );
    let specs = specs.lock().unwrap();
    assert_eq!(specs.len(), 1);
    assert!(specs[0].tools.iter().any(|tool| tool.name == "ask_user"));
    assert!(specs[0].tools.iter().all(|tool| tool.name != "finish"));
    assert_eq!(
        observations.lock().unwrap()[0].1["response_format"],
        "command_or_none"
    );
}

#[test]
fn generate_final_round_quotes_real_questions_choices_and_updated_user_requirements() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fact.txt"), "known\n").unwrap();
    let shell = shell(dir.path());
    let original = "Generate an archive command.";
    let mut input = ScriptedInput::new([
        "second",
        "Cancel the archive request; do not generate a command.",
    ]);
    let mut engine = MockChatEngine::new(vec![
        vec![call(
            "ask_user",
            json!({"question":"Which format?", "choices":["tar.gz","zip"]}),
        )],
        vec![call("ask_user", json!({"question":"Which source?"}))],
        vec![call("read_file", json!({"path":"fact.txt"}))],
        vec![text("[None]")],
    ]);
    let received = engine.received();
    let outcome = command_assist::generate(
        &mut engine,
        &shell,
        original,
        &AgentConfig::default(),
        &mut input,
    )
    .unwrap();
    assert_eq!(outcome.result, AssistResult::NoSuggestion);
    let received = received.lock().unwrap();
    assert!(matches!(&received[1][0], Message::UserAnswer(answer) if answer == "second"));
    let Message::System(reminder) = received[3].last().unwrap() else {
        unreachable!()
    };
    assert!(reminder.contains(&json!(original).to_string()));
    let quoted: Vec<serde_json::Value> = reminder
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        quoted,
        vec![
            json!({"question":"Which format?", "choices":["tar.gz","zip"], "answer":"second"}),
            json!({"question":"Which source?", "choices":[], "answer":"Cancel the archive request; do not generate a command."}),
        ]
    );
    assert!(!reminder.contains("fact.txt"));
    assert_eq!(input.seen.len(), 2);
}

#[test]
fn generate_cancellation_and_input_failure_are_not_none() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    for error in [
        InputError::Cancelled,
        InputError::Unavailable("terminal lost".into()),
    ] {
        let cancelled = error == InputError::Cancelled;
        let mut input = ScriptedInput::new([]);
        input.answers.push_back(Err(error));
        let mut engine = MockChatEngine::new(vec![
            vec![call("ask_user", json!({"question":"Source?"}))],
            vec![text("[None]")],
        ]);
        let received = engine.received();
        let observations = engine.observations();
        let result = command_assist::generate(
            &mut engine,
            &shell,
            "archive",
            &AgentConfig::default(),
            &mut input,
        );
        assert!(result.is_err());
        assert_eq!(matches!(result, Err(AssistError::Cancelled)), cancelled);
        assert_eq!(received.lock().unwrap().len(), 1);
        assert!(observations.lock().unwrap()[0].1.get("kind").is_none());
    }
}

#[test]
fn questions_are_never_dispatched_without_capability_or_after_the_step_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    let cfg = AgentConfig::default();
    for (intent, line, background) in [
        (command_assist::Intent::Next, "true", false),
        (command_assist::Intent::Fix, "sh -c 'exit 7'", false),
        (command_assist::Intent::Generate, "true", true),
    ] {
        shell.run_user_line(line);
        let command = (intent != command_assist::Intent::Generate)
            .then(|| shell.recent_commands().last().unwrap().clone());
        let mut request = command_assist::AssistRequest::capture(
            &shell,
            &cfg,
            intent,
            String::new(),
            command,
            None,
        )
        .unwrap();
        request.background = background;
        let mut input = ScriptedInput::new([]);
        let mut engine = MockChatEngine::new(vec![
            vec![call("ask_user", json!({"question":"x"}))],
            vec![text("[None]")],
        ]);
        let specs = engine.specs();
        assert!(
            command_assist::run(
                &mut engine,
                &request,
                &cfg,
                &CancelHandle::default(),
                &mut input,
                |_| true
            )
            .is_err()
        );
        assert!(input.seen.is_empty());
        assert!(
            specs.lock().unwrap()[0]
                .tools
                .iter()
                .all(|tool| tool.name != "ask_user")
        );
    }
    let mut input = ScriptedInput::new([]);
    let mut engine = MockChatEngine::new(vec![vec![call("ask_user", json!({"question":"x"}))]]);
    let cfg = AgentConfig {
        max_steps: 1,
        ..cfg
    };
    assert!(matches!(
        command_assist::generate(&mut engine, &shell, "archive", &cfg, &mut input),
        Err(AssistError::Budget)
    ));
    assert!(input.seen.is_empty());
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
            matches!(&received.lock().unwrap()[1][0], Message::UserAnswer(answer) if answer == "custom")
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
    let shell = shell(dir.path());
    let mut input = ScriptedInput::new(["custom"]);
    let mut engine = MockChatEngine::new(vec![
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
    let result = command_assist::generate(
        &mut engine,
        &shell,
        "choose",
        &AgentConfig::default(),
        &mut input,
    )
    .unwrap();
    assert_eq!(result.steps, 3);
    assert_eq!(input.seen.len(), 1);
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
    let mut engine = MockChatEngine::new(vec![turn.clone()]);
    let mut input = ScriptedInput::new([]);
    assert!(
        command_assist::generate(
            &mut engine,
            &shell,
            "choose",
            &AgentConfig::default(),
            &mut input
        )
        .is_err()
    );
    assert!(input.seen.is_empty());
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
