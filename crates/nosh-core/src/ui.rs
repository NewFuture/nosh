//! Rendering of agent activity: the `┃` block in the terminal, JSON Lines for
//! `nosh -a --json`, and a recorder for tests.

use std::io::Write;

use nosh_permissions::Risk;
use nosh_shell::{OutputSink, style};
use serde_json::{Value, json};

pub fn bar() -> String {
    style::cyan(style::glyph("┃", "|"))
}

#[derive(Debug, Clone, Default)]
pub struct TaskSummary {
    pub status: String,
    pub steps: usize,
    pub secs: f64,
    pub prompt_tokens: usize,
    pub cached_tokens: usize,
    pub completion_tokens: usize,
    pub prefill_tps: f64,
    pub decode_tps: f64,
    /// Time to the first token of the first step.
    pub ttft_secs: f64,
    pub context_used: usize,
    pub context_max: usize,
    pub note: Option<String>,
}

impl TaskSummary {
    /// One line of engine statistics (`NOSH_STATS=1`).
    pub fn stats_line(&self) -> String {
        let sep = style::glyph(" · ", " | ");
        let rss = nosh_llm::rss_mb()
            .map(|(cur, peak)| format!("{sep}rss {cur:.0}/{peak:.0} MB"))
            .unwrap_or_default();
        format!(
            "stats: prompt {} (+{} cached) tok @ {:.0} tok/s{sep}gen {} tok @ {:.1} tok/s{sep}ttft {:.2}s{sep}ctx {}/{}{rss}",
            self.prompt_tokens,
            self.cached_tokens,
            self.prefill_tps,
            self.completion_tokens,
            self.decode_tps,
            self.ttft_secs,
            self.context_used,
            self.context_max
        )
    }
}

fn stats_enabled() -> bool {
    std::env::var_os("NOSH_STATS").is_some_and(|v| !v.is_empty() && v != "0")
}

pub trait AgentUi {
    fn prefill(&mut self, _done: usize, _total: usize) {}
    /// Ends any partial line before something else writes to the terminal.
    fn pause(&mut self) {}
    fn text(&mut self, s: &str);
    fn think(&mut self, _s: &str) {}
    /// A tool is about to run: `label` such as `SAFE · auto`.
    fn tool_start(&mut self, tool: &str, detail: &str, risk: Option<Risk>, label: &str);
    fn output(&mut self, chunk: &str, is_err: bool);
    fn tool_end(&mut self, summary: &str);
    fn notice(&mut self, msg: &str);
    fn error(&mut self, msg: &str);
    fn proposed(&mut self, cmd: &str, explanation: Option<&str>);
    fn finish(&mut self, summary: &TaskSummary);
}

/// Adapts an [`AgentUi`] as the live output sink of an agent command.
pub struct UiSink<'a>(pub &'a mut dyn AgentUi);

impl OutputSink for UiSink<'_> {
    fn stdout(&mut self, chunk: &str) {
        self.0.output(chunk, false);
    }
    fn stderr(&mut self, chunk: &str) {
        self.0.output(chunk, true);
    }
}

const LIVE_LINES: usize = 8;

#[derive(Default)]
struct TextState {
    line_open: bool,
    started: bool,
    carriage_return: bool,
}

impl TextState {
    fn render(&mut self, s: &str, prefix: &str) -> String {
        let mut out = String::new();
        for ch in s.chars() {
            if !self.line_open {
                if ch == '\n' && !self.started {
                    continue;
                }
                out.push_str(prefix);
                self.line_open = true;
            }
            self.started = true;
            out.push(ch);
            if ch == '\n' {
                self.line_open = false;
            }
        }
        out
    }

    fn push(&mut self, s: &str, prefix: &str) -> String {
        let mut text = String::new();
        for ch in s.chars() {
            if std::mem::take(&mut self.carriage_return) && ch != '\n' {
                text.push('\r');
            }
            if ch == '\r' {
                self.carriage_return = true;
            } else {
                text.push(ch);
            }
        }
        self.render(&style::visible_text(&text), prefix)
    }

    fn finish(&mut self, prefix: &str) -> String {
        let mut out = if std::mem::take(&mut self.carriage_return) {
            self.render("\\r", prefix)
        } else {
            String::new()
        };
        if self.line_open {
            out.push('\n');
            self.line_open = false;
        }
        out
    }
}

