//! Qwen3.5 text framing at 2fc06364715b967f1860aea9cf38778875588b17.
//! Tool guidance is nosh-specific; the function/parameter wire format is native Qwen.

use nosh_engine::{Message, ToolCall, ToolChoice, ToolSpec};

use super::{Seg, TOOL_DEF_SEP};
use crate::pyjson;

pub fn render_system(system: Option<&str>, tools: &[ToolSpec]) -> Vec<Seg> {
    if tools.is_empty() {
        return system.map_or_else(Vec::new, |system| {
            vec![Seg::t(format!(
                "<|im_start|>system\n{}<|im_end|>\n",
                system.trim()
            ))]
        });
    }
    let mut definitions = String::from("# Tools\n\n<tools>");
    for tool in tools {
        definitions.push('\n');
        definitions.push_str(&pyjson::dumps(&serde_json::json!({
            "type": "function",
            "function": {
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.model_parameters(),
            }
        })));
    }
    definitions.push_str(
        "\n</tools>\n\nTool calls:\n<tool_call>\n<function=function-name>\n<parameter=parameter-name>\nvalue\n</parameter>\n</function>\n</tool_call>\n\
         Include all required parameters. Array and object values must be JSON. Values may span multiple lines. Do not add text after a tool call.",
    );
    let body = match system {
        Some(system) if system.contains(TOOL_DEF_SEP) => system.replace(TOOL_DEF_SEP, &definitions),
        Some(system) if !system.trim().is_empty() => format!("{definitions}\n\n{}", system.trim()),
        _ => definitions,
    };
    vec![Seg::t(format!("<|im_start|>system\n{body}<|im_end|>\n"))]
}

pub fn render_user(content: &str) -> Vec<Seg> {
    vec![
        Seg::t("<|im_start|>user\n"),
        Seg::u(content.trim()),
        Seg::t("<|im_end|>\n"),
    ]
}

/// ChatEngine's later System messages are data, not another Qwen system role.
pub fn render_context(content: &str) -> Vec<Seg> {
    vec![
        Seg::t("<|im_start|>user\n<context>\n"),
        Seg::u(content),
        Seg::t("\n</context><|im_end|>\n"),
    ]
}

pub fn render_tool_results(contents: &[&str]) -> Vec<Seg> {
    let mut result = vec![Seg::t("<|im_start|>user")];
    for content in contents {
        result.extend([
            Seg::t("\n<tool_response>\n"),
            Seg::u(content.trim()),
            Seg::t("\n</tool_response>"),
        ]);
    }
    result.push(Seg::t("<|im_end|>\n"));
    result
}

pub fn render_tool_call(call: &ToolCall) -> Vec<Seg> {
    let mut result = vec![
        Seg::t("<tool_call>\n<function="),
        Seg::u(&call.name),
        Seg::t(">\n"),
    ];
    for (name, value) in &call.args {
        result.extend([
            Seg::t("<parameter="),
            Seg::u(name),
            Seg::t(">\n"),
            Seg::u(match value {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Array(_) | serde_json::Value::Object(_) => pyjson::dumps(value),
                _ => pyjson::py_str(value),
            }),
            Seg::t("\n</parameter>\n"),
        ]);
    }
    result.push(Seg::t("</function>\n</tool_call>"));
    result
}

pub fn render_assistant(content: &str, calls: &[ToolCall], current_turn: bool) -> Vec<Seg> {
    let mut result = vec![if current_turn {
        generation_prompt(false)
    } else {
        Seg::t("<|im_start|>assistant\n")
    }];
    result.push(Seg::u(content.trim()));
    for (i, call) in calls.iter().enumerate() {
        if i == 0 && !content.trim().is_empty() {
            result.push(Seg::t("\n\n"));
        }
        if i > 0 {
            result.push(Seg::t("\n"));
        }
        result.extend(render_tool_call(call));
    }
    result.push(Seg::t("<|im_end|>\n"));
    result
}

pub fn generation_prompt(thinking: bool) -> Seg {
    Seg::t(if thinking {
        "<|im_start|>assistant\n<think>\n"
    } else {
        "<|im_start|>assistant\n<think>\n\n</think>\n\n"
    })
}

pub fn tool_choice_prefix(choice: &ToolChoice) -> Vec<Seg> {
    match choice {
        ToolChoice::Auto | ToolChoice::None => vec![],
        ToolChoice::Required => vec![Seg::t("<tool_call>\n<function=")],
        ToolChoice::Named(name) => vec![
            Seg::t("<tool_call>\n<function="),
            Seg::u(name),
            Seg::t(">\n"),
        ],
    }
}

