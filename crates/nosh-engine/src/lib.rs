//! Engine-facing types: messages in, structured events out. Nothing here
//! exposes template text, special tokens or tool-call syntax.

pub mod mock;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use mock::MockChatEngine;

/// Recoverable protocol errors and opaque, source-preserving backend failures.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Config(String),
    #[error("context is full ({used} of {max} tokens)")]
    ContextFull { used: usize, max: usize },
    #[error("unknown session {0}")]
    UnknownSession(SessionId),
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A tool the model may call. `parameters` is a JSON Schema object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    pub fn matches_name(&self, name: &str) -> bool {
        same_identifier(&self.name, name)
            || aliases(&self.parameters, "x-name-aliases").any(|alias| same_identifier(alias, name))
    }

    /// Compatibility metadata stays host-side, not in the model's tool catalog.
    pub fn model_parameters(&self) -> Value {
        let mut schema = self.parameters.clone();
        Self::strip_compatibility_metadata(&mut schema);
        schema
    }

    fn strip_compatibility_metadata(schema: &mut Value) {
        if let Some(object) = schema.as_object_mut() {
            for key in [
                "x-name-aliases",
                "x-ignored-parameters",
                "x-aliases",
                "x-scalar-text",
                "x-compatibility-only",
            ] {
                object.remove(key);
            }
        }
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.retain(|_, parameter| parameter["x-compatibility-only"] != true);
            for parameter in properties.values_mut() {
                Self::strip_compatibility_metadata(parameter);
            }
        }
        if let Some(items) = schema.get_mut("items") {
            Self::strip_compatibility_metadata(items);
        }
    }

    pub fn prepare_call(&self, call: &ToolCall) -> Result<ToolCall, CallError> {
        if !self.matches_name(&call.name) {
            return Err(self.error(
                CallErrorKind::UnknownTool,
                format!("unknown tool '{}'; expected {}", call.name, self.name),
            ));
        }
        Ok(ToolCall {
            name: self.name.clone(),
            args: self.normalize_args(call.args.clone())?,
        })
    }

    /// Accepts equivalent spellings and values without changing string contents.
    pub fn normalize_args(
        &self,
        args: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<Map<String, Value>, CallError> {
        let properties = self.parameters.get("properties").and_then(Value::as_object);
        let required = self.required();
        let mut normalized = Map::new();
        for (name, value) in args {
            let mut matches = properties
                .into_iter()
                .flat_map(|properties| properties.iter())
                .filter(|(key, schema)| {
                    same_identifier(key, &name)
                        || aliases(schema, "x-aliases").any(|alias| same_identifier(alias, &name))
                });
            let parameter =
                match properties.and_then(|properties| properties.get_key_value(name.trim())) {
                    Some(exact) => Some(exact),
                    None => {
                        let first = matches.next();
                        if matches.next().is_some() {
                            return Err(self.error(
                                CallErrorKind::Malformed,
                                format!("ambiguous param '{name}' for {}", self.name),
                            ));
                        }
                        first
                    }
                };
            let (key, value) = match parameter {
                Some((key, schema)) => {
                    if !required.contains(&key.as_str())
                        && optional_value_is_missing(&value, schema)
                    {
                        continue;
                    }
                    let value = normalize_parameter(key, value, schema)
                        .map_err(|message| self.error(CallErrorKind::BadType, message))?;
                    (key.to_string(), value)
                }
                None => {
                    if aliases(&self.parameters, "x-ignored-parameters")
                        .any(|ignored| same_identifier(ignored, &name))
                    {
                        continue;
                    }
                    if self.parameters["additionalProperties"] == false {
                        let allowed = properties
                            .into_iter()
                            .flat_map(|properties| properties.iter())
                            .filter(|(_, schema)| schema["x-compatibility-only"] != true)
                            .map(|(key, _)| key.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(self.error(
                            CallErrorKind::Malformed,
                            format!(
                                "unknown param '{name}' for {}; allowed: {allowed}",
                                self.name
                            ),
                        ));
                    }
                    (name, value)
                }
            };
            if let Some(previous) = normalized.get(&key) {
                if previous != &value {
                    return Err(self.error(
                        CallErrorKind::Malformed,
                        format!("conflicting values for param '{key}' for {}", self.name),
                    ));
                }
            } else {
                normalized.insert(key, value);
            }
        }
        for field in required {
            if !normalized.contains_key(field) {
                return Err(self.error(
                    CallErrorKind::MissingParam,
                    format!("missing required param '{field}' for {}", self.name),
                ));
            }
        }
        Ok(normalized)
    }

    fn error(&self, kind: CallErrorKind, message: String) -> CallError {
        CallError {
            kind,
            message,
            tool: Some(self.name.clone()),
        }
    }

    pub fn required(&self) -> Vec<&str> {
        self.parameters
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }
}

fn same_identifier(left: &str, right: &str) -> bool {
    fn characters(text: &str) -> impl Iterator<Item = char> + '_ {
        text.trim()
            .chars()
            .filter(|character| !matches!(character, '_' | '-'))
            .map(|character| character.to_ascii_lowercase())
    }
    characters(left).eq(characters(right))
}