/// Terminal renderer (stderr; the answer can go to stdout for `nosh -a`).
pub struct TermUi {
    bar: String,
    answer_to_stdout: bool,
    stdout: style::Terminal,
    stderr: style::Terminal,
    text: TextState,
    text_is_think: bool,
    status_shown: bool,
    output: [style::OutputBuffer; 2],
    shown: usize,
    hidden: usize,
    tail: Vec<(String, bool)>,
    pub show_think: bool,
}

impl TermUi {
    pub fn new(answer_to_stdout: bool) -> Self {
        Self {
            bar: bar(),
            answer_to_stdout,
            stdout: style::stdout(),
            stderr: style::stderr(),
            text: TextState::default(),
            text_is_think: false,
            status_shown: false,
            output: Default::default(),
            shown: 0,
            hidden: 0,
            tail: Vec::new(),
            show_think: false,
        }
    }

    fn clear_status(&mut self) {
        if self.status_shown {
            eprint!("\r\x1b[K");
            self.status_shown = false;
        }
    }

    fn end_text_line(&mut self) {
        let prefix = self.text_prefix();
        let out = self.text.finish(&prefix);
        self.write_text(&out);
    }

    fn text_prefix(&self) -> String {
        if self.answer_to_stdout && !self.text_is_think {
            if !self.stdout.tty {
                return String::new();
            }
            format!("{} ", self.stdout.paint("36", self.stdout.glyph("┃", "|")))
        } else {
            format!("{} ", self.bar)
        }
    }

    fn write_text(&self, out: &str) {
        if out.is_empty() {
            return;
        }
        if self.answer_to_stdout && !self.text_is_think {
            print!("{out}");
            let _ = std::io::stdout().flush();
        } else {
            let out = if self.text_is_think {
                style::dim(out)
            } else {
                out.to_string()
            };
            eprint!("{out}");
            let _ = std::io::stderr().flush();
        }
    }

    fn write_prefixed(&mut self, s: &str, think: bool) {
        if self.text_is_think != think {
            self.end_text_line();
            self.text = TextState::default();
            self.text_is_think = think;
        }
        let prefix = self.text_prefix();
        let out = self.text.push(s, &prefix);
        self.write_text(&out);
    }

    fn columns(&self) -> Option<usize> {
        self.stderr
            .tty
            .then(|| nosh_shell::term::stderr_columns().unwrap_or(100))
    }

    fn output_line(&self, line: &str, is_err: bool, columns: Option<usize>) -> String {
        // Leave the last column unused to avoid terminal auto-wrap.
        let max = columns.map_or(usize::MAX, |w| w.saturating_sub(1));
        let prefix = style::clip_line(&format!("{}   ", self.stderr.glyph("┃", "|")), max, 0, "");
        let offset = style::width(&prefix);
        let text = style::clip_line(
            line,
            max.saturating_sub(offset),
            offset,
            self.stderr.glyph("…", "..."),
        );
        format!(
            "{}{}",
            self.stderr.paint("36", &prefix),
            self.stderr.paint(if is_err { "31" } else { "2" }, &text,)
        )
    }

    fn out_line(&mut self, line: &str, is_err: bool) {
        if self.shown < LIVE_LINES {
            self.shown += 1;
            eprintln!("{}", self.output_line(line, is_err, self.columns()));
        } else {
            // Only the count and the last two lines are kept (cheap per line).
            self.hidden += 1;
            let mut keep = if self.tail.len() >= 2 {
                self.tail.remove(0)
            } else {
                (String::new(), is_err)
            };
            keep.0.clear();
            keep.0.push_str(line);
            keep.1 = is_err;
            self.tail.push(keep);
        }
    }
}

impl AgentUi for TermUi {
    fn pause(&mut self) {
        self.clear_status();
        self.end_text_line();
    }

    fn prefill(&mut self, done: usize, total: usize) {
        if !self.stderr.ansi || total < 200 || done >= total {
            return;
        }
        self.end_text_line();
        let pct = done * 100 / total.max(1);
        let status = format!(
            "{} {} {pct}%",
            self.stderr.glyph("┃", "|"),
            self.stderr.glyph("…", "...")
        );
        let status = style::clip_line(
            &status,
            self.columns().unwrap_or(100).saturating_sub(1),
            0,
            "",
        );
        eprint!("\r\x1b[K{}", style::dim(&status));
        let _ = std::io::stderr().flush();
        self.status_shown = true;
    }

    fn text(&mut self, s: &str) {
        self.clear_status();
        self.write_prefixed(s, false);
    }

