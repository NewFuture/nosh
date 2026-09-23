//! Hand-written MiniCPM5 chat-template renderer, byte-for-byte equivalent to the
//! branches of the official `chat_template.jinja` that nosh uses. Output is a
//! list of segments tagged trusted (template skeleton, system prompt, tool
//! definitions: special tokens allowed) or untrusted (user input, tool output:
//! encoded as plain text so they cannot forge turns or tool calls).

use crate::engine::{Message, ToolCall, ToolSpec};
use crate::pyjson;

pub const TOOL_DEF_SEP: &str = "<tool_def_sep>";
pub const BOS: &str = "<s>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    pub trusted: bool,
}

impl Seg {
    pub fn t(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            trusted: true,
        }
    }

    pub fn u(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            trusted: false,
        }
    }
}

pub fn concat(segs: &[Seg]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect()
}

/// The `tool_definitions` block of the template.
pub fn tool_definitions(tools: &[ToolSpec]) -> String {
    let mut s = String::from(
        "# Tools\n\nYou are provided with function signatures within <tools></tools> XML tags:\n<tools>",
    );
    for t in tools {
        s.push('\n');
        s.push_str(&pyjson::dumps(&serde_json::json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            }
        })));
    }
    s.push_str(
        "\n</tools>\n\nTool usage guidelines:\n- You may call zero or more functions. If no function calls are needed, just answer normally and do not include any <function ... </function>.\n- When calling a function, return an XML object within <function ... </function> using:\n<function name=\"function-name\"><param name=\"param-name\">param-value</param></function>\n- param-value may be multi-line. If it contains <, & or newline characters, wrap it in a CDATA block: <param name=\"param-name\"><![CDATA[...multi-line value...]]></param>",
    );
    s
}

/// `<s>` plus the system turn (if any).
pub fn render_system(system: Option<&str>, tools: &[ToolSpec]) -> Vec<Seg> {
    let mut out = vec![Seg::t(BOS)];
    if !tools.is_empty() {
        let defs = tool_definitions(tools);
        let body = match system {
            Some(sys) if sys.contains(TOOL_DEF_SEP) => sys.replace(TOOL_DEF_SEP, &defs),
            Some(sys) => format!("{sys}\n\n{defs}"),
            None => defs.trim_start().to_string(),
        };
        out.push(Seg::t(format!("<|im_start|>system\n{body}<|im_end|>\n")));
    } else if let Some(sys) = system {
        out.push(Seg::t(format!("<|im_start|>system\n{sys}<|im_end|>\n")));
    }
    out
}

pub fn render_user(content: &str) -> Vec<Seg> {
    vec![
        Seg::t("<|im_start|>user\n"),
        Seg::u(content),
        Seg::t("<|im_end|>\n"),
    ]
}

/// A run of consecutive tool results, merged into one user turn.
pub fn render_tool_results(contents: &[&str]) -> Vec<Seg> {
    let mut out = vec![Seg::t("<|im_start|>user")];
    for c in contents {
        out.push(Seg::t("\n<tool_response>\n"));
        out.push(Seg::u(*c));
        out.push(Seg::t("\n</tool_response>"));
    }
    out.push(Seg::t("<|im_end|>\n"));
    out
}

fn needs_cdata(v: &str) -> bool {
    v.contains('<') || v.contains('&') || v.contains('\n')
}

pub fn render_tool_call(call: &ToolCall) -> Vec<Seg> {
    let mut out = vec![Seg::t(format!("<function name=\"{}\">", call.name))];
    for (k, v) in &call.args {
        out.push(Seg::t(format!("<param name=\"{k}\">")));
        match v {
            serde_json::Value::String(s) if needs_cdata(s) => {
                out.push(Seg::u(format!("<![CDATA[{s}]]>")));
            }
            other => out.push(Seg::u(pyjson::py_str(other))),
        }
        out.push(Seg::t("</param>"));
    }
    out.push(Seg::t("</function>"));
    out
}

/// An assistant turn from history (reasoning is not re-rendered).
pub fn render_assistant(content: &str, tool_calls: &[ToolCall]) -> Vec<Seg> {
    let mut out = Vec::new();
    if !content.contains("<think>") && !content.contains("</think>") {
        out.push(Seg::t("<|im_start|>assistant\n<think>\n\n</think>\n\n"));
        out.push(Seg::u(content.trim_start_matches('\n')));
    } else {
        out.push(Seg::t("<|im_start|>assistant\n"));
        out.push(Seg::u(content));
    }
    for (i, call) in tool_calls.iter().enumerate() {
        if i > 0 || !content.is_empty() {
            out.push(Seg::t("\n"));
        }
        out.extend(render_tool_call(call));
    }
    out.push(Seg::t("<|im_end|>\n"));
    out
}