fn aliases<'a>(schema: &'a Value, key: &str) -> impl Iterator<Item = &'a str> {
    schema
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

fn optional_value_is_missing(value: &Value, schema: &Value) -> bool {
    let kind = schema["type"].as_str().unwrap_or("");
    if !matches!(
        kind,
        "string" | "integer" | "number" | "boolean" | "array" | "object"
    ) {
        return false;
    }
    if value.is_null() || (kind != "array" && value.as_array().is_some_and(Vec::is_empty)) {
        return true;
    }
    value.as_str().is_some_and(|text| {
        if kind == "string" {
            schema["minLength"]
                .as_u64()
                .is_some_and(|minimum| minimum > 0)
                && text.is_empty()
        } else {
            text.trim().is_empty()
                || text.trim().eq_ignore_ascii_case("null")
                || text.trim().eq_ignore_ascii_case("none")
        }
    })
}

fn integer_text(text: &str) -> Option<i64> {
    if let Ok(value) = text.parse() {
        return Some(value);
    }
    let (sign, text) = if let Some(text) = text.strip_prefix('-') {
        ("-", text)
    } else {
        ("", text.strip_prefix('+').unwrap_or(text))
    };
    let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
    let exponent_digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
        || exponent_digits.is_empty()
        || !exponent_digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let trimmed = digits.trim_start_matches('0').trim_end_matches('0');
    if trimmed.is_empty() {
        return Some(0);
    }
    let trailing = digits.len() - digits.trim_end_matches('0').len();
    let zeros = exponent
        .parse::<i64>()
        .ok()?
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(i64::try_from(trailing).ok()?)?;
    let zeros = usize::try_from(zeros).ok()?;
    if trimmed.len().checked_add(zeros)? > 19 {
        return None;
    }
    format!("{sign}{trimmed}{}", "0".repeat(zeros)).parse().ok()
}

