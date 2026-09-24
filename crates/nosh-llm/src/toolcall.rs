//! Streaming output parser driven by special-token ids (design §5.6):
//! TEXT ⇄ THINK via 8/9, TEXT → CALL via 18 `<function`, CALL → TEXT via 19
//! `</function>`. Inside a call, `<param`/`</param>` (20/21) delimit arguments;
//! values may be CDATA or entity-escaped, and are converted per JSON Schema.

use serde_json::{Map, Value};

use crate::engine::{CallError, CallErrorKind, ToolCall, ToolSpec};
use crate::tokenizer::{Tok, Utf8Stream};

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
        let Some(semi) = tail[..tail.len().min(12)].find(';') else {
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

    let tool = tools.iter().find(|t| t.name == name).ok_or_else(|| {
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

    let mut args = Map::new();
    for (k, v) in raw_args {
        let typed = match tool.param_type(&k) {
            Some("integer") => v
                .trim()
                .parse::<i64>()
                .ok()
                .or_else(|| {
                    // `60.0` is an integer; `1.9`, NaN, infinities and values
                    // out of range are not.
                    let f = v.trim().parse::<f64>().ok()?;
                    (f.is_finite()
                        && f.fract() == 0.0
                        && (i64::MIN as f64..i64::MAX as f64).contains(&f))
                    .then_some(f as i64)
                })
                .map(Value::from)
                .ok_or_else(|| {
                    err(
                        CallErrorKind::BadType,
                        Some(&name),
                        format!("param '{k}' must be an integer, got '{v}'"),
                    )
                })?,
            Some("number") => v.trim().parse::<f64>().map(Value::from).map_err(|_| {
                err(
                    CallErrorKind::BadType,
                    Some(&name),
                    format!("param '{k}' must be a number, got '{v}'"),
                )
            })?,
            Some("boolean") => match v.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Value::Bool(true),
                "false" | "0" | "no" => Value::Bool(false),
                _ => {
                    return Err(err(
                        CallErrorKind::BadType,
                        Some(&name),
                        format!("param '{k}' must be true or false, got '{v}'"),
                    ));
                }
            },
            Some("array") | Some("object") => serde_json::from_str(&v).map_err(|_| {
                err(
                    CallErrorKind::BadType,
                    Some(&name),
                    format!("param '{k}' must be JSON, got '{v}'"),
                )
            })?,
            _ => Value::String(v),
        };
        args.insert(k, typed);
    }
    for req in tool.required() {
        if !args.contains_key(req) {
            return Err(err(
                CallErrorKind::MissingParam,
                Some(&name),
                format!("missing required param '{req}' for {name}"),
            ));
        }
    }
    Ok(ToolCall { name, args })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "run_command".into(),
                description: "run".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout_sec": {"type": "integer"},
                    },
                    "required": ["command"],
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
            r#"<function name="run_command"><param name="command">ls -la</param><param name="timeout_sec"> 30 </param></function>"#,
            &tools(),
        )
        .unwrap();
        assert_eq!(c.name, "run_command");
        assert_eq!(c.str_arg("command"), Some("ls -la"));
        assert_eq!(c.int_arg("timeout_sec"), Some(30));
    }

    #[test]
    fn parses_cdata_and_entities() {
        let c = parse_call(
            "<function name=\"run_command\"><param name=\"command\"><![CDATA[echo <x> && printf 'a\nb']]></param></function>",
            &tools(),
        )
        .unwrap();
        assert_eq!(c.str_arg("command"), Some("echo <x> && printf 'a\nb'"));
        let c = parse_call(
            r#"<function name="run_command"><param name="command">grep -c &quot;a&amp;b&quot; f &lt; in &#x41;&#66;</param></function>"#,
            &tools(),
        )
        .unwrap();
        assert_eq!(c.str_arg("command"), Some("grep -c \"a&b\" f < in AB"));
    }

    #[test]
    fn cdata_may_contain_param_close() {
        let c = parse_call(
            "<function name=\"run_command\"><param name=\"command\"><![CDATA[echo '</param>']]></param></function>",
            &tools(),
        )
        .unwrap();
        assert_eq!(c.str_arg("command"), Some("echo '</param>'"));
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
            r#"<function name="run_command"><param name="timeout_sec">5</param></function>"#,
            &tools(),
        )
        .unwrap_err();
        assert_eq!(e.kind, CallErrorKind::MissingParam);

        let e = parse_call(
            r#"<function name="run_command"><param name="command">x</param><param name="timeout_sec">soon</param></function>"#,
            &tools(),
        )
        .unwrap_err();
        assert_eq!(e.kind, CallErrorKind::BadType);

        // Integers written as floats are accepted only when they are whole
        // numbers in range.
        let timeout = |v: &str| {
            parse_call(
                &format!(
                    r#"<function name="run_command"><param name="command">x</param><param name="timeout_sec">{v}</param></function>"#
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

        let e =
            parse_call(r#"<function name="run_command">junk</function>"#, &tools()).unwrap_err();
        assert_eq!(e.kind, CallErrorKind::Malformed);

        let e = parse_call(
            r#"<function name="run_command"><param name="command">ls</function>"#,
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
    fn stream_state_machine() {
        let mut p = StreamParser::new(tools(), false);
        let out = feed(
            &mut p,
            &[
                (TXT, "Let me"),
                (TXT, " check."),
                (TXT, "\n"),
                (FUNCTION_OPEN, ""),
                (TXT, " name=\"run_command\">"),
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
                (TXT, " name=\"run_command\">"),
                (PARAM_OPEN, ""),
                (TXT, " name=\"command\">ls"),
            ],
        );
        assert_eq!(out[0], Parsed::Think("hmm".into()));
        assert_eq!(out[1], Parsed::Text("ok".into()));
        match &out[2] {
            Parsed::Call(Err(e)) => {
                assert_eq!(e.kind, CallErrorKind::Truncated);
                assert_eq!(e.tool.as_deref(), Some("run_command"));
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