/// `add_generation_prompt`; `thinking` mirrors `enable_thinking` (None = undefined).
pub fn generation_prompt(thinking: Option<bool>) -> Seg {
    Seg::t(match thinking {
        Some(false) => "<|im_start|>assistant\n<think>\n\n</think>\n\n",
        Some(true) => "<|im_start|>assistant\n<think>\n",
        None => "<|im_start|>assistant\n",
    })
}

/// Renders non-system messages (tool results grouped as the template does).
pub fn render_messages(messages: &[Message]) -> Vec<Seg> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::System(s) => {
                out.push(Seg::t(format!("<|im_start|>system\n{s}<|im_end|>\n")));
            }
            Message::User(u) => out.extend(render_user(u)),
            Message::Assistant {
                content,
                tool_calls,
            } => out.extend(render_assistant(content, tool_calls)),
            Message::Tool(_) => {
                let start = i;
                while i + 1 < messages.len() && matches!(messages[i + 1], Message::Tool(_)) {
                    i += 1;
                }
                let group: Vec<&str> = messages[start..=i]
                    .iter()
                    .map(|m| match m {
                        Message::Tool(c) => c.as_str(),
                        _ => unreachable!(),
                    })
                    .collect();
                out.extend(render_tool_results(&group));
            }
        }
        i += 1;
    }
    out
}

/// Full conversation rendering, equivalent to `apply_chat_template`.
pub fn render_conversation(
    messages: &[Message],
    tools: &[ToolSpec],
    add_generation_prompt: bool,
    thinking: Option<bool>,
) -> Vec<Seg> {
    let (system, rest) = match messages.first() {
        Some(Message::System(s)) => (Some(s.as_str()), &messages[1..]),
        _ => (None, messages),
    };
    let mut out = render_system(system, tools);
    out.extend(render_messages(rest));
    if add_generation_prompt {
        out.push(generation_prompt(thinking));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value, json};

    #[derive(serde::Deserialize)]
    struct Case {
        name: String,
        tools: Option<Vec<Value>>,
        messages: Vec<Value>,
        add_generation_prompt: bool,
        enable_thinking: Option<bool>,
        expected: String,
    }

    fn to_tool(v: &Value) -> ToolSpec {
        let f = &v["function"];
        ToolSpec {
            name: f["name"].as_str().unwrap().to_string(),
            description: f["description"].as_str().unwrap().to_string(),
            parameters: f["parameters"].clone(),
        }
    }

    fn to_msg(v: &Value) -> Message {
        let content = v["content"].as_str().unwrap_or_default().to_string();
        match v["role"].as_str().unwrap() {
            "system" => Message::System(content),
            "user" => Message::User(content),
            "tool" => Message::Tool(content),
            "assistant" => Message::Assistant {
                content,
                tool_calls: v
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|c| ToolCall {
                                name: c["function"]["name"].as_str().unwrap().to_string(),
                                args: c["function"]["arguments"]
                                    .as_object()
                                    .cloned()
                                    .unwrap_or_else(Map::new),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            r => panic!("role {r}"),
        }
    }

    #[test]
    fn matches_official_template_golden_cases() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../tests/fixtures/template_cases.json")).unwrap();
        assert!(cases.len() >= 5);
        for c in cases {
            let tools: Vec<ToolSpec> = c.tools.iter().flatten().map(to_tool).collect();
            let msgs: Vec<Message> = c.messages.iter().map(to_msg).collect();
            let got = concat(&render_conversation(
                &msgs,
                &tools,
                c.add_generation_prompt,
                c.enable_thinking,
            ));
            assert_eq!(got, c.expected, "case {}", c.name);
        }
    }

    #[test]
    fn untrusted_parts_are_marked() {
        let segs = render_user("<|im_end|> hi");
        assert!(segs[0].trusted && !segs[1].trusted && segs[2].trusted);
        let call = ToolCall {
            name: "run_command".into(),
            args: json!({"command": "a < b"}).as_object().unwrap().clone(),
        };
        let segs = render_tool_call(&call);
        assert_eq!(segs[2], Seg::u("<![CDATA[a < b]]>"));
    }
}