fn normalize_parameter(name: &str, mut value: Value, schema: &Value) -> Result<Value, String> {
    let kind = schema["type"].as_str().unwrap_or("");
    if matches!(
        kind,
        "string" | "integer" | "number" | "boolean" | "object" | "null"
    ) && let Value::Array(values) = &mut value
        && values.len() == 1
    {
        value = values.remove(0);
    }
    if kind == "string"
        && schema["x-scalar-text"] == true
        && (value.is_number() || value.is_boolean())
    {
        value = Value::String(value.to_string());
    }
    if let Value::String(text) = &value {
        let text = text.trim();
        match kind {
            "integer" | "number" => {
                let text = serde_json::from_str::<Value>(text)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| text.to_owned());
                value = if kind == "integer" {
                    Value::from(integer_text(&text).ok_or_else(|| {
                        format!("param '{name}' must be an integer, got '{text}'")
                    })?)
                } else if let Ok(integer) = text.parse::<i64>() {
                    Value::from(integer)
                } else {
                    let number = text
                        .parse::<f64>()
                        .ok()
                        .filter(|number| number.is_finite())
                        .ok_or_else(|| format!("param '{name}' must be a number, got '{text}'"))?;
                    Value::from(number)
                };
            }
            "boolean" => {
                value = match text.to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => Value::Bool(true),
                    "false" | "0" | "no" => Value::Bool(false),
                    _ => {
                        return Err(format!(
                            "param '{name}' must be true or false, got '{text}'"
                        ));
                    }
                };
            }
            "array" | "object" => {
                value = serde_json::from_str(text)
                    .map_err(|_| format!("param '{name}' must be JSON"))?;
                if let Value::String(text) = &value {
                    value = serde_json::from_str(text)
                        .map_err(|_| format!("param '{name}' must be JSON"))?;
                }
            }
            _ => {}
        }
    }
    if matches!(kind, "integer" | "number")
        && value.as_i64().is_none()
        && let Some(number) = value.as_f64()
        && number.is_finite()
        && number.fract() == 0.0
        && (i64::MIN as f64..i64::MAX as f64).contains(&number)
    {
        value = Value::from(number as i64);
    }
    if kind == "boolean" {
        match value.as_i64() {
            Some(0) => value = Value::Bool(false),
            Some(1) => value = Value::Bool(true),
            _ => {}
        }
    }
    if kind == "integer" && value.as_i64().is_none() {
        return Err(format!("param '{name}' must be an integer"));
    }
    if let Some(kind) = schema["type"].as_str() {
        let valid = match kind {
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            "null" => value.is_null(),
            _ => return Err(format!("unsupported type '{kind}' for param '{name}'")),
        };
        if !valid {
            return Err(format!("param '{name}' must have type '{kind}'"));
        }
    }
    if let Some(number) = value.as_i64() {
        if let Some(minimum) = schema["minimum"].as_i64()
            && number < minimum
        {
            return Err(format!("param '{name}' must be at least {minimum}"));
        }
        if let Some(maximum) = schema["maximum"].as_i64()
            && number > maximum
        {
            return Err(format!("param '{name}' must be at most {maximum}"));
        }
    }
    if let Some(text) = value.as_str()
        && let Some(minimum) = schema["minLength"].as_u64()
        && (text.chars().count() as u64) < minimum
    {
        return Err(format!(
            "param '{name}' must contain at least {minimum} characters"
        ));
    }
    if let Some(values) = value.as_array_mut() {
        let mut normalized = Vec::new();
        for (index, value) in std::mem::take(values).into_iter().enumerate() {
            let value = match schema.get("items") {
                Some(items) => normalize_parameter(&format!("{name}[{index}]"), value, items)?,
                None => value,
            };
            if schema["uniqueItems"] != true || !normalized.contains(&value) {
                normalized.push(value);
            }
            if let Some(maximum) = schema["maxItems"].as_u64()
                && normalized.len() as u64 > maximum
            {
                return Err(format!(
                    "param '{name}' must contain at most {maximum} items"
                ));
            }
        }
        *values = normalized;
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args: Map<String, Value>,
}

impl ToolCall {
    pub fn str_arg(&self, key: &str) -> Option<&str> {
        self.args.get(key).and_then(Value::as_str)
    }

    pub fn int_arg(&self, key: &str) -> Option<i64> {
        self.args.get(key).and_then(Value::as_i64)
    }
}

/// Conversation messages. Appended system context, user input and tool content
/// are encoded as plain text; the initial [`SessionSpec::system`] is trusted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool(String),
    /// A standalone tool reply supplied by the user; never compacted as tool output.
    UserAnswer(String),
}

