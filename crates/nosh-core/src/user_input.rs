//! User clarification is a separate capability, never an execution approval.

use std::io::Write;

use nosh_engine::{CancelHandle, ToolCall, ToolSpec};
use nosh_platform::tr;
use nosh_shell::{style, term};
use serde_json::json;

const TEXT_BYTES: usize = 4096;
const CHOICE_BYTES: usize = 512;
const MAX_CHOICES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserQuestion {
    pub question: String,
    pub choices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserAnswer {
    pub question: UserQuestion,
    pub answer: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InputError {
    #[error("user input cancelled")]
    Cancelled,
    #[error("user input unavailable: {0}")]
    Unavailable(String),
    #[error("invalid user question or answer: {0}")]
    Invalid(String),
}

pub trait UserInput: Send {
    fn available(&self) -> bool;
    fn ask(&mut self, question: &UserQuestion, cancel: &CancelHandle)
    -> Result<String, InputError>;
}

pub struct NoUserInput;

impl UserInput for NoUserInput {
    fn available(&self) -> bool {
        false
    }

    fn ask(&mut self, _: &UserQuestion, _: &CancelHandle) -> Result<String, InputError> {
        Err(InputError::Unavailable(
            "no interactive input channel".into(),
        ))
    }
}

pub struct TerminalUserInput;

impl UserInput for TerminalUserInput {
    fn available(&self) -> bool {
        term::available()
    }

    fn ask(
        &mut self,
        question: &UserQuestion,
        cancel: &CancelHandle,
    ) -> Result<String, InputError> {
        let unavailable = |error: std::io::Error| InputError::Unavailable(error.to_string());
        if !self.available() {
            return Err(InputError::Unavailable(
                "no visible, usable terminal".into(),
            ));
        }
        if cancel.is_cancelled() {
            return Err(InputError::Cancelled);
        }
        term::flush_input().map_err(unavailable)?;
        {
            let mut err = std::io::stderr().lock();
            writeln!(
                err,
                "{} {}",
                crate::ui::bar(),
                style::visible_text(&question.question)
            )
            .map_err(unavailable)?;
            for choice in &question.choices {
                writeln!(err, "  - {}", style::visible_text(choice)).map_err(unavailable)?;
            }
            if !question.choices.is_empty() {
                writeln!(
                    err,
                    "{}",
                    tr!(
                        "上下方向键选择，或直接输入其他答案。",
                        "Use Up/Down to choose, or type any answer."
                    )
                )
                .map_err(unavailable)?;
            }
            err.flush().map_err(unavailable)?;
        }
        loop {
            let text = term::read_answer(
                tr!("回答> ", "answer> "),
                &question.choices,
                TEXT_BYTES,
                &|| cancel.is_cancelled(),
            )
            .map_err(unavailable)?
            .ok_or(InputError::Cancelled)?;
            if !text.trim().is_empty() {
                return Ok(text);
            }
            eprintln!(
                "{}",
                tr!(
                    "请输入答案，或按 Esc 取消。",
                    "Enter an answer, or press Esc to cancel."
                )
            );
        }
    }
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "ask_user".into(),
        description: "Ask the user for missing information or choices. Their reply can update the request; wait for it before calling other tools.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "Question to ask."},
                "choices": {"type": "array", "items": {"type":"string"}, "description": "Optional choices. The user can also type their own answer."}
            },
            "required": ["question"],
            "additionalProperties": false
        }),
    }
}

fn valid_text(text: &str, limit: usize) -> bool {
    !text.trim().is_empty()
        && text.len() <= limit
        && !text
            .chars()
            .any(|c| style::is_hidden(c) && !matches!(c, '\u{200c}' | '\u{200d}'))
}

impl UserQuestion {
    pub(crate) fn parse(call: &ToolCall) -> Result<Self, InputError> {
        if call
            .args
            .keys()
            .any(|key| key != "question" && key != "choices")
        {
            return Err(InputError::Invalid("unknown ask_user parameter".into()));
        }
        let question = call.str_arg("question").filter(|text| valid_text(text, TEXT_BYTES))
            .ok_or_else(|| InputError::Invalid("question must be nonempty text of at most 4096 bytes without hidden characters".into()))?;
        let choices = match call.args.get("choices") {
            None => vec![],
            Some(serde_json::Value::Array(values)) if values.len() <= MAX_CHOICES => {
                let mut choices = Vec::new();
                for value in values {
                    let text = value
                        .as_str()
                        .filter(|text| {
                            valid_text(text, CHOICE_BYTES) && !text.contains(['\n', '\t'])
                        })
                        .ok_or_else(|| {
                            InputError::Invalid(
                                "choices must be nonempty single-line strings of at most 512 bytes"
                                    .into(),
                            )
                        })?;
                    if choices
                        .iter()
                        .any(|choice: &String| choice.trim() == text.trim())
                    {
                        return Err(InputError::Invalid("choices must be distinct".into()));
                    }
                    choices.push(text.to_owned());
                }
                choices
            }
            _ => {
                return Err(InputError::Invalid(
                    "choices must be an array of at most 20 strings".into(),
                ));
            }
        };
        Ok(Self {
            question: question.into(),
            choices,
        })
    }
}

pub(crate) fn ask(
    input: &mut dyn UserInput,
    call: &ToolCall,
    cancel: &CancelHandle,
) -> Result<UserAnswer, InputError> {
    let question = UserQuestion::parse(call)?;
    if cancel.is_cancelled() {
        return Err(InputError::Cancelled);
    }
    if !input.available() {
        return Err(InputError::Unavailable(
            "no interactive input channel".into(),
        ));
    }
    let answer = input.ask(&question, cancel);
    if cancel.is_cancelled() {
        return Err(InputError::Cancelled);
    }
    let answer = answer?;
    if !valid_text(&answer, TEXT_BYTES) {
        return Err(InputError::Invalid(
            "answer must be nonempty text of at most 4096 bytes without hidden characters".into(),
        ));
    }
    Ok(UserAnswer { question, answer })
}

#[cfg(test)]
pub(crate) mod tests;
