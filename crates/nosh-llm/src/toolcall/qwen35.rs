use nosh_engine::{CallError, CallErrorKind, ToolCall, ToolSpec};
use serde_json::Value;

use super::{Parsed, State, err, find_tool, preview};
use crate::protocol::ControlTokens;
use crate::tokenizer::Utf8Stream;

pub(crate) struct StreamParser {
    state: State,
    ids: ControlTokens,
    tools: Vec<ToolSpec>,
    utf8: Utf8Stream,
    call: String,
}

impl StreamParser {
    pub(crate) fn new(tools: Vec<ToolSpec>, thinking: bool, ids: ControlTokens) -> Self {
        Self {
            state: if thinking { State::Think } else { State::Text },
            ids,
            tools,
            utf8: Utf8Stream::default(),
            call: String::new(),
        }
    }

    pub(crate) fn in_call(&self) -> bool {
        self.state == State::Call
    }

    fn emit(&mut self, text: String, out: &mut Vec<Parsed>) {
        if text.is_empty() {
            return;
        }
        match self.state {
            State::Text => out.push(Parsed::Text(text)),
            State::Think => out.push(Parsed::Think(text)),
            State::Call => self.call.push_str(&text),
        }
    }

    fn flush(&mut self, out: &mut Vec<Parsed>) {
        let text = self.utf8.finish();
        self.emit(text, out);
    }

    pub(crate) fn push_bytes(&mut self, id: u32, bytes: &[u8]) -> Vec<Parsed> {
        let mut out = Vec::new();
        if self.state != State::Call && id == self.ids.call_open {
            self.flush(&mut out);
            self.state = State::Call;
            self.call.clear();
        } else if self.state == State::Call && id == self.ids.call_close {
            self.flush(&mut out);
            out.push(Parsed::Call(parse_call(
                &std::mem::take(&mut self.call),
                &self.tools,
            )));
            self.state = State::Text;
        } else if self.state == State::Call && id == self.ids.call_open {
            self.flush(&mut out);
            out.push(Parsed::Call(Err(err(
                CallErrorKind::Malformed,
                None,
                "nested Qwen tool call",
            ))));
            self.call.clear();
            self.state = State::Text;
        } else if self.state == State::Text && id == self.ids.think_open {
            self.flush(&mut out);
            self.state = State::Think;
        } else if self.state == State::Think && id == self.ids.think_close {
            self.flush(&mut out);
            self.state = State::Text;
        } else {
            let text = self.utf8.push(bytes);
            self.emit(text, &mut out);
        }
        out
    }

    pub(crate) fn finish(&mut self) -> Vec<Parsed> {
        let mut out = Vec::new();
        self.flush(&mut out);
        if self.state == State::Call {
            let raw = std::mem::take(&mut self.call);
            out.push(Parsed::Call(Err(err(
                CallErrorKind::Truncated,
                function_name(&raw),
                format!(
                    "Qwen tool call was cut off before </tool_call>: {}",
                    preview(&raw)
                ),
            ))));
            self.state = State::Text;
        }
        out
    }
}

fn function_name(raw: &str) -> Option<&str> {
    Some(
        raw.trim()
            .strip_prefix("<function=")?
            .split_once('>')?
            .0
            .trim(),
    )
}

