//! Advisory input feedback. The editor never waits for parsing or filesystem I/O.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use brush_parser::{ParseError, Parser, ParserOptions};
use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};

use crate::backend::SessionState;
use crate::trigger::{self, TriggerConfig};

const MAX_INPUT: usize = 4096;
const MAX_TOKENS: usize = 256;
const MAX_DEPTH: usize = 32;
const MAX_PATH_DIRS: usize = 32;
const QUERY_TIMEOUT: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Command,
    Word,
    String,
    Variable,
    Operator,
    Comment,
    Path,
    Pending,
    Error,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
    kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Pending(&'static str),
    Error(String),
    Unknown(&'static str),
    Querying,
    Unavailable(&'static str),
    Ready,
}

struct Request {
    text: String,
    generation: u64,
    session: Arc<SessionState>,
}

struct ResultState {
    text: String,
    generation: u64,
    spans: Vec<Span>,
    status: Status,
}

#[derive(Default)]
struct Work {
    latest: Option<Request>,
    result: Option<ResultState>,
    started: Option<Instant>,
    active: Option<(String, u64)>,
    stopped: bool,
}

struct Shared {
    work: Mutex<Work>,
    wake: Condvar,
}

/// One worker per editor, one replaceable request, one result. A blocked worker
/// remains counted; it is never replaced, joined on exit, or waited on by input.
pub(super) struct InputFeedback {
    shared: Arc<Shared>,
    session: Arc<SessionState>,
    generation: u64,
    prefix: String,
    ai_enabled: bool,
    colors: bool,
    cache: Mutex<Option<(String, Vec<Span>)>>,
    display: Mutex<Status>,
}

impl InputFeedback {
    pub(super) fn new(session: SessionState, cfg: &TriggerConfig, colors: bool) -> Self {
        let shared = Arc::new(Shared {
            work: Mutex::new(Work::default()),
            wake: Condvar::new(),
        });
        let worker = shared.clone();
        // Never create a replacement worker if parsing or metadata blocks.
        let spawned = std::thread::Builder::new()
            .name("nosh-input-feedback".into())
            .spawn(move || run_worker(worker));
        if spawned.is_err() {
            shared
                .work
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .stopped = true;
        }
        Self {
            shared,
            session: Arc::new(session),
            generation: 0,
            prefix: cfg.ai_prefix.clone(),
            ai_enabled: cfg.ai_enabled,
            colors,
            cache: Mutex::new(None),
            display: Mutex::new(Status::Ready),
        }
    }

    pub(super) fn refresh(&mut self, session: SessionState) {
        self.generation = self.generation.wrapping_add(1);
        self.session = Arc::new(session);
        if let Ok(mut work) = self.shared.work.try_lock() {
            work.result = None;
            work.latest = None;
        }
        if let Ok(mut cache) = self.cache.try_lock() {
            *cache = None;
        }
        if let Ok(mut display) = self.display.try_lock() {
            *display = Status::Ready;
        }
    }

    pub(super) fn message(&self) -> Option<String> {
        let status = self.display.try_lock().ok()?.clone();
        Some(match status {
            Status::Ready => return None,
            Status::Pending(reason) => format!("… {reason}"),
            Status::Error(reason) => format!("! {reason}"),
            Status::Unknown(reason) => format!("? {reason}"),
            Status::Querying => "… checking".into(),
            Status::Unavailable(reason) => format!("! feedback unavailable: {reason}"),
        })
    }

    fn feedback(&self, line: &str) -> (Vec<Span>, Status) {
        if !self.colors || line.is_empty() {
            return (Vec::new(), Status::Ready);
        }
        if line.len() > MAX_INPUT {
            return (
                Vec::new(),
                Status::Unavailable("input exceeds feedback budget"),
            );
        }
        if self.ai_enabled && !self.prefix.is_empty() && line.trim_start().starts_with(&self.prefix)
        {
            return (
                vec![Span {
                    start: 0,
                    end: line.len(),
                    kind: Kind::String,
                }],
                Status::Ready,
            );
        }
        if self.ai_enabled && trigger::apostrophe_prose(line.trim()) {
            return (Vec::new(), Status::Unknown("natural language"));
        }
        let spans = if let Ok(mut cache) = self.cache.try_lock() {
            if let Some((old, spans)) = cache.as_ref()
                && old == line
            {
                spans.clone()
            } else {
                let spans = lex(line);
                *cache = Some((line.to_owned(), spans.clone()));
                spans
            }
        } else {
            return (Vec::new(), Status::Unavailable("feedback busy"));
        };
        if spans.len() >= MAX_TOKENS {
            return (spans, Status::Unavailable("token budget exceeded"));
        }
        if nesting(line) > MAX_DEPTH {
            return (spans, Status::Unavailable("nesting budget exceeded"));
        }
        if spans.iter().any(|s| s.kind == Kind::Pending) {
            return (spans, Status::Pending("unfinished input"));
        }
        let Ok(mut work) = self.shared.work.try_lock() else {
            return (spans, Status::Unavailable("feedback busy"));
        };
        if work.stopped {
            return (spans, Status::Unavailable("feedback worker stopped"));
        }
        if let Some(start) = work.started
            && start.elapsed() >= QUERY_TIMEOUT
        {
            // A timed out syscall/thread is NOT cancelled. No new jobs until
            // it returns; old results can never color a newer input.
            work.stopped = true;
            work.latest = None;
            return (spans, Status::Unavailable("feedback query timed out"));
        }
        if let Some(result) = &work.result
            && result.generation == self.generation
            && result.text == line
        {
            let mut styled = spans;
            styled.extend(result.spans.iter().cloned());
            return (styled, result.status.clone());
        }
        if work
            .active
            .as_ref()
            .is_none_or(|(text, generation)| text != line || *generation != self.generation)
            && work
                .latest
                .as_ref()
                .is_none_or(|r| r.text != line || r.generation != self.generation)
        {
            work.latest = Some(Request {
                text: line.to_owned(),
                generation: self.generation,
                session: self.session.clone(),
            });
            self.shared.wake.notify_one();
        }
        (spans, Status::Querying)
    }
}

impl Drop for InputFeedback {
    fn drop(&mut self) {
        if let Ok(mut work) = self.shared.work.try_lock() {
            work.stopped = true;
            work.latest = None;
            self.shared.wake.notify_one();
        }
    }
}

impl Highlighter for InputFeedback {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let (spans, status) = self.feedback(line);
        if let Ok(mut display) = self.display.try_lock() {
            *display = status.clone();
        }
        let mut output = StyledText::new();
        output.push((Style::new(), line.to_owned()));
        for span in spans {
            if line.is_char_boundary(span.start) && line.is_char_boundary(span.end) {
                output.style_range(span.start, span.end, paint(span.kind));
            }
        }
        // Preserve exactly the input's bytes and cursor positions. The final
        // character carries the diagnostic state when there is no specific span.
        if !line.is_empty() && !matches!(status, Status::Ready) {
            let start = line.char_indices().next_back().map_or(0, |(i, _)| i);
            let style = match status {
                Status::Error(_) => paint(Kind::Error),
                Status::Pending(_) => paint(Kind::Pending),
                Status::Querying => Style::new().italic(),
                Status::Unknown(_) => paint(Kind::Unknown),
                Status::Unavailable(_) => Style::new().underline(),
                Status::Ready => Style::new(),
            };
            output.style_range(start, line.len(), style);
        }
        output
    }
}

fn paint(kind: Kind) -> Style {
    match kind {
        Kind::Command => Color::Blue.bold(),
        Kind::Word => Style::new(),
        Kind::String => Color::Green.normal(),
        Kind::Variable => Color::Cyan.normal(),
        Kind::Operator => Color::Purple.normal(),
        Kind::Comment => Color::DarkGray.normal(),
        Kind::Path => Color::Green.underline(),
        Kind::Pending => Color::Yellow.normal(),
        Kind::Error => Color::Red.underline(),
        Kind::Unknown => Color::Yellow.italic(),
    }
}

// A bounded lexical *display* pass, not a second shell parser. Byte offsets
// remain valid for Reedline; brush's character offsets are never used as bytes.
fn lex(line: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut at_command = true;
    let mut redir = false;
    let mut i = 0;
    let mut quote = None;
    let mut depth = 0usize;
    while i < line.len() && spans.len() < MAX_TOKENS {
        let c = line[i..].chars().next().unwrap();
        if c.is_whitespace() {
            if c == '\n' {
                at_command = true;
            }
            i += c.len_utf8();
            continue;
        }
        let start = i;
        if c == '#'
            && (i == 0
                || line[..i].chars().next_back().is_some_and(|previous| {
                    previous.is_whitespace() || ";|&(){}".contains(previous)
                }))
        {
            spans.push(Span {
                start,
                end: line.len(),
                kind: Kind::Comment,
            });
            break;
        }
        if "|&;<>(){}".contains(c) {
            i += c.len_utf8();
            if i < line.len() && line[i..].starts_with(c) && "|&<>".contains(c) {
                i += 1;
            }
            redir = c == '<' || c == '>';
            if "|&;({".contains(c) {
                at_command = true;
            }
            if c == '(' || c == '{' {
                depth += 1;
            } else if c == ')' || c == '}' {
                depth = depth.saturating_sub(1);
            }
            spans.push(Span {
                start,
                end: i,
                kind: Kind::Operator,
            });
            continue;
        }
        let mut string = false;
        let mut variable = false;
        while i < line.len() {
            let ch = line[i..].chars().next().unwrap();
            if let Some(q) = quote {
                if ch == '\\' && q == '"' {
                    i += ch.len_utf8();
                    if i < line.len() {
                        i += line[i..].chars().next().unwrap().len_utf8();
                    }
                    continue;
                }
                i += ch.len_utf8();
                if ch == q {
                    quote = None;
                }
            } else if ch == '\'' || ch == '"' || ch == '`' {
                string = true;
                quote = Some(ch);
                i += ch.len_utf8();
            } else if ch == '\\' {
                i += 1;
                if i < line.len() {
                    i += line[i..].chars().next().unwrap().len_utf8();
                }
            } else if ch.is_whitespace() || "|&;<>(){}".contains(ch) {
                break;
            } else {
                variable |= ch == '$';
                i += ch.len_utf8();
            }
        }
        let word = &line[start..i];
        let kind = if quote.is_some() || word.ends_with('\\') {
            Kind::Pending
        } else if at_command && (string || variable) {
            Kind::Unknown
        } else if string {
            Kind::String
        } else if variable {
            Kind::Variable
        } else if redir {
            Kind::Path
        } else if word.starts_with("./") || word.starts_with('/') {
            Kind::Word
        } else if at_command && !word.contains('=') {
            Kind::Command
        } else {
            Kind::Word
        };
        spans.push(Span {
            start,
            end: i,
            kind,
        });
        if redir {
            redir = false;
        } else if matches!(kind, Kind::Command | Kind::Unknown) {
            at_command = false;
        }
    }
    if depth > 0
        && depth <= MAX_DEPTH
        && !spans.iter().any(|s| s.kind == Kind::Pending)
        && let Some(last) = spans.last_mut()
    {
        last.kind = Kind::Pending;
    }
    if line.trim_end().ends_with(['|', '&', '<', '>'])
        && let Some(last) = spans.last_mut()
    {
        last.kind = Kind::Pending;
    }
    spans
}

fn nesting(line: &str) -> usize {
    let mut depth = 0usize;
    let mut maximum = 0;
    for ch in line.chars() {
        match ch {
            '(' | '{' => {
                depth += 1;
                maximum = maximum.max(depth);
            }
            ')' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    maximum
}

fn run_worker(shared: Arc<Shared>) {
    loop {
        let request = {
            let mut work = shared.work.lock().unwrap_or_else(|e| e.into_inner());
            while work.latest.is_none() && !work.stopped {
                work = shared.wake.wait(work).unwrap_or_else(|e| e.into_inner());
            }
            if work.stopped {
                return;
            }
            work.started = Some(Instant::now());
            let request = work.latest.take().unwrap();
            work.active = Some((request.text.clone(), request.generation));
            request
        };
        let result = std::panic::catch_unwind(|| analyze(&request));
        let mut work = shared.work.lock().unwrap_or_else(|e| e.into_inner());
        work.started = None;
        work.active = None;
        match result {
            Ok(result) => work.result = Some(result),
            Err(_) => {
                work.stopped = true;
                work.latest = None;
            }
        }
    }
}

fn analyze(request: &Request) -> ResultState {
    let text = &request.text;
    let mut spans = Vec::new();
    let mut status = if lex(text).iter().any(|s| s.kind == Kind::Unknown) {
        Status::Unknown("dynamic or quoted command name")
    } else {
        Status::Ready
    };
    let mut parser = Parser::new(text.as_bytes(), &ParserOptions::default());
    match parser.parse_program() {
        Err(e) if trigger::is_incomplete(&e) => status = Status::Pending("unfinished shell syntax"),
        Err(e) => {
            let index = match &e {
                ParseError::ParsingNear(pos) => pos.index,
                ParseError::Tokenizing {
                    position: Some(pos),
                    ..
                } => pos.index,
                _ => text.chars().count().saturating_sub(1),
            };
            let start = text
                .char_indices()
                .nth(index)
                .map_or(text.len(), |(i, _)| i);
            let end = text[start..]
                .chars()
                .next()
                .map_or(start, |c| start + c.len_utf8());
            spans.push(Span {
                start,
                end,
                kind: Kind::Error,
            });
            status = Status::Error(e.to_string());
        }
        Ok(_) => {}
    }
    if matches!(status, Status::Ready) {
        let words = lex(text);
        let dynamic = text.contains("$(")
            || text.contains('`')
            || text.contains("PATH=")
            || text.contains("function ")
            || text.contains("()")
            || text.contains("eval ");
        for word in words
            .iter()
            .filter(|w| matches!(w.kind, Kind::Command | Kind::Word | Kind::Path))
            .take(32)
        {
            let value = &text[word.start..word.end];
            if value.contains(['$', '*', '?', '[', '{', '\\', '\'', '"']) {
                continue;
            }
            if word.kind == Kind::Command {
                if request.session.aliases.contains_key(value)
                    || request.session.functions.contains(value)
                    || request.session.builtins.contains(value)
                    || matches!(
                        value,
                        "if" | "then"
                            | "else"
                            | "fi"
                            | "for"
                            | "do"
                            | "done"
                            | "while"
                            | "case"
                            | "esac"
                    )
                {
                    continue;
                }
                if dynamic {
                    status = Status::Unknown("dynamic command or session state");
                    continue;
                }
                match find_command(value, &request.session) {
                    Some(true) => {}
                    Some(false) => {
                        spans.push(Span {
                            start: word.start,
                            end: word.end,
                            kind: Kind::Error,
                        });
                        status = Status::Error(format!("command not found: {value}"));
                    }
                    None => status = Status::Unknown("PATH cannot be checked completely"),
                }
            } else if word.kind == Kind::Path || value.starts_with("./") || value.starts_with('/') {
                let path = if Path::new(value).is_absolute() {
                    PathBuf::from(value)
                } else {
                    request.session.cwd.join(value)
                };
                if path.metadata().is_ok() {
                    spans.push(Span {
                        start: word.start,
                        end: word.end,
                        kind: Kind::Path,
                    });
                } else if word.kind == Kind::Path
                    && text[..word.start].trim_end().ends_with('<')
                    && !text[..word.start].contains([';', '|', '&'])
                {
                    status = Status::Unknown("input path missing in current snapshot");
                }
            }
        }
    }
    ResultState {
        text: text.clone(),
        generation: request.generation,
        spans,
        status,
    }
}

fn find_command(name: &str, session: &SessionState) -> Option<bool> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| -> Option<bool> {
        match std::fs::metadata(p) {
            Ok(m) => Some(m.is_file() && m.permissions().mode() & 0o111 != 0),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
            Err(_) => None,
        }
    };
    if name.contains('/') {
        let path = if Path::new(name).is_absolute() {
            PathBuf::from(name)
        } else {
            session.cwd.join(name)
        };
        return executable(&path);
    }
    let path = session.var("PATH")?;
    let mut dirs = path.split(':');
    let mut uncertain = false;
    for dir in dirs.by_ref().take(MAX_PATH_DIRS) {
        let base = if dir.is_empty() {
            &session.cwd
        } else {
            Path::new(dir)
        };
        let base = if base.is_absolute() {
            base.to_path_buf()
        } else {
            session.cwd.join(base)
        };
        match executable(&base.join(name)) {
            Some(true) => return Some(true),
            None => uncertain = true,
            Some(false) => {}
        }
    }
    if uncertain || dirs.next().is_some() {
        None
    } else {
        Some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn without_worker() -> InputFeedback {
        InputFeedback {
            shared: Arc::new(Shared {
                work: Mutex::new(Work::default()),
                wake: Condvar::new(),
            }),
            session: Arc::new(SessionState::default()),
            generation: 0,
            prefix: "#".into(),
            ai_enabled: true,
            colors: true,
            cache: Mutex::new(None),
            display: Mutex::new(Status::Ready),
        }
    }

    #[test]
    fn lexical_feedback_is_byte_aligned_and_never_changes_input() {
        for line in [
            "echo '你👩‍💻",
            "echo \"$(printf x)\"",
            "a |",
            "echo e\u{301} > new.txt",
        ] {
            for span in lex(line) {
                assert!(line.is_char_boundary(span.start) && line.is_char_boundary(span.end));
            }
            let feedback =
                InputFeedback::new(SessionState::default(), &TriggerConfig::default(), true);
            let rendered = feedback.highlight(line, 0);
            assert_eq!(
                rendered
                    .buffer
                    .iter()
                    .map(|(_, s)| s.as_str())
                    .collect::<String>(),
                line
            );
        }
        assert!(lex("echo 'abc").iter().any(|s| s.kind == Kind::Pending));
        assert!(lex("echo x |").iter().any(|s| s.kind == Kind::Pending));
    }

    #[test]
    fn stale_results_and_over_budget_input_are_not_used() {
        let feedback = InputFeedback::new(SessionState::default(), &TriggerConfig::default(), true);
        assert!(matches!(
            feedback.feedback(&"x".repeat(MAX_INPUT + 1)).1,
            Status::Unavailable(_)
        ));
        assert!(matches!(feedback.feedback("# ask me").1, Status::Ready));
        assert!(matches!(
            feedback.feedback("what's next").1,
            Status::Unknown(_)
        ));
    }

    #[test]
    fn static_checks_preserve_uncertainty_and_output_targets() {
        let mut session = SessionState::default();
        session.cwd = std::env::temp_dir();
        session.vars.insert("PATH".into(), String::new());
        session.builtins.extend(["echo".into(), "cat".into()]);
        let check = |s: &str| {
            analyze(&Request {
                text: s.into(),
                generation: 0,
                session: Arc::new(session.clone()),
            })
        };
        assert!(matches!(
            check("echo hello > new.txt").status,
            Status::Ready
        ));
        assert!(matches!(
            check("not_a_real_command_xyz").status,
            Status::Error(_)
        ));
        assert!(matches!(
            check("PATH=/tmp unknown").status,
            Status::Unknown(_)
        ));
        assert!(matches!(
            check("echo \"$(printf x)\"").status,
            Status::Ready
        ));
        assert!(matches!(check("echo )").status, Status::Error(_)));
        assert!(matches!(
            check("cat < absent_input").status,
            Status::Unknown(_)
        ));
        assert!(matches!(
            check("touch input; cat < input").status,
            Status::Ready | Status::Error(_)
        ));
    }

    #[test]
    fn in_flight_queries_coalesce_and_timeout_without_replacement() {
        let feedback = without_worker();
        {
            let mut work = feedback.shared.work.lock().unwrap();
            work.active = Some(("stuck".into(), 0));
            work.started = Some(Instant::now());
        }
        feedback.feedback("first");
        feedback.feedback("second");
        feedback.feedback("second");
        {
            let mut work = feedback.shared.work.lock().unwrap();
            assert_eq!(work.latest.as_ref().unwrap().text, "second");
            work.started = Some(Instant::now() - QUERY_TIMEOUT - Duration::from_millis(1));
        }
        assert!(matches!(
            feedback.feedback("third").1,
            Status::Unavailable("feedback query timed out")
        ));
        let work = feedback.shared.work.lock().unwrap();
        assert!(work.stopped);
        assert!(work.latest.is_none());
    }

    #[test]
    fn session_generation_rejects_stale_results() {
        let mut feedback = without_worker();
        feedback.refresh(SessionState::default());
        {
            let mut work = feedback.shared.work.lock().unwrap();
            work.result = Some(ResultState {
                text: "echo x".into(),
                generation: 0,
                spans: vec![Span {
                    start: 0,
                    end: 4,
                    kind: Kind::Error,
                }],
                status: Status::Error("stale".into()),
            });
        }
        assert!(matches!(feedback.feedback("echo x").1, Status::Querying));
        assert!(matches!(
            feedback.feedback(&"(".repeat(MAX_DEPTH + 1)).1,
            Status::Unavailable("nesting budget exceeded")
        ));
    }
}