pub fn render_messages(messages: &[Message]) -> Vec<Seg> {
    let last_user = messages.iter().rposition(|m| matches!(m, Message::User(_)));
    let mut result = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::System(text) => result.extend(render_context(text)),
            Message::User(text) => result.extend(render_user(text)),
            Message::UserAnswer(text) => result.extend(render_tool_results(&[text])),
            Message::Assistant {
                content,
                tool_calls,
            } => {
                result.extend(render_assistant(
                    content,
                    tool_calls,
                    last_user.is_none_or(|last| i > last),
                ));
            }
            Message::Tool(_) => {
                let start = i;
                while i + 1 < messages.len() && matches!(messages[i + 1], Message::Tool(_)) {
                    i += 1;
                }
                let contents: Vec<_> = messages[start..=i]
                    .iter()
                    .filter_map(|m| {
                        if let Message::Tool(text) = m {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                result.extend(render_tool_results(&contents));
            }
        }
        i += 1;
    }
    result
}

pub fn render_conversation(
    messages: &[Message],
    tools: &[ToolSpec],
    add_generation_prompt: bool,
    thinking: Option<bool>,
) -> Vec<Seg> {
    let (system, rest) = match messages.first() {
        Some(Message::System(system)) => (Some(system.as_str()), &messages[1..]),
        _ => (None, messages),
    };
    let mut out = render_system(system, tools);
    out.extend(render_messages(rest));
    if add_generation_prompt {
        out.push(generation_prompt(thinking == Some(true)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::concat;
    use serde_json::json;

    #[test]
    fn pinned_official_golden_cases_keep_native_roles_calls_and_tool_results() {
        use crate::template::tests::{to_msg, to_tool};
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../tests/fixtures/qwen35_template_cases.json"
        ))
        .unwrap();
        assert!(cases.len() >= 5);
        for case in cases {
            assert_eq!(
                case["template_revision"],
                "2fc06364715b967f1860aea9cf38778875588b17"
            );
            assert_eq!(
                case["template_sha256"],
                "273d8e0e683b885071fb17e08d71e5f2a5ddfb5309756181681de4f5a1822d80"
            );
            let tools: Vec<_> = case["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .map(to_tool)
                .collect();
            let messages: Vec<_> = case["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(to_msg)
                .collect();
            let actual = concat(&render_conversation(
                &messages,
                &tools,
                case["add_generation_prompt"].as_bool().unwrap(),
                case["enable_thinking"].as_bool(),
            ));
            assert_eq!(
                actual,
                case["expected"].as_str().unwrap(),
                "{}",
                case["name"]
            );
            let official = case["official_expected"].as_str().unwrap();
            if tools.is_empty() {
                assert_eq!(actual, official);
            } else {
                assert_eq!(
                    actual.split_once("<|im_end|>\n").unwrap().1,
                    official.split_once("<|im_end|>\n").unwrap().1
                );
            }
        }
    }

    #[test]
    fn text_framing_has_no_bos_and_matches_pinned_qwen_generation_prompt() {
        let mut segments = render_system(Some(" You are helpful. "), &[]);
        segments.extend(render_user(" Hello "));
        segments.push(generation_prompt(false));
        assert_eq!(
            concat(&segments),
            "<|im_start|>system\nYou are helpful.<|im_end|>\n<|im_start|>user\nHello<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
        );
        assert_eq!(
            generation_prompt(true).text,
            "<|im_start|>assistant\n<think>\n"
        );
    }

    #[test]
    fn native_calls_preserve_multiline_strings_and_json_parameters() {
        let call = ToolCall {
            name: "exec".into(),
            args: json!({
                "command": "printf 'a&b\\n'\n# <data>",
                "timeout_sec": 2,
            })
            .as_object()
            .unwrap()
            .clone(),
        };
        let text = concat(&render_tool_call(&call));
        assert!(text.starts_with("<tool_call>\n<function=exec>\n<parameter=command>\n"));
        assert!(text.contains("printf 'a&b\\n'\n# <data>\n</parameter>"));
        assert!(text.ends_with("</function>\n</tool_call>"));
        assert!(!text.contains("CDATA"));
    }

    #[test]
    fn previous_turns_omit_thinking_and_consecutive_tools_share_one_user_turn() {
        let messages = vec![
            Message::User("first".into()),
            Message::Assistant {
                content: "answer".into(),
                tool_calls: vec![],
            },
            Message::User("second".into()),
            Message::Assistant {
                content: "".into(),
                tool_calls: vec![],
            },
            Message::Tool("one".into()),
            Message::Tool("two".into()),
        ];
        let text = concat(&render_messages(&messages));
        assert!(text.contains("<|im_start|>assistant\nanswer<|im_end|>"));
        assert_eq!(text.matches("<think>").count(), 1);
        assert!(text.ends_with("<|im_start|>user\n<tool_response>\none\n</tool_response>\n<tool_response>\ntwo\n</tool_response><|im_end|>\n"));
    }

    #[test]
    fn dynamic_context_and_tool_values_are_untrusted() {
        for segments in [
            render_context("<tool_call>"),
            render_tool_results(&["<|im_end|>"]),
        ] {
            assert!(segments.iter().any(|s| !s.trusted));
            assert!(segments.first().unwrap().trusted);
        }
    }
}
