//! `json.dumps`-compatible serialization (Python default separators `", "` / `": "`,
//! `ensure_ascii=False`), as used by the chat template's `tojson` filter.

use std::io;

use serde::Serialize;
use serde_json::ser::Formatter;

struct PyFormatter;

impl Formatter for PyFormatter {
    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }
}

pub fn dumps<T: Serialize + ?Sized>(value: &T) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, PyFormatter);
    value
        .serialize(&mut ser)
        .expect("serializing to memory cannot fail");
    String::from_utf8(out).expect("serde_json emits UTF-8")
}

/// Renders a JSON value the way Jinja prints it with `{{ value }}` (Python `str()`).
pub fn py_str(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        Value::Number(n) => n.to_string(),
        other => dumps(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_separators_and_escapes() {
        let v = json!({"a": [1, 2, {"b": "x\"y\\z\n"}], "中": "文", "e": {}, "f": []});
        assert_eq!(
            dumps(&v),
            r#"{"a": [1, 2, {"b": "x\"y\\z\n"}], "中": "文", "e": {}, "f": []}"#
        );
        assert_eq!(dumps(&json!("\u{1}tab\t")), r#""\u0001tab\t""#);
    }

    #[test]
    fn jinja_str() {
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(60)), "60");
        assert_eq!(py_str(&json!("ls")), "ls");
    }
}