    fn think(&mut self, s: &str) {
        if self.show_think {
            self.clear_status();
            self.write_prefixed(s, true);
        }
    }

    fn tool_start(&mut self, tool: &str, detail: &str, risk: Option<Risk>, label: &str) {
        self.clear_status();
        self.end_text_line();
        let risk_s = match risk {
            Some(Risk::Safe) => style::green(label),
            Some(Risk::Mutating) => style::yellow(label),
            Some(_) => style::red(label),
            None => style::dim(label),
        };
        eprintln!(
            "{} {} {}  {risk_s}",
            self.bar,
            style::cyan(style::glyph("⚙", "*")),
            style::bold(tool)
        );
        for (i, l) in detail.split('\n').enumerate() {
            let p = match (i, tool) {
                (0, "run_command") => "$ ",
                (_, "run_command") => "  ",
                _ => "",
            };
            eprintln!("{}   {p}{}", self.bar, style::visible(l));
        }
        self.shown = 0;
        self.hidden = 0;
        self.tail.clear();
        self.output = Default::default();
    }

    fn output(&mut self, chunk: &str, is_err: bool) {
        for line in self.output[usize::from(is_err)].push(chunk) {
            self.out_line(&line, is_err);
        }
    }

    fn tool_end(&mut self, summary: &str) {
        for is_err in [false, true] {
            if let Some(rest) = self.output[usize::from(is_err)].finish() {
                self.out_line(&rest, is_err);
            }
        }
        if self.hidden > 0 {
            let more = self.hidden.saturating_sub(self.tail.len());
            if more > 0 {
                eprintln!(
                    "{}   {}",
                    self.bar,
                    style::dim(&format!("{} {more} more lines", style::glyph("…", "...")))
                );
            }
            for (line, is_err) in std::mem::take(&mut self.tail) {
                eprintln!("{}", self.output_line(&line, is_err, self.columns()));
            }
        }
        if !summary.is_empty() {
            eprintln!(
                "{}   {}",
                self.bar,
                style::dim(&style::visible_text(summary))
            );
        }
        self.text = TextState::default();
    }

    fn notice(&mut self, msg: &str) {
        self.clear_status();
        self.end_text_line();
        eprintln!("{} {}", self.bar, style::dim(&style::visible_text(msg)));
    }

    fn error(&mut self, msg: &str) {
        self.clear_status();
        self.end_text_line();
        eprintln!("{} {}", self.bar, style::red(&style::visible_text(msg)));
    }

    fn proposed(&mut self, cmd: &str, explanation: Option<&str>) {
        self.clear_status();
        self.end_text_line();
        eprintln!(
            "{} {} {}",
            self.bar,
            style::cyan(style::glyph("↳", "->")),
            style::bold(&style::visible(cmd))
        );
        if let Some(e) = explanation.filter(|e| !e.trim().is_empty()) {
            eprintln!(
                "{} {}",
                self.bar,
                style::dim(&style::visible_text(e.trim()))
            );
        }
    }

    fn finish(&mut self, s: &TaskSummary) {
        self.clear_status();
        self.end_text_line();
        let mark = match s.status.as_str() {
            "completed" => style::green(style::glyph("✔", "+")),
            "cancelled" => style::yellow(style::glyph("✗", "x")),
            _ => style::yellow(style::glyph("⚠", "!")),
        };
        let sep = style::glyph(" · ", " | ");
        let mut line = format!("{} steps{sep}{:.1} s", s.steps, s.secs);
        if s.decode_tps > 0.0 {
            line.push_str(&format!("{sep}{:.1} tok/s", s.decode_tps));
        }
        if let Some(n) = &s.note {
            line = format!("{n} ({line})");
        }
        eprintln!(
            "{} {mark} {}",
            self.bar,
            style::dim(&style::visible_text(&line))
        );
        if stats_enabled() {
            eprintln!("{} {}", self.bar, style::dim(&s.stats_line()));
        }
        self.text = TextState::default();
    }
}

/// JSON Lines events on stdout (`nosh -a --json`).
#[derive(Default)]
pub struct JsonUi;