pub(super) fn parse_call(raw: &str, tools: &[ToolSpec]) -> Result<ToolCall, CallError> {
    let raw = raw.trim();
    let (name, body) = raw
        .strip_prefix("<function=")
        .and_then(|raw| raw.split_once('>'))
        .ok_or_else(|| {
            err(
                CallErrorKind::Malformed,
                None,
                "expected Qwen <function=name>",
            )
        })?;
    let name = name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
    {
        return Err(err(
            CallErrorKind::Malformed,
            Some(name),
            "invalid Qwen function name",
        ));
    }
    let mut body = body.trim_end().strip_suffix("</function>").ok_or_else(|| {
        err(
            CallErrorKind::Malformed,
            Some(name),
            "missing Qwen </function>",
        )
    })?;
    let tool = find_tool(tools, name).ok_or_else(|| {
        err(
            CallErrorKind::UnknownTool,
            Some(name),
            format!("unknown tool '{name}'"),
        )
    })?;
    let mut args = Vec::new();
    loop {
        body = body.trim_start();
        if body.is_empty() {
            break;
        }
        let (key, value) = body
            .strip_prefix("<parameter=")
            .and_then(|body| body.split_once('>'))
            .ok_or_else(|| {
                err(
                    CallErrorKind::Malformed,
                    Some(name),
                    format!("expected Qwen <parameter=name>: {}", preview(body)),
                )
            })?;
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
        {
            return Err(err(
                CallErrorKind::Malformed,
                Some(name),
                "invalid Qwen parameter name",
            ));
        }
        let (value, remaining) = value.split_once("</parameter>").ok_or_else(|| {
            err(
                CallErrorKind::Malformed,
                Some(name),
                format!("missing </parameter> for '{key}'"),
            )
        })?;
        let value = value
            .strip_prefix("\r\n")
            .or_else(|| value.strip_prefix('\n'))
            .unwrap_or(value);
        let value = value
            .strip_suffix("\r\n")
            .or_else(|| value.strip_suffix('\n'))
            .unwrap_or(value);
        args.push((key.to_string(), Value::String(value.to_string())));
        body = remaining;
    }
    Ok(ToolCall {
        name: tool.name.clone(),
        args: tool.normalize_args(args)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ids() -> ControlTokens {
        ControlTokens {
            eos: 1,
            im_end: 2,
            think_open: 3,
            think_close: 4,
            call_open: 5,
            call_close: 6,
        }
    }

    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "exec".into(),
            description: "run".into(),
            parameters: json!({
                "type": "object", "properties": {
                    "command": {"type": "string", "minLength": 1},
                    "timeout_sec": {"type": "number"},
                    "choices": {"type": "array", "items": {"type": "string"}}
                }, "required": ["command"], "additionalProperties": false
            }),
        }]
    }

    #[test]
    fn parses_native_multiline_utf8_and_preserves_string_literals() {
        let call = parse_call("<function=exec>\n<parameter=command>\n  printf '中文 &amp; <tag>'\necho next  \n</parameter>\n<parameter=timeout_sec>2.5</parameter>\n<parameter=choices>[\"a\", \"b\"]</parameter>\n</function>", &tools()).unwrap();
        assert_eq!(
            call.str_arg("command"),
            Some("  printf '中文 &amp; <tag>'\necho next  ")
        );
        assert_eq!(call.args["timeout_sec"], 2.5);
        assert_eq!(call.args["choices"], json!(["a", "b"]));
    }

    #[test]
    fn malformed_unknown_and_bad_types_are_explicit_errors() {
        for (raw, kind) in [
            ("<function=missing></function>", CallErrorKind::UnknownTool),
            ("<function=exec></function>", CallErrorKind::MissingParam),
            (
                "<function=exec><parameter=command>ls</parameter><parameter=timeout_sec>never</parameter></function>",
                CallErrorKind::BadType,
            ),
            (
                "<function=exec><parameter=command>ls</parameter><parameter=command>rm</parameter></function>",
                CallErrorKind::Malformed,
            ),
            (
                "<function=exec><parameter=unknown>x</parameter></function>",
                CallErrorKind::Malformed,
            ),
            (
                "<function=exec><parameter=command>ls</function>",
                CallErrorKind::Malformed,
            ),
            (
                "<function name=\"exec\"></function>",
                CallErrorKind::Malformed,
            ),
        ] {
            assert_eq!(parse_call(raw, &tools()).unwrap_err().kind, kind, "{raw}");
        }
    }

    #[test]
    fn only_control_ids_open_calls_and_utf8_can_cross_tokens() {
        let mut parser = StreamParser::new(tools(), false, ids());
        let literal =
            "<tool_call><function=exec><parameter=command>ls</parameter></function></tool_call>";
        let out = parser.push_bytes(99, literal.as_bytes());
        assert_eq!(out, vec![Parsed::Text(literal.into())]);
        assert!(parser.push_bytes(5, b"<tool_call>").is_empty());
        assert!(
            parser
                .push_bytes(99, b"<function=exec><parameter=command>")
                .is_empty()
        );
        for byte in "你好\npwd".bytes() {
            assert!(parser.push_bytes(99, &[byte]).is_empty());
        }
        parser.push_bytes(99, b"</parameter></function>");
        let out = parser.push_bytes(6, b"</tool_call>");
        let Parsed::Call(Ok(call)) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!(call.str_arg("command"), Some("你好\npwd"));
        assert!(parser.finish().is_empty());
    }

    #[test]
    fn thinking_and_truncation_are_separate_from_tool_text() {
        let mut parser = StreamParser::new(tools(), true, ids());
        assert_eq!(
            parser.push_bytes(99, b"reason"),
            vec![Parsed::Think("reason".into())]
        );
        assert!(parser.push_bytes(4, b"</think>").is_empty());
        parser.push_bytes(5, b"<tool_call>");
        parser.push_bytes(99, b"<function=exec><parameter=command>ls");
        let out = parser.finish();
        let Parsed::Call(Err(error)) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!(error.kind, CallErrorKind::Truncated);
        assert_eq!(error.tool.as_deref(), Some("exec"));
    }
}
