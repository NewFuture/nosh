//! Streaming output parser driven by special-token ids (design §5.6):
//! TEXT ⇄ THINK via 8/9, TEXT → CALL via 18 `<function`, CALL → TEXT via 19
//! `</function>`. Inside a call, `<param`/`</param>` (20/21) delimit arguments;
//! values may be CDATA or entity-escaped, and are converted per JSON Schema.

use serde_json::Value;

use crate::tokenizer::{Tok, Utf8Stream};
use nosh_engine::{CallError, CallErrorKind, ToolCall, ToolSpec};

pub(crate) mod qwen35;

pub const THINK_OPEN: u32 = 8;
pub const THINK_CLOSE: u32 = 9;
pub const FUNCTION_OPEN: u32 = 18;
pub const FUNCTION_CLOSE: u32 = 19;
pub const PARAM_OPEN: u32 = 20;
pub const PARAM_CLOSE: u32 = 21;

#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Text(String),
    Think(String),
    Call(Result<ToolCall, CallError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Text,
    Think,
    Call,
}

pub struct StreamParser {
    state: State,
    utf8: Utf8Stream,
    call: String,
    tools: Vec<ToolSpec>,
}

impl StreamParser {
    /// `start_in_think` when the generation prompt opened a `<think>` block.
    pub fn new(tools: Vec<ToolSpec>, start_in_think: bool) -> Self {
        Self {
            state: if start_in_think {
                State::Think
            } else {
                State::Text
            },
            utf8: Utf8Stream::default(),
            call: String::new(),
            tools,
        }
    }

    pub fn in_call(&self) -> bool {
        self.state == State::Call
    }

    fn flush(&mut self, out: &mut Vec<Parsed>) {
        let s = self.utf8.finish();
        self.emit(s, out);
    }

    fn emit(&mut self, s: String, out: &mut Vec<Parsed>) {
        if s.is_empty() {
            return;
        }
        match self.state {
            State::Text => out.push(Parsed::Text(s)),
            State::Think => out.push(Parsed::Think(s)),
            State::Call => self.call.push_str(&s),
        }
    }

    pub fn push(&mut self, id: u32, tok: &Tok) -> Vec<Parsed> {
        self.push_bytes(id, tok.token_bytes(id))
    }

    /// Feeds one generated token (`bytes` is its decoded byte string).
    pub fn push_bytes(&mut self, id: u32, bytes: &[u8]) -> Vec<Parsed> {
        let mut out = Vec::new();
        match (self.state, id) {
            (State::Text, THINK_OPEN) => {
                self.flush(&mut out);
                self.state = State::Think;
            }
            (State::Think, THINK_CLOSE) => {
                self.flush(&mut out);
                self.state = State::Text;
            }
            (State::Text | State::Think, FUNCTION_OPEN) => {
                self.flush(&mut out);
                self.state = State::Call;
                self.call = String::from("<function");
            }
            (State::Call, FUNCTION_CLOSE) => {
                self.flush(&mut out);
                self.call.push_str("</function>");
                let raw = std::mem::take(&mut self.call);
                out.push(Parsed::Call(parse_call(&raw, &self.tools)));
                self.state = State::Text;
            }
            (State::Call, PARAM_OPEN) => {
                self.flush(&mut out);
                self.call.push_str("<param");
            }
            (State::Call, PARAM_CLOSE) => {
                self.flush(&mut out);
                self.call.push_str("</param>");
            }
            _ => {
                let s = self.utf8.push(bytes);
                self.emit(s, &mut out);
            }
        }
        out
    }

    /// End of generation: a call still open is reported as truncated.
    pub fn finish(&mut self) -> Vec<Parsed> {
        let mut out = Vec::new();
        self.flush(&mut out);
        if self.state == State::Call {
            let raw = std::mem::take(&mut self.call);
            out.push(Parsed::Call(Err(CallError {
                kind: CallErrorKind::Truncated,
                message: format!(
                    "tool call was cut off before </function>: {}",
                    preview(&raw)
                ),
                tool: parse_name(&raw),
            })));
            self.state = State::Text;
        }
        out
    }
}

fn preview(s: &str) -> String {
    let s: String = s.chars().take(160).collect();
    s.replace('\n', "\\n")
}