fn emit(v: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

impl AgentUi for JsonUi {
    fn text(&mut self, s: &str) {
        emit(json!({"ev": "text", "text": s}));
    }
    fn think(&mut self, s: &str) {
        emit(json!({"ev": "think", "text": s}));
    }
    fn tool_start(&mut self, tool: &str, detail: &str, risk: Option<Risk>, label: &str) {
        emit(json!({"ev": "tool_call", "name": tool, "detail": detail,
            "risk": risk.map(|r| r.label()), "decision": label}));
    }
    fn output(&mut self, chunk: &str, is_err: bool) {
        emit(
            json!({"ev": "output", "stream": if is_err { "stderr" } else { "stdout" }, "text": chunk}),
        );
    }
    fn tool_end(&mut self, summary: &str) {
        emit(json!({"ev": "tool_result", "summary": summary}));
    }
    fn notice(&mut self, msg: &str) {
        emit(json!({"ev": "notice", "text": msg}));
    }
    fn error(&mut self, msg: &str) {
        emit(json!({"ev": "error", "text": msg}));
    }
    fn proposed(&mut self, cmd: &str, explanation: Option<&str>) {
        emit(json!({"ev": "proposed", "command": cmd, "explanation": explanation}));
    }
    fn finish(&mut self, s: &TaskSummary) {
        emit(
            json!({"ev": "done", "status": s.status, "steps": s.steps, "secs": s.secs,
            "usage": {"prompt": s.prompt_tokens, "cached": s.cached_tokens,
                "completion": s.completion_tokens, "prefill_tok_s": s.prefill_tps,
                "tok_s": s.decode_tps, "ttft_s": s.ttft_secs,
                "context": s.context_used, "context_max": s.context_max}}),
        );
    }
}

/// Records everything (tests).
#[derive(Default, Debug)]
pub struct RecordUi {
    pub text: String,
    pub events: Vec<String>,
    pub output: String,
}

impl AgentUi for RecordUi {
    fn text(&mut self, s: &str) {
        self.text.push_str(s);
    }

    fn tool_start(&mut self, tool: &str, detail: &str, risk: Option<Risk>, label: &str) {
        self.events.push(format!(
            "tool {tool} [{}] {label}: {detail}",
            risk.map(|r| r.label()).unwrap_or("-")
        ));
    }
    fn output(&mut self, chunk: &str, _is_err: bool) {
        self.output.push_str(chunk);
    }
    fn tool_end(&mut self, summary: &str) {
        self.events.push(format!("end {summary}"));
    }
    fn notice(&mut self, msg: &str) {
        self.events.push(format!("notice {msg}"));
    }
    fn error(&mut self, msg: &str) {
        self.events.push(format!("error {msg}"));
    }
    fn proposed(&mut self, cmd: &str, _: Option<&str>) {
        self.events.push(format!("proposed {cmd}"));
    }
    fn finish(&mut self, s: &TaskSummary) {
        self.events.push(format!("finish {} {}", s.status, s.steps));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_text_preserves_unicode_and_normalizes_split_crlf() {
        let text = "\r\n\u{4f60}\u{597d}\r\n\u{1f469}\u{200d}\u{1f4bb} e\u{301}\r";
        for split in text.char_indices().map(|(i, _)| i).chain([text.len()]) {
            let mut state = TextState::default();
            let out = state.push(&text[..split], "| ")
                + &state.push(&text[split..], "| ")
                + &state.finish("| ");
            assert_eq!(
                out,
                "| \u{4f60}\u{597d}\n| \u{1f469}\u{200d}\u{1f4bb} e\u{301}\\r\n"
            );
        }
    }

    #[test]
    fn output_fits_even_narrow_terminals() {
        let ui = TermUi::new(false);
        for line in [
            "\u{4e2d}".repeat(50),
            "x\t".repeat(30),
            "\u{1f600}".repeat(40),
        ] {
            for columns in 1..100 {
                let shown = style::strip_ansi(&ui.output_line(&line, false, Some(columns)));
                assert!(style::width(&shown) < columns, "{columns}: {shown:?}");
            }
            let shown = style::strip_ansi(&ui.output_line(&line, false, None));
            assert!(
                !shown.ends_with("..."),
                "redirected text has no terminal-width cap"
            );
        }
    }

    #[test]
    fn partial_output_and_folded_tail_keep_their_streams() {
        let mut ui = TermUi::new(false);
        ui.shown = LIVE_LINES;
        ui.output("out", false);
        ui.output("err\n", true);
        ui.output("put\n", false);
        assert_eq!(ui.tail, [("err".into(), true), ("output".into(), false)]);
        ui.output("error without newline", true);
        assert!(ui.output[0].finish().is_none());
        assert_eq!(
            ui.output[1].finish().as_deref(),
            Some("error without newline")
        );
    }
}