/// One-line stand-in for an old tool result (keeps the status header).
pub fn shorten_tool_result(content: &str) -> String {
    const KEEP: usize = 200;
    let count = content.chars().count();
    if count <= KEEP + 40 {
        return content.to_string();
    }
    let end = content.char_indices().nth(KEEP).expect("long result").0;
    format!(
        "{}\n[\u{2026} older output omitted to save context ({count} chars)]",
        &content[..end]
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallErrorKind {
    Malformed,
    UnknownTool,
    MissingParam,
    BadType,
    Truncated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallError {
    pub kind: CallErrorKind,
    pub message: String,
    /// Tool name if it could be parsed.
    pub tool: Option<String>,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// Visible answer text (streamed).
    Text(String),
    /// Reasoning text inside `<think>` (streamed).
    Think(String),
    ToolCall(ToolCall),
    CallError(CallError),
    /// Prompt processing progress in tokens.
    Prefill {
        done: usize,
        total: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    EndOfTurn,
    MaxTokens,
    Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens that had to be prefilled this step.
    pub prompt_tokens: usize,
    /// Tokens reused from the KV cache.
    pub cached_tokens: usize,
    pub completion_tokens: usize,
    pub prefill_secs: f64,
    pub decode_secs: f64,
    /// Time from `step` start to the first sampled token.
    pub ttft_secs: f64,
    pub context_used: usize,
    pub context_max: usize,
}

impl Usage {
    /// Adds one completed step, retaining the first step's TTFT and latest context.
    pub fn add_step(&mut self, step: &Self, first: bool) {
        self.prompt_tokens += step.prompt_tokens;
        self.cached_tokens += step.cached_tokens;
        self.completion_tokens += step.completion_tokens;
        self.prefill_secs += step.prefill_secs;
        self.decode_secs += step.decode_secs;
        if first {
            self.ttft_secs = step.ttft_secs;
        }
        self.context_used = step.context_used;
        self.context_max = step.context_max;
    }

    pub fn prefill_tps(&self) -> f64 {
        self.prompt_tokens as f64 / self.prefill_secs.max(1e-9)
    }

    pub fn decode_tps(&self) -> f64 {
        self.completion_tokens as f64 / self.decode_secs.max(1e-9)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepOutcome {
    pub text: String,
    pub think: String,
    pub tool_calls: Vec<ToolCall>,
    pub errors: Vec<CallError>,
    pub stop: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SamplingParams {
    pub temperature: f32,
    pub top_p: f32,
    pub min_p: f32,
    /// Applied only once repetition is detected (design §7.3).
    pub repetition_penalty: f32,
    /// Temperature inside `<function … </function>`.
    pub tool_call_temperature: f32,
    pub seed: Option<u64>,
}

impl Default for SamplingParams {
    fn default() -> Self {
        Self {
            temperature: 1.0,
            top_p: 0.95,
            min_p: 0.0,
            repetition_penalty: 1.05,
            tool_call_temperature: 0.3,
            seed: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    /// Host-side provenance for observations; never rendered into model tokens.
    pub label: String,
    /// Trusted static system prompt; `<tool_def_sep>` marks where tool definitions go.
    pub system: String,
    pub tools: Vec<ToolSpec>,
    pub thinking: bool,
    pub sampling: SamplingParams,
    pub max_new_tokens: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "name", rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    /// Disable tool-call decoding for this step without changing the conversation.
    None,
    Required,
    Named(String),
}

pub type SessionId = u64;

/// Cheap, thread-safe cancellation flag (e.g. set from a SIGINT handler).
#[derive(Debug, Clone, Default)]
pub struct CancelHandle(Arc<AtomicBool>);

impl CancelHandle {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Messages in, events out (design §3.4). Implemented by inference adapters
/// and the scripted [`MockChatEngine`], without requiring a model runtime.
pub trait ChatEngine: Send {
    fn open(&mut self, spec: SessionSpec) -> Result<SessionId, EngineError>;

    /// One-step decoding policy, reset to Auto after the next step.
    /// None disables tool calls; Required/Named return one call, not prose.
    fn set_tool_choice(&mut self, _sid: SessionId, _choice: ToolChoice) -> Result<(), EngineError> {
        Err(EngineError::Config(
            "engine does not support tool choice".into(),
        ))
    }

    /// Appends `append` to the conversation and generates one assistant turn.
    /// System messages append plain-text context; they do not replace the initial system prompt.
    fn step(
        &mut self,
        sid: SessionId,
        append: Vec<Message>,
        sink: &mut dyn FnMut(Event),
    ) -> Result<StepOutcome, EngineError>;

    /// Keeps the first `keep` appended/generated messages, including system context.
    /// The initial system prefix is never removed.
    fn rewind(&mut self, sid: SessionId, keep: usize) -> Result<(), EngineError>;

    /// Number of appended/generated messages, excluding only the initial system prefix.
    fn message_count(&self, sid: SessionId) -> usize;

    /// Replaces the content of tool results older than the last `keep_recent`
    /// messages with a one-line note; returns the number of shortened results.
    /// A grouped tool turn overlapping recent messages may be kept intact.
    /// Tokenization failures leave the conversation unchanged.
    fn compact_tool_results(
        &mut self,
        sid: SessionId,
        keep_recent: usize,
    ) -> Result<usize, EngineError>;

    /// `(used, max)` context tokens.
    fn context_usage(&self, sid: SessionId) -> (usize, usize);

    fn cancel_handle(&self) -> CancelHandle;

    fn cancel(&self, _sid: SessionId) {
        self.cancel_handle().cancel();
    }

    /// Optional host observations, separate from model input and output.
    fn record_observation(&mut self, _sid: SessionId, _value: Value) -> Result<(), EngineError> {
        Ok(())
    }

    fn close(&mut self, sid: SessionId);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_text_compatibility_does_not_round_away_fractional_or_large_values() {
        let spec = ToolSpec {
            name: "lines".into(),
            description: "Lines.".into(),
            parameters: serde_json::json!({
                "type":"object","properties":{"line":{"type":"integer"}},"required":["line"]
            }),
        };
        for (text, expected) in [
            ("60.0", 60),
            ("1e2", 100),
            ("1.20e1", 12),
            ("9007199254740993.0", 9_007_199_254_740_993),
            ("9223372036854775807.0", i64::MAX),
            ("-9223372036854775808.0", i64::MIN),
        ] {
            let args = spec
                .normalize_args([("line".into(), Value::String(text.into()))])
                .unwrap();
            assert_eq!(args["line"], Value::from(expected), "{text}");
        }
        for text in [
            "9007199254740992.5",
            "1.0000000000000000001",
            "1.2",
            "1e30",
            "0e--2",
            "0e+-1",
        ] {
            assert!(
                spec.normalize_args([("line".into(), Value::String(text.into()))])
                    .is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn tool_compatibility_normalizes_equivalent_values_without_changing_text() {
        let spec = ToolSpec {
            name: "read_file".into(),
            description: "Read.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type":"string","x-aliases":["file_path"]},
                    "start_line": {"type":"integer","x-aliases":["start"]},
                    "choices": {"type":"array","uniqueItems":true,"maxItems":2,
                        "items":{"type":"string"}}
                },
                "required": ["path"],
                "additionalProperties": false,
                "x-name-aliases": ["read_text_file"],
                "x-ignored-parameters": ["reason"]
            }),
        };
        let path = " \"a & b\" \n";
        let call = ToolCall {
            name: "ReadTextFile".into(),
            args: serde_json::json!({
                "filePath":[path],"start":"3.0","startLine":3.0,
                "choices":"[\"yes\",\"yes\",\"no\"]","reason":"inspect"
            })
            .as_object()
            .unwrap()
            .clone(),
        };
        let prepared = spec.prepare_call(&call).unwrap();
        assert_eq!(prepared.name, "read_file");
        assert_eq!(prepared.str_arg("path"), Some(path));
        assert_eq!(prepared.int_arg("start_line"), Some(3));
        assert_eq!(prepared.args["choices"], serde_json::json!(["yes", "no"]));
        assert!(!prepared.args.contains_key("reason"));
        assert_eq!(spec.prepare_call(&prepared).unwrap(), prepared);
        for value in [
            Value::Null,
            serde_json::json!(""),
            serde_json::json!("null"),
        ] {
            let mut call = prepared.clone();
            call.args.insert("start_line".into(), value);
            assert!(
                !spec
                    .prepare_call(&call)
                    .unwrap()
                    .args
                    .contains_key("start_line")
            );
        }
        for args in [
            serde_json::json!({"path":"a","file_path":"b"}),
            serde_json::json!({"path":["a","b"]}),
            serde_json::json!({"path":"a","start":1.5}),
            serde_json::json!({"path":"a","offset":10}),
        ] {
            assert!(
                spec.normalize_args(args.as_object().unwrap().clone())
                    .is_err(),
                "{args}"
            );
        }
        let model = spec.model_parameters();
        assert!(model.get("x-name-aliases").is_none());
        assert!(model["properties"]["path"].get("x-aliases").is_none());
    }

    #[test]
    fn tool_normalization_preserves_valid_calls_and_reports_actionable_errors() {
        let spec = ToolSpec {
            name: "example".into(),
            description: "Example.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "minLength": 1},
                    "timeout": {"type": "integer", "minimum": 1, "maximum": 600},
                    "choices": {"type": "array", "maxItems": 2, "uniqueItems": true,
                        "items": {"type": "string", "minLength": 1}}
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        };
        for args in [
            serde_json::json!({"text":"ok"}),
            serde_json::json!({"text":"ok","timeout":1,"choices":[]}),
            serde_json::json!({"text":"ok","timeout":600,"choices":["yes","no"]}),
            serde_json::json!({"text":"\u{4e2d}","choices":["\u{6587}"]}),
        ] {
            assert_eq!(
                spec.normalize_args(args.as_object().unwrap().clone())
                    .unwrap(),
                *args.as_object().unwrap()
            );
        }
        for (args, kind, message) in [
            (serde_json::json!({}), CallErrorKind::MissingParam, "'text'"),
            (
                serde_json::json!({"text":"ok","timout":5}),
                CallErrorKind::Malformed,
                "allowed: text, timeout, choices",
            ),
            (
                serde_json::json!({"text":""}),
                CallErrorKind::BadType,
                "'text'",
            ),
            (
                serde_json::json!({"text":5}),
                CallErrorKind::BadType,
                "string",
            ),
            (
                serde_json::json!({"text":"ok","timeout":0}),
                CallErrorKind::BadType,
                "at least 1",
            ),
            (
                serde_json::json!({"text":"ok","timeout":601}),
                CallErrorKind::BadType,
                "at most 600",
            ),
            (
                serde_json::json!({"text":"ok","choices":{}}),
                CallErrorKind::BadType,
                "array",
            ),
            (
                serde_json::json!({"text":"ok","choices":[5]}),
                CallErrorKind::BadType,
                "choices[0]",
            ),
            (
                serde_json::json!({"text":"ok","choices":[""]}),
                CallErrorKind::BadType,
                "choices[0]",
            ),
            (
                serde_json::json!({"text":"ok","choices":["a","b","c"]}),
                CallErrorKind::BadType,
                "at most 2",
            ),
        ] {
            let error = spec
                .normalize_args(args.as_object().unwrap().clone())
                .unwrap_err();
            assert_eq!(error.kind, kind, "{args}");
            assert_eq!(error.tool.as_deref(), Some("example"));
            assert!(error.message.contains(message), "{args}: {error}");
        }
        let mut open = spec.clone();
        open.parameters
            .as_object_mut()
            .unwrap()
            .remove("additionalProperties");
        open.normalize_args(
            serde_json::json!({"text":"ok","extra":true})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(
            spec.normalize_args(
                serde_json::json!({"text":"ok","timeout":"5","choices":["yes","yes"]})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap(),
            *serde_json::json!({"text":"ok","timeout":5,"choices":["yes"]})
                .as_object()
                .unwrap(),
        );
    }

    #[test]
    fn opaque_and_nullable_parameters_preserve_valid_array_and_null_values() {
        let spec = ToolSpec {
            name: "data".into(),
            description: "Data.".into(),
            parameters: serde_json::json!({
                "type":"object","additionalProperties":false,
                "properties":{
                    "opaque":{},
                    "nullable":{"type":["string","null"]},
                    "null_value":{"type":"null"}
                }
            }),
        };
        for opaque in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!(["literal"]),
            serde_json::json!([1, 2]),
            serde_json::json!({"value":"null"}),
        ] {
            let args = serde_json::json!({"opaque":opaque,"nullable":null,"null_value":null})
                .as_object()
                .unwrap()
                .clone();
            assert_eq!(spec.normalize_args(args.clone()).unwrap(), args);
        }
    }

    #[test]
    fn usage_adds_costs_but_keeps_first_ttft_and_latest_context() {
        for first_ttft in [0.0, 0.5] {
            let first = Usage {
                prompt_tokens: 100,
                cached_tokens: 20,
                completion_tokens: 10,
                prefill_secs: 0.25,
                decode_secs: 0.5,
                ttft_secs: first_ttft,
                context_used: 120,
                context_max: 8192,
            };
            let next = Usage {
                prompt_tokens: 25,
                cached_tokens: 110,
                completion_tokens: 5,
                prefill_secs: 0.5,
                decode_secs: 0.25,
                ttft_secs: 2.0,
                context_used: 135,
                context_max: 16384,
            };
            let mut total = Usage::default();
            total.add_step(&first, true);
            assert_eq!(total, first);
            total.add_step(&next, false);
            assert_eq!(
                total,
                Usage {
                    prompt_tokens: 125,
                    cached_tokens: 130,
                    completion_tokens: 15,
                    prefill_secs: 0.75,
                    decode_secs: 0.75,
                    ttft_secs: first_ttft,
                    context_used: 135,
                    context_max: 16384,
                }
            );
            assert_eq!(total.decode_tps(), 20.0);
        }
    }

    #[test]
    fn shortening_preserves_unicode_and_the_existing_marker() {
        let short = "\u{4e2d}".repeat(240);
        assert_eq!(shorten_tool_result(&short), short);
        let long = "\u{4e2d}".repeat(241);
        assert_eq!(
            shorten_tool_result(&long),
            format!(
                "{}\n[\u{2026} older output omitted to save context (241 chars)]",
                "\u{4e2d}".repeat(200)
            )
        );
    }

    #[test]
    fn cancellation_is_shared_and_resettable() {
        let cancel = CancelHandle::default();
        let worker = cancel.clone();
        std::thread::spawn(move || worker.cancel()).join().unwrap();
        assert!(cancel.is_cancelled());
        let observer = cancel.clone();
        cancel.reset();
        assert!(!observer.is_cancelled());
    }

    #[test]
    fn protocol_serialization_preserves_existing_shapes() {
        let message = Message::UserAnswer("keep backups".into());
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value, serde_json::json!({"UserAnswer": "keep backups"}));
        assert_eq!(serde_json::from_value::<Message>(value).unwrap(), message);
        for (choice, expected) in [
            (ToolChoice::Auto, serde_json::json!({"type": "auto"})),
            (ToolChoice::None, serde_json::json!({"type": "none"})),
            (
                ToolChoice::Required,
                serde_json::json!({"type": "required"}),
            ),
            (
                ToolChoice::Named("exec".into()),
                serde_json::json!({"type": "named", "name": "exec"}),
            ),
        ] {
            assert_eq!(serde_json::to_value(&choice).unwrap(), expected);
            assert_eq!(
                serde_json::from_value::<ToolChoice>(expected).unwrap(),
                choice
            );
        }
    }
}
