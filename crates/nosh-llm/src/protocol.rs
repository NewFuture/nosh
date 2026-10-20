//! Model-local framing and control IDs. None of these types cross ChatEngine.

use nosh_engine::{ToolCall, ToolChoice, ToolSpec};

use crate::LlmError;
use crate::template::{self, Seg};
use crate::tokenizer::Tok;
use crate::toolcall::{self, Parsed, StreamParser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatFormat {
    MiniCpm5,
    Qwen35,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ControlTokens {
    pub(crate) eos: u32,
    pub(crate) im_end: u32,
    pub(crate) think_open: u32,
    pub(crate) think_close: u32,
    pub(crate) call_open: u32,
    pub(crate) call_close: u32,
}

pub(crate) enum Parser {
    MiniCpm5(StreamParser),
    Qwen35(toolcall::qwen35::StreamParser),
}

impl Parser {
    pub(crate) fn in_call(&self) -> bool {
        match self {
            Self::MiniCpm5(parser) => parser.in_call(),
            Self::Qwen35(parser) => parser.in_call(),
        }
    }

    pub(crate) fn push(&mut self, id: u32, tok: &Tok) -> Vec<Parsed> {
        match self {
            Self::MiniCpm5(parser) => parser.push(id, tok),
            Self::Qwen35(parser) => parser.push_bytes(id, tok.token_bytes(id)),
        }
    }

    pub(crate) fn finish(&mut self) -> Vec<Parsed> {
        match self {
            Self::MiniCpm5(parser) => parser.finish(),
            Self::Qwen35(parser) => parser.finish(),
        }
    }
}

impl ChatFormat {
    pub(crate) fn parse(name: &str, arch: &str) -> Result<Self, LlmError> {
        match (name, arch) {
            ("minicpm5", "llama") => Ok(Self::MiniCpm5),
            ("qwen35", "qwen35") => Ok(Self::Qwen35),
            _ => Err(LlmError::Config(format!(
                "unsupported chat format {name:?} for GGUF architecture {arch:?}"
            ))),
        }
    }

    pub(crate) fn control_tokens(self, tok: &mut Tok) -> Result<ControlTokens, LlmError> {
        let names = match self {
            Self::MiniCpm5 => [
                "</s>",
                "<|im_end|>",
                "<think>",
                "</think>",
                "<function",
                "</function>",
            ],
            Self::Qwen35 => [
                "<|endoftext|>",
                "<|im_end|>",
                "<think>",
                "</think>",
                "<tool_call>",
                "</tool_call>",
            ],
        };
        let ids: Vec<_> = names
            .iter()
            .map(|name| {
                tok.token_id(name).ok_or_else(|| {
                    LlmError::Config(format!("tokenizer control token {name:?} missing"))
                })
            })
            .collect::<Result<_, _>>()?;
        if self == Self::MiniCpm5
            && ids
                != [
                    1,
                    130073,
                    toolcall::THINK_OPEN,
                    toolcall::THINK_CLOSE,
                    toolcall::FUNCTION_OPEN,
                    toolcall::FUNCTION_CLOSE,
                ]
        {
            return Err(LlmError::Config(
                "tokenizer does not implement the MiniCPM5 control-token mapping".into(),
            ));
        }
        if self == Self::Qwen35 {
            tok.protect_tokens(&[
                "<|endoftext|>",
                "<|im_start|>",
                "<|im_end|>",
                "<think>",
                "</think>",
                "<tool_call>",
                "</tool_call>",
                "<tool_response>",
                "</tool_response>",
            ])?;
        }
        Ok(ControlTokens {
            eos: ids[0],
            im_end: ids[1],
            think_open: ids[2],
            think_close: ids[3],
            call_open: ids[4],
            call_close: ids[5],
        })
    }

    pub(crate) fn parser(self, tools: Vec<ToolSpec>, thinking: bool, ids: ControlTokens) -> Parser {
        match self {
            Self::MiniCpm5 => Parser::MiniCpm5(StreamParser::new(tools, thinking)),
            Self::Qwen35 => {
                Parser::Qwen35(toolcall::qwen35::StreamParser::new(tools, thinking, ids))
            }
        }
    }

    pub(crate) fn system(self, system: &str, tools: &[ToolSpec]) -> Vec<Seg> {
        match self {
            Self::MiniCpm5 => template::render_system(Some(system), tools),
            Self::Qwen35 => template::qwen35::render_system(Some(system), tools),
        }
    }

    pub(crate) fn user(self, content: &str) -> Vec<Seg> {
        match self {
            Self::MiniCpm5 => template::render_user(content),
            Self::Qwen35 => template::qwen35::render_user(content),
        }
    }

    pub(crate) fn context(self, content: &str) -> Vec<Seg> {
        match self {
            Self::MiniCpm5 => template::render_context(content),
            Self::Qwen35 => template::qwen35::render_context(content),
        }
    }

    pub(crate) fn tools(self, contents: &[&str]) -> Vec<Seg> {
        match self {
            Self::MiniCpm5 => template::render_tool_results(contents),
            Self::Qwen35 => template::qwen35::render_tool_results(contents),
        }
    }

    pub(crate) fn assistant(self, content: &str, calls: &[ToolCall]) -> Vec<Seg> {
        match self {
            Self::MiniCpm5 => template::render_assistant(content, calls),
            Self::Qwen35 => template::qwen35::render_assistant(content, calls, true),
        }
    }

    pub(crate) fn generation(self, thinking: bool) -> Seg {
        match self {
            Self::MiniCpm5 => template::generation_prompt(Some(thinking)),
            Self::Qwen35 => template::qwen35::generation_prompt(thinking),
        }
    }

    pub(crate) fn tool_choice(self, choice: &ToolChoice) -> Vec<Seg> {
        match self {
            Self::MiniCpm5 => template::tool_choice_prefix(choice),
            Self::Qwen35 => template::qwen35::tool_choice_prefix(choice),
        }
    }
}
