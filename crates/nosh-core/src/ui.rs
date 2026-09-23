//! Rendering of agent activity: the `┃` block in the terminal, JSON Lines for
//! `nosh -a --json`, and a recorder for tests.

use std::io::{IsTerminal, Write};

use nosh_permissions::Risk;
use nosh_shell::{OutputSink, style};
use serde_json::{Value, json};

pub fn bar() -> String {
    style::cyan("┃")
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
        let rss = nosh_llm::rss_mb()
            .map(|(cur, peak)| format!(" · rss {cur:.0}/{peak:.0} MB"))
            .unwrap_or_default();
        format!(
            "stats: prompt {} (+{} cached) tok @ {:.0} tok/s · gen {} tok @ {:.1} tok/s · ttft {:.2}s · ctx {}/{}{rss}",
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
/// Longest partial output line kept for display.
const MAX_PARTIAL: usize = 4096;

/// Largest char boundary of `s` at or below `n`.
fn floor_boundary(s: &str, n: usize) -> usize {
    let mut i = n.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Terminal renderer (stderr; the answer can go to stdout for `nosh -a`).
pub struct TermUi {
    bar: String,
    answer_to_stdout: bool,
    stdout_tty: bool,
    stderr_tty: bool,
    at_line_start: bool,
    status_shown: bool,
    text_started: bool,
    line_buf: String,
    shown: usize,
    hidden: usize,
    tail: Vec<String>,
    width: usize,
    pub show_think: bool,
}

impl TermUi {
    pub fn new(answer_to_stdout: bool) -> Self {
        let width = crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(100);
        Self {
            bar: bar(),
            answer_to_stdout,
            stdout_tty: std::io::stdout().is_terminal(),
            stderr_tty: std::io::stderr().is_terminal(),
            at_line_start: true,
            status_shown: false,
            text_started: false,
            line_buf: String::new(),
            shown: 0,
            hidden: 0,
            tail: Vec::new(),
            width: width.max(40),
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
        if !self.at_line_start {
            if self.answer_to_stdout {
                println!();
            } else {
                eprintln!();
            }
            self.at_line_start = true;
        }
    }

    fn write_prefixed(&mut self, s: &str, dim: bool) {
        let to_stdout = self.answer_to_stdout && !dim;
        let with_bar = !to_stdout || self.stdout_tty;
        let mut out = String::new();
        for ch in s.chars() {
            if self.at_line_start {
                if ch == '\n' && !self.text_started {
                    continue;
                }
                if with_bar {
                    out.push_str(&self.bar);
                    out.push(' ');
                }
                self.at_line_start = false;
            }
            self.text_started = true;
            out.push(ch);
            if ch == '\n' {
                self.at_line_start = true;
            }
        }
        let out = if dim { style::dim(&out) } else { out };
        if to_stdout {
            print!("{out}");
            let _ = std::io::stdout().flush();
        } else {
            eprint!("{out}");
            let _ = std::io::stderr().flush();
        }
    }

    fn clip(&self, line: &str) -> String {
        let line = style::safe_output_line(line);
        let max = self.width.saturating_sub(6);
        if line.chars().count() > max {
            line.chars()
                .take(max.saturating_sub(1))
                .chain("…".chars())
                .collect()
        } else {
            line
        }
    }

    fn out_line(&mut self, line: &str, is_err: bool) {
        if self.shown < LIVE_LINES {
            self.shown += 1;
            let shown = self.clip(line);
            let s = if is_err {
                style::red(&shown)
            } else {
                style::dim(&shown)
            };
            eprintln!("{}   {s}", self.bar);
        } else {
            // Only the count and the last two lines are kept (cheap per line).
            self.hidden += 1;
            let mut keep = if self.tail.len() >= 2 {
                self.tail.remove(0)
            } else {
                String::new()
            };
            keep.clear();
            keep.push_str(&line[..floor_boundary(line, 1024)]);
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
        if !self.stderr_tty || total < 200 || done >= total {
            return;
        }
        self.end_text_line();
        let pct = done * 100 / total.max(1);
        eprint!("\r\x1b[K{} {}", self.bar, style::dim(&format!("… {pct}%")));
        let _ = std::io::stderr().flush();
        self.status_shown = true;
    }

    fn text(&mut self, s: &str) {
        self.clear_status();
        self.write_prefixed(&style::visible(s), false);
    }

    fn think(&mut self, s: &str) {
        if self.show_think {
            self.clear_status();
            self.write_prefixed(&style::visible(s), true);
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
            style::cyan("⚙"),
            style::bold(tool)
        );
        for (i, l) in detail.lines().enumerate() {
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
        self.line_buf.clear();
    }

    fn output(&mut self, chunk: &str, is_err: bool) {
        let mut rest = chunk;
        while let Some(i) = rest.find('\n') {
            let line = &rest[..i];
            rest = &rest[i + 1..];
            if self.line_buf.is_empty() {
                self.out_line(line, is_err);
            } else {
                let mut full = std::mem::take(&mut self.line_buf);
                full.push_str(
                    &line[..floor_boundary(line, MAX_PARTIAL.saturating_sub(full.len()))],
                );
                self.out_line(&full, is_err);
            }
        }
        // A line without a newline is kept only up to a bound.
        let room = MAX_PARTIAL.saturating_sub(self.line_buf.len());
        self.line_buf.push_str(&rest[..floor_boundary(rest, room)]);
    }

    fn tool_end(&mut self, summary: &str) {
        if !self.line_buf.is_empty() {
            let rest = std::mem::take(&mut self.line_buf);
            self.out_line(&rest, false);
        }
        if self.hidden > 0 {
            let more = self.hidden.saturating_sub(self.tail.len());
            if more > 0 {
                eprintln!(
                    "{}   {}",
                    self.bar,
                    style::dim(&format!("… {more} more lines"))
                );
            }
            for l in std::mem::take(&mut self.tail) {
                eprintln!("{}   {}", self.bar, style::dim(&self.clip(&l)));
            }
        }
        if !summary.is_empty() {
            eprintln!("{}   {}", self.bar, style::dim(summary));
        }
        self.text_started = false;
    }

    fn notice(&mut self, msg: &str) {
        self.clear_status();
        self.end_text_line();
        eprintln!("{} {}", self.bar, style::dim(msg));
    }

    fn error(&mut self, msg: &str) {
        self.clear_status();
        self.end_text_line();
        eprintln!("{} {}", self.bar, style::red(msg));
    }

    fn proposed(&mut self, cmd: &str, explanation: Option<&str>) {
        self.clear_status();
        self.end_text_line();
        eprintln!(
            "{} {} {}",
            self.bar,
            style::cyan("↳"),
            style::bold(&style::visible(cmd))
        );
        if let Some(e) = explanation.filter(|e| !e.trim().is_empty()) {
            eprintln!("{}   {}", self.bar, style::dim(&style::visible(e.trim())));
        }
    }

    fn finish(&mut self, s: &TaskSummary) {
        self.clear_status();
        self.end_text_line();
        let mark = match s.status.as_str() {
            "completed" => style::green("✔"),
            "cancelled" => style::yellow("✗"),
            _ => style::yellow("⚠"),
        };
        let mut line = format!("{} steps · {:.1} s", s.steps, s.secs);
        if s.decode_tps > 0.0 {
            line.push_str(&format!(" · {:.1} tok/s", s.decode_tps));
        }
        if let Some(n) = &s.note {
            line = format!("{n} ({line})");
        }
        eprintln!("{} {mark} {}", self.bar, style::dim(&line));
        if stats_enabled() {
            eprintln!("{} {}", self.bar, style::dim(&s.stats_line()));
        }
        self.text_started = false;
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