fn err(kind: CallErrorKind, tool: Option<&str>, message: impl Into<String>) -> CallError {
    CallError {
        kind,
        message: message.into(),
        tool: tool.map(str::to_string),
    }
}

fn find_tool<'a>(tools: &'a [ToolSpec], name: &str) -> Option<&'a ToolSpec> {
    tools.iter().find(|tool| tool.name == name).or_else(|| {
        let mut matches = tools.iter().filter(|tool| tool.matches_name(name));
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    })
}

/// Parses an attribute value `name="x"` / `name='x'` / `name=x` at the start of `s`.
fn parse_attr<'a>(s: &'a str, attr: &str) -> Option<(String, &'a str)> {
    let s = s.trim_start();
    let s = s
        .strip_prefix(attr)?
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    if let Some(q) = s.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let body = &s[1..];
        let end = body.find(q)?;
        Some((unescape(&body[..end]), &body[end + 1..]))
    } else {
        let end = s.find(|c: char| c == '>' || c.is_whitespace())?;
        Some((s[..end].to_string(), &s[end..]))
    }
}

fn parse_name(raw: &str) -> Option<String> {
    raw.strip_prefix("<function")
        .and_then(|r| parse_attr(r, "name"))
        .map(|(n, _)| n)
}

/// XML entity unescaping (named + numeric).
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let Some(semi) = tail.bytes().take(12).position(|byte| byte == b';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..semi];
        let decoded = match ent {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parses `<function name="…"><param name="…">…</param>…</function>` and converts
/// argument types according to the tool's JSON Schema.
pub fn parse_call(raw: &str, tools: &[ToolSpec]) -> Result<ToolCall, CallError> {
    let body = raw
        .strip_prefix("<function")
        .and_then(|r| r.strip_suffix("</function>"))
        .ok_or_else(|| {
            err(
                CallErrorKind::Malformed,
                None,
                "expected <function …</function>",
            )
        })?;
    let (name, rest) = parse_attr(body, "name").ok_or_else(|| {
        err(
            CallErrorKind::Malformed,
            None,
            format!("missing name=\"…\" in {}", preview(raw)),
        )
    })?;
    let name = name.trim().to_string();
    let mut rest = rest.trim_start().strip_prefix('>').ok_or_else(|| {
        err(
            CallErrorKind::Malformed,
            Some(&name),
            "expected '>' after the function name",
        )
    })?;

    let tool = find_tool(tools, &name).ok_or_else(|| {
        let known: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        err(
            CallErrorKind::UnknownTool,
            Some(&name),
            format!("unknown tool '{name}'; available: {}", known.join(", ")),
        )
    })?;

    let mut raw_args: Vec<(String, String)> = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let Some(after) = rest.strip_prefix("<param") else {
            return Err(err(
                CallErrorKind::Malformed,
                Some(&name),
                format!("unexpected text inside <function>: {}", preview(rest)),
            ));
        };
        let (pname, after) = parse_attr(after, "name").ok_or_else(|| {
            err(
                CallErrorKind::Malformed,
                Some(&name),
                "missing <param name=\"…\">",
            )
        })?;
        let after = after.trim_start().strip_prefix('>').ok_or_else(|| {
            err(
                CallErrorKind::Malformed,
                Some(&name),
                "expected '>' after the param name",
            )
        })?;
        let (value, after) = if let Some(cdata) = after.trim_start().strip_prefix("<![CDATA[") {
            let end = cdata
                .find("]]>")
                .ok_or_else(|| err(CallErrorKind::Malformed, Some(&name), "unterminated CDATA"))?;
            let tail = cdata[end + 3..].trim_start();
            let tail = tail.strip_prefix("</param>").ok_or_else(|| {
                err(
                    CallErrorKind::Malformed,
                    Some(&name),
                    format!("missing </param> for '{pname}'"),
                )
            })?;
            (cdata[..end].to_string(), tail)
        } else {
            let end = after.find("</param>").ok_or_else(|| {
                err(
                    CallErrorKind::Malformed,
                    Some(&name),
                    format!("missing </param> for '{pname}'"),
                )
            })?;
            (
                unescape(after[..end].trim()),
                &after[end + "</param>".len()..],
            )
        };
        raw_args.push((pname.trim().to_string(), value));
        rest = after;
    }

    let args = tool.normalize_args(
        raw_args
            .into_iter()
            .map(|(key, value)| (key, Value::String(value))),
    )?;
    Ok(ToolCall {
        name: tool.name.clone(),
        args,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "exec".into(),
                description: "run".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "minLength": 1},
                        "timeout_sec": {"type": "integer", "minimum": 1, "maximum": 600},
                    },
                    "required": ["command"],
                    "additionalProperties": false,
                }),
            },
            ToolSpec {
                name: "list_dir".into(),
                description: "ls".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "depth": {"type": "integer"}, "all": {"type": "boolean"}},
                }),
            },
        ]
    }

    #[test]
    fn parses_basic_call() {
        let c = parse_call(
            r#"<function name="exec"><param name="command">ls -la</param><param name="timeout_sec"> 30 </param></function>"#,
            &tools(),
        )
        .unwrap();
        assert_eq!(c.name, "exec");
        assert_eq!(c.str_arg("command"), Some("ls -la"));
        assert_eq!(c.int_arg("timeout_sec"), Some(30));
    }

    #[test]
    fn both_protocols_prefer_exact_names_and_reject_ambiguous_aliases() {
        let tools = [
            ToolSpec {
                name: "exec".into(),
                description: String::new(),
                parameters: json!({"x-name-aliases": ["run", "shell"]}),
            },
            ToolSpec {
                name: "run".into(),
                description: String::new(),
                parameters: json!({"x-name-aliases": ["shell"]}),
            },
        ];
        for (name, expected) in [
            ("exec", Some("exec")),
            ("EXEC", Some("exec")),
            ("run", Some("run")),
            ("shell", None),
            ("missing", None),
        ] {
            for result in [
                parse_call(&format!("<function name=\"{name}\"></function>"), &tools),
                qwen35::parse_call(&format!("<function={name}></function>"), &tools),
            ] {
                match expected {
                    Some(expected) => assert_eq!(result.unwrap().name, expected),
                    None => assert_eq!(result.unwrap_err().kind, CallErrorKind::UnknownTool),
                }
            }
        }
    }

    #[test]
    fn parses_cdata_and_entities() {
        let c = parse_call(
            "<function name=\"exec\"><param name=\"command\"><![CDATA[echo <x> && printf 'a\nb']]></param></function>",
            &tools(),
        )
        .unwrap();
        assert_eq!(c.str_arg("command"), Some("echo <x> && printf 'a\nb'"));
        let c = parse_call(
            r#"<function name="exec"><param name="command">grep -c &quot;a&amp;b&quot; f &lt; in &#x41;&#66;</param></function>"#,
            &tools(),
        )
        .unwrap();
        assert_eq!(c.str_arg("command"), Some("grep -c \"a&b\" f < in AB"));
    }

    #[test]
    fn entity_scanning_preserves_utf8_at_and_after_the_lookahead_boundary() {
        for (input, expected) in [
            ("a&amp;中文中文.txt", "a&中文中文.txt"),
            ("&amp;🙂🙂🙂", "&🙂🙂🙂"),
            ("&中文中文中文", "&中文中文中文"),
            ("&中文中文;尾", "&中文中文;尾"),
            ("&abcdefghijklmnop;中文", "&abcdefghijklmnop;中文"),
            ("&#x1f642;中文", "🙂中文"),
        ] {
            assert_eq!(unescape(input), expected);
        }
        for value in ["a&amp;中文中文.txt", "<![CDATA[a&中文中文.txt]]>"] {
            let raw = format!(
                "<function name=\"exec\"><param name=\"command\">{value}</param></function>"
            );
            assert_eq!(
                parse_call(&raw, &tools()).unwrap().str_arg("command"),
                Some("a&中文中文.txt")
            );
        }
    }

    #[test]
    fn cdata_may_contain_param_close() {
        let c = parse_call(
            "<function name=\"exec\"><param name=\"command\"><![CDATA[echo '</param>']]></param></function>",
            &tools(),
        )
        .unwrap();
        assert_eq!(c.str_arg("command"), Some("echo '</param>'"));
    }

    #[test]
    fn invalid_parameters_are_not_ignored_or_overwritten() {
        for (raw, kind, message) in [
            (
                r#"<function name="exec"><param name="command">pwd</param><param name="cwd">/tmp</param></function>"#,
                CallErrorKind::Malformed,
                "allowed: command, timeout_sec",
            ),
            (
                r#"<function name="exec"><param name="command">pwd</param><param name="command">ls</param></function>"#,
                CallErrorKind::Malformed,
                "conflicting values for param 'command'",
            ),
            (
                r#"<function name="exec"><param name="command">pwd</param><param name="timeout_sec">0</param></function>"#,
                CallErrorKind::BadType,
                "at least 1",
            ),
            (
                r#"<function name="exec"><param name="command">pwd</param><param name="timeout_sec">601</param></function>"#,
                CallErrorKind::BadType,
                "at most 600",
            ),
        ] {
            let error = parse_call(raw, &tools()).unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(error.tool.as_deref(), Some("exec"));
            assert!(error.message.contains(message), "{error}");
        }
        let call = parse_call(
            r#"<function name="EXEC"><param name="command">pwd</param><param name="command">pwd</param><param name="timeout-sec">5</param><param name="timeout_sec">5.0</param></function>"#,
            &tools(),
        ).unwrap();
        assert_eq!(call.name, "exec");
        assert_eq!(call.str_arg("command"), Some("pwd"));
        assert_eq!(call.int_arg("timeout_sec"), Some(5));
    }

    #[test]
    fn json_array_parameters_keep_their_declared_shape() {
        let tool = ToolSpec {
            name: "ask_user".into(),
            description: "Ask.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "choices": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        };
        for bad in [r#""yes""#, "{}", "[1]"] {
            let raw = format!(
                r#"<function name="ask_user"><param name="question">Format?</param><param name="choices">{bad}</param></function>"#
            );
            let error = parse_call(&raw, std::slice::from_ref(&tool)).unwrap_err();
            assert_eq!(error.kind, CallErrorKind::BadType);
            assert_eq!(error.tool.as_deref(), Some("ask_user"));
        }
        let call = parse_call(
            r#"<function name="ask_user"><param name="question">Format?</param><param name="choices">["tar.gz","zip"]</param></function>"#,
            std::slice::from_ref(&tool),
        )
        .unwrap();
        assert_eq!(call.args["choices"], json!(["tar.gz", "zip"]));
        let call = parse_call(
            r#"<function name="ask_user"><param name="question">Format?</param><param name="choices">null</param></function>"#,
            std::slice::from_ref(&tool),
        ).unwrap();
        assert!(!call.args.contains_key("choices"));
    }

    #[test]
    fn schema_conversion_and_errors() {
        let c = parse_call(
            r#"<function name="list_dir"><param name="path">.</param><param name="all">TRUE</param></function>"#,
            &tools(),
        )
        .unwrap();
        assert_eq!(c.args["all"], json!(true));

        let e = parse_call(
            r#"<function name="exec"><param name="timeout_sec">5</param></function>"#,
            &tools(),
        )
        .unwrap_err();
        assert_eq!(e.kind, CallErrorKind::MissingParam);

        let e = parse_call(
            r#"<function name="exec"><param name="command">x</param><param name="timeout_sec">soon</param></function>"#,
            &tools(),
        )
        .unwrap_err();
        assert_eq!(e.kind, CallErrorKind::BadType);

        // Integers written as floats are accepted only when they are whole
        // numbers in range.
        let timeout = |v: &str| {
            parse_call(
                &format!(
                    r#"<function name="exec"><param name="command">x</param><param name="timeout_sec">{v}</param></function>"#
                ),
                &tools(),
            )
        };
        assert_eq!(timeout("60.0").unwrap().int_arg("timeout_sec"), Some(60));
        assert_eq!(timeout("1e2").unwrap().int_arg("timeout_sec"), Some(100));
        for bad in ["1.9", "-0.5", "NaN", "inf", "-infinity", "1e19", "-1e30"] {
            let e = timeout(bad).unwrap_err();
            assert_eq!(e.kind, CallErrorKind::BadType, "{bad}");
            assert!(
                e.message.contains("must be an integer"),
                "{bad}: {}",
                e.message
            );
        }

        let e = parse_call(r#"<function name="rm_rf"></function>"#, &tools()).unwrap_err();
        assert_eq!(e.kind, CallErrorKind::UnknownTool);
        assert_eq!(e.tool.as_deref(), Some("rm_rf"));

        let e = parse_call(r#"<function name="exec">junk</function>"#, &tools()).unwrap_err();
        assert_eq!(e.kind, CallErrorKind::Malformed);

        let e = parse_call(
            r#"<function name="exec"><param name="command">ls</function>"#,
            &tools(),
        )
        .unwrap_err();
        assert_eq!(e.kind, CallErrorKind::Malformed);
    }

    fn feed(p: &mut StreamParser, items: &[(u32, &str)]) -> Vec<Parsed> {
        let mut out = Vec::new();
        for (id, s) in items {
            out.extend(p.push_bytes(*id, s.as_bytes()));
        }
        out.extend(p.finish());
        out
    }

    const TXT: u32 = 1000;

    #[test]
    fn host_prefilled_tool_opening_is_parsed_with_generated_arguments() {
        let prefix = crate::template::concat(&crate::template::tool_choice_prefix(
            &nosh_engine::ToolChoice::Named("exec".into()),
        ));
        assert_eq!(prefix, "<function name=\"exec\">");
        assert!(crate::template::tool_choice_prefix(&nosh_engine::ToolChoice::Auto).is_empty());
        let mut parser = StreamParser::new(tools(), false);
        assert!(parser.push_bytes(FUNCTION_OPEN, b"").is_empty());
        assert!(
            parser
                .push_bytes(TXT, prefix.trim_start_matches("<function").as_bytes())
                .is_empty()
        );
        assert!(parser.in_call());
        let result = feed(
            &mut parser,
            &[
                (PARAM_OPEN, ""),
                (TXT, " name=\"command\">echo ok"),
                (PARAM_CLOSE, ""),
                (FUNCTION_CLOSE, ""),
            ],
        );
        assert!(
            matches!(result.as_slice(), [Parsed::Call(Ok(call))] if call.str_arg("command") == Some("echo ok"))
        );
    }

    #[test]
    fn stream_state_machine() {
        let mut p = StreamParser::new(tools(), false);
        let out = feed(
            &mut p,
            &[
                (TXT, "Let me"),
                (TXT, " check."),
                (TXT, "\n"),
                (FUNCTION_OPEN, ""),
                (TXT, " name=\"exec\">"),
                (PARAM_OPEN, ""),
                (TXT, " name=\"command\">"),
                (TXT, "ls"),
                (TXT, " /tmp"),
                (PARAM_CLOSE, ""),
                (FUNCTION_CLOSE, ""),
            ],
        );
        let text: String = out
            .iter()
            .filter_map(|e| match e {
                Parsed::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Let me check.\n");
        let calls: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                Parsed::Call(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].as_ref().unwrap().str_arg("command"),
            Some("ls /tmp")
        );
    }

    #[test]
    fn think_blocks_and_truncation() {
        let mut p = StreamParser::new(tools(), true);
        let out = feed(
            &mut p,
            &[
                (TXT, "hmm"),
                (THINK_CLOSE, ""),
                (TXT, "ok"),
                (FUNCTION_OPEN, ""),
                (TXT, " name=\"exec\">"),
                (PARAM_OPEN, ""),
                (TXT, " name=\"command\">ls"),
            ],
        );
        assert_eq!(out[0], Parsed::Think("hmm".into()));
        assert_eq!(out[1], Parsed::Text("ok".into()));
        match &out[2] {
            Parsed::Call(Err(e)) => {
                assert_eq!(e.kind, CallErrorKind::Truncated);
                assert_eq!(e.tool.as_deref(), Some("exec"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn utf8_split_across_tokens() {
        let mut p = StreamParser::new(vec![], false);
        let b = "端口".as_bytes();
        let mut out = p.push_bytes(TXT, &b[..4]);
        out.extend(p.push_bytes(TXT, &b[4..]));
        out.extend(p.finish());
        let text: String = out
            .iter()
            .map(|e| match e {
                Parsed::Text(t) => t.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(text, "端口");
    }
}
