use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use nosh_hub::tr;
use nu_ansi_term::{Color, Style};
use reedline::{
    EditContext, EditMode, EventStatus, Highlighter, Hinter, History, PromptContext,
    PromptInteraction, ReedlineEvent, ReedlineRawEvent, StyledText,
};

use super::worker::{ChildHandle, Kind, Worker, kill_child};
use super::*;
use crate::style;

const MISSING_COMMAND_IDLE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub(crate) struct Feedback {
    pub text: String,
    pub compact: String,
    pub state: State,
    pub correction: Option<Correction>,
}

#[derive(Default)]
struct FeedbackSnapshot {
    input: Option<Arc<Input>>,
    generation: u64,
    session: u64,
    value: Option<Feedback>,
}

#[derive(Default)]
struct Mailbox {
    context: Option<Arc<Context>>,
    latest: Option<Arc<Input>>,
}

#[derive(Clone, Default)]
struct Publication {
    analysis: Option<Arc<Analysis>>,
    lookup: Option<(Version, Arc<Vec<Observation>>, LookupStats)>,
    syntax_error: Option<String>,
    lookup_error: Option<String>,
    serial: u64,
    correction: Option<Correction>,
}

struct CorrectionSnapshot {
    input: Arc<Input>,
    proposal: Correction,
    epoch: u64,
}

impl CorrectionSnapshot {
    fn current(&self, shared: &Shared) -> bool {
        shared.current(self.input.version)
            && shared.correction_enabled.load(Ordering::Acquire)
            && self.epoch == shared.edit_epoch.load(Ordering::Acquire)
            && self.proposal.matches(&self.input)
    }
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    publication: Mutex<Publication>,
    display: Mutex<String>,
    feedback: Mutex<FeedbackSnapshot>,
    correction: Mutex<Option<CorrectionSnapshot>>,
    correction_enabled: AtomicBool,
    edit_epoch: AtomicU64,
    started: Instant,
    delayed_session: AtomicU64,
    delayed_input: AtomicU64,
    delayed_due_ms: AtomicU64,
    session: AtomicU64,
    generation: AtomicU64,
    stopped: AtomicBool,
    context_ok: AtomicBool,
    show_status: AtomicBool,
    children: Mutex<[Option<ChildHandle>; 2]>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    columns: AtomicUsize,
}

struct Lifetime(Arc<Shared>);

struct SupervisorStartup {
    launcher: WorkerCommand,
    index: SharedIndex,
    attempted: AtomicBool,
}

impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.stopped.store(true, Ordering::Release);
        // A CLI can exit before its supervisor gets another turn. Signal owned
        // children here too, without waiting or racing reaping/PID reuse.
        let children = self
            .0
            .children
            .try_lock()
            .ok()
            .map(|children| children.clone());
        if let Some(children) = children {
            for child in children.into_iter().flatten() {
                if let Err(error) = kill_child(&child) {
                    eprintln!("nosh input worker cleanup: {error}");
                }
            }
        }
        self.0.wake.notify_one();
    }
}

#[derive(Clone)]
pub struct InputAssist {
    shared: Arc<Shared>,
    _lifetime: Arc<Lifetime>,
    startup: Option<Arc<SupervisorStartup>>,
}

impl InputAssist {
    pub(crate) fn new(
        launcher: Option<WorkerCommand>,
        index: SharedIndex,
        repaint: Arc<dyn Fn() + Send + Sync>,
        columns: usize,
    ) -> Self {
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox::default()),
            wake: Condvar::new(),
            publication: Mutex::new(Publication::default()),
            display: Mutex::new(String::new()),
            feedback: Mutex::new(FeedbackSnapshot::default()),
            correction: Mutex::new(None),
            correction_enabled: AtomicBool::new(false),
            edit_epoch: AtomicU64::new(0),
            started: Instant::now(),
            delayed_session: AtomicU64::new(0),
            delayed_input: AtomicU64::new(0),
            delayed_due_ms: AtomicU64::new(0),
            session: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            context_ok: AtomicBool::new(false),
            show_status: AtomicBool::new(true),
            children: Mutex::new([None, None]),
            repaint,
            columns: AtomicUsize::new(columns),
        });
        let this = Self {
            _lifetime: Arc::new(Lifetime(shared.clone())),
            shared,
            startup: launcher.map(|launcher| {
                Arc::new(SupervisorStartup {
                    launcher,
                    index,
                    attempted: AtomicBool::new(false),
                })
            }),
        };
        if this.startup.is_none() {
            this.shared
                .publish(|p| p.syntax_error = Some("no input worker entry point".into()));
        }
        this
    }

    fn start_supervisor(&self) {
        let Some(startup) = &self.startup else {
            return;
        };
        if startup.attempted.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = self.shared.clone();
        let launcher = startup.launcher.clone();
        let index = startup.index.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("nosh-input-assist".into())
            .spawn(move || supervise(shared, launcher, index))
        {
            self.shared
                .publish(|p| p.syntax_error = Some(short_error(error)));
        }
    }

    pub(crate) fn prepare(&self, context: Result<Context, String>) {
        self.shared.show_status.store(true, Ordering::Release);
        self.shared.session.fetch_add(1, Ordering::AcqRel);
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.shared.context_ok.store(false, Ordering::Release);
        let installed = match self.shared.mailbox.try_lock() {
            Ok(mut mailbox) => match context {
                Ok(context) => {
                    mailbox.context = Some(Arc::new(context));
                    mailbox.latest = None;
                    self.set_display("");
                    self.shared.context_ok.store(true, Ordering::Release);
                    true
                }
                Err(error) => {
                    mailbox.context = None;
                    mailbox.latest = None;
                    self.set_display(&format!(
                        "{}: {}",
                        tr!("输入分析暂不可用", "Input analysis unavailable"),
                        style::visible_text(&error)
                    ));
                    false
                }
            },
            Err(_) => {
                self.set_display(tr!(
                    "输入分析暂不可用：会话快照忙",
                    "Input analysis unavailable: session snapshot busy"
                ));
                false
            }
        };
        if installed {
            self.start_supervisor();
        }
        self.shared.wake.notify_one();
    }

    pub(crate) fn suspend(&self) {
        self.shared.context_ok.store(false, Ordering::Release);
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut mailbox) = self.shared.mailbox.try_lock() {
            mailbox.latest = None;
        }
        self.shared.wake.notify_one();
    }

    pub(crate) fn highlighter(&self) -> Box<dyn Highlighter> {
        Box::new(InputHighlighter {
            assist: self.clone(),
            cache: RefCell::new(RenderCache::default()),
        })
    }

    pub(crate) fn hinter(&self, inner: reedline::DefaultHinter) -> Box<dyn Hinter> {
        Box::new(CorrectionHinter {
            assist: self.clone(),
            inner,
            hidden: false,
        })
    }

    pub(crate) fn set_correction_enabled(&self, enabled: bool) {
        if self
            .shared
            .correction_enabled
            .swap(enabled, Ordering::AcqRel)
            != enabled
        {
            self.shared.edit_epoch.fetch_add(1, Ordering::AcqRel);
            self.shared.wake.notify_one();
        }
    }

    pub(crate) fn present_correction(&self, context: &PromptContext<'_>, presented: bool) -> bool {
        let ready = if presented
            && context.cursor == context.buffer.len()
            && context.selection.is_none()
            && matches!(context.interaction, PromptInteraction::Editing)
        {
            self.shared.feedback.try_lock().ok().and_then(|snapshot| {
                let input = snapshot.input.as_ref()?;
                let proposal = snapshot.value.as_ref()?.correction.as_ref()?;
                (input.text == context.buffer
                    && proposal.matches(input)
                    && self.shared.current(input.version))
                .then(|| CorrectionSnapshot {
                    input: input.clone(),
                    proposal: proposal.clone(),
                    epoch: self.shared.edit_epoch.load(Ordering::Acquire),
                })
            })
        } else {
            None
        };
        if let Ok(mut view) = self.shared.correction.try_lock() {
            *view = ready;
            view.is_some()
        } else {
            self.shared.edit_epoch.fetch_add(1, Ordering::AcqRel);
            false
        }
    }

    fn correction_matches(&self, line: &str, cursor: usize) -> bool {
        self.shared.correction.try_lock().ok().is_some_and(|view| {
            view.as_ref().is_some_and(|snapshot| {
                snapshot.current(&self.shared)
                    && snapshot.input.text == line
                    && cursor == line.len()
            })
        })
    }

    fn delay_repaint_until(&self, version: Version, due: Instant) {
        let due_ms = due
            .saturating_duration_since(self.shared.started)
            .as_millis()
            .min(u128::from(u64::MAX - 1)) as u64
            + 1;
        let current_due = self.shared.delayed_due_ms.load(Ordering::Acquire);
        let current = Version {
            session: self.shared.delayed_session.load(Ordering::Acquire),
            input: self.shared.delayed_input.load(Ordering::Acquire),
        };
        if current_due == 0 || current != version || due_ms < current_due {
            self.shared
                .delayed_session
                .store(version.session, Ordering::Release);
            self.shared
                .delayed_input
                .store(version.input, Ordering::Release);
            self.shared.delayed_due_ms.store(due_ms, Ordering::Release);
            self.shared.wake.notify_one();
        }
    }

    pub(crate) fn after_completion(&self, shell: &Mutex<crate::backend::BrushShell>) {
        let context = self
            .shared
            .mailbox
            .try_lock()
            .ok()
            .and_then(|mailbox| mailbox.context.clone());
        if let Some(context) = context {
            let trigger = crate::TriggerConfig {
                ai_enabled: context.ai_enabled,
                ai_prefix: context.ai_prefix.clone(),
                builtin_name: context.ai_builtin.clone(),
                trigger_on_error: context.trigger_on_error,
                ..crate::TriggerConfig::default()
            };
            self.prepare(crate::EmbeddedShell::input_context_from_shared(
                shell,
                &trigger,
                &context.abbreviations,
            ));
        } else {
            self.prepare(Err("session snapshot unavailable after completion".into()));
        }
    }

    pub(crate) fn edit_mode(&self, inner: Box<dyn EditMode>) -> Box<dyn EditMode> {
        Box::new(InputEditMode {
            inner,
            shared: self.shared.clone(),
        })
    }

    pub(crate) fn status(&self) -> String {
        if !self.shared.show_status.load(Ordering::Acquire) {
            return String::new();
        }
        let text = self
            .shared
            .display
            .try_lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| {
                tr!(
                    "输入分析暂不可用：状态忙",
                    "Input analysis unavailable: status busy"
                )
                .into()
            });
        style::clip_line(
            &text,
            self.shared
                .columns
                .load(Ordering::Relaxed)
                .saturating_sub(1),
            0,
            "...",
        )
    }

    pub(crate) fn feedback(&self, line: &str) -> Option<Feedback> {
        if !self.shared.show_status.load(Ordering::Acquire) {
            return None;
        }
        let Ok(snapshot) = self.shared.feedback.try_lock() else {
            return Some(Feedback {
                text: tr!(
                    "诊断暂不可用：状态忙",
                    "Diagnostics unavailable: status busy"
                )
                .into(),
                compact: tr!("诊断状态忙", "Diagnostic status busy").into(),
                state: State::Unavailable,
                correction: None,
            });
        };
        if snapshot.session != self.shared.session.load(Ordering::Acquire)
            || snapshot.generation != self.shared.generation.load(Ordering::Acquire)
            || snapshot
                .input
                .as_ref()
                .is_some_and(|input| input.text != line)
        {
            return None;
        }
        snapshot.value.clone()
    }

    fn set_feedback(&self, input: Option<Arc<Input>>, value: Option<Feedback>) {
        if let Ok(mut snapshot) = self.shared.feedback.try_lock() {
            *snapshot = FeedbackSnapshot {
                input,
                generation: self.shared.generation.load(Ordering::Acquire),
                session: self.shared.session.load(Ordering::Acquire),
                value,
            };
        }
    }

    fn set_display(&self, text: &str) {
        self.set_display_state(text, State::Unavailable);
    }

    fn set_display_state(&self, text: &str, state: State) {
        self.set_feedback(
            None,
            (!text.is_empty()).then(|| Feedback {
                text: crate::status::plain(text),
                compact: style::clip_line(&crate::status::plain(text), 28, 0, "..."),
                state,
                correction: None,
            }),
        );
        self.set_legacy_display(text);
    }

    fn set_legacy_display(&self, text: &str) {
        if let Ok(mut status) = self.shared.display.try_lock() {
            if status.as_str() == text {
                return;
            }
            let text = if text.contains(['\n', '\t']) {
                Cow::Owned(text.replace('\n', "\\n").replace('\t', "\\t"))
            } else {
                Cow::Borrowed(text)
            };
            *status = text.chars().take(320).collect();
        }
    }

    fn request(&self, line: &str) -> Option<Arc<Input>> {
        let mut mailbox = self.shared.mailbox.try_lock().ok()?;
        if !self.shared.context_ok.load(Ordering::Acquire) {
            return None;
        }
        let context = mailbox.context.clone()?;
        let version = Version {
            input: self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1,
            session: self.shared.session.load(Ordering::Acquire),
        };
        let input = Arc::new(Input {
            version,
            text: line.to_owned(),
            context,
            command_input: false,
        });
        mailbox.latest = Some(input.clone());
        drop(mailbox);
        self.shared.wake.notify_one();
        Some(input)
    }
}

impl Shared {
    fn publish(&self, change: impl FnOnce(&mut Publication)) {
        if let Ok(mut publication) = self.publication.try_lock() {
            change(&mut publication);
            publication.serial += 1;
        }
        (self.repaint)();
    }

    fn flush(&self, publication: &Publication) -> bool {
        let Ok(mut destination) = self.publication.try_lock() else {
            return false;
        };
        let old = std::mem::replace(&mut *destination, publication.clone());
        drop(destination);
        drop(old);
        (self.repaint)();
        true
    }

    fn current(&self, version: Version) -> bool {
        self.context_ok.load(Ordering::Acquire)
            && version.session == self.session.load(Ordering::Acquire)
            && version.input == self.generation.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct RenderCache {
    input: Option<Arc<Input>>,
    serial: u64,
    spans: Vec<Span>,
    findings: Vec<Finding>,
    requested: Option<Instant>,
    correction: Option<Correction>,
}

struct InputHighlighter {
    assist: InputAssist,
    cache: RefCell<RenderCache>,
}

impl Highlighter for InputHighlighter {
    fn highlight(&self, line: &str, cursor: usize) -> StyledText {
        let mut cache = self.cache.borrow_mut();
        if line.len() > MAX_INPUT {
            self.assist.shared.generation.fetch_add(1, Ordering::AcqRel);
            cache.input = None;
            self.assist.set_display(tr!(
                "输入诊断已暂停：超出分析大小上限，原文仍可编辑",
                "Input diagnostics paused: size limit; the full text remains editable"
            ));
            return plain(line);
        }
        if !self.assist.shared.context_ok.load(Ordering::Acquire) {
            return plain(line);
        }
        self.assist
            .shared
            .show_status
            .store(true, Ordering::Release);
        let session = self.assist.shared.session.load(Ordering::Acquire);
        if cache.input.as_ref().is_none_or(|input| {
            input.text != line
                || input.version.session != session
                || input.version.input != self.assist.shared.generation.load(Ordering::Acquire)
        }) {
            cache.input = self.assist.request(line);
            cache.spans.clear();
            cache.findings.clear();
            cache.correction = None;
            cache.serial = u64::MAX;
            cache.requested = Some(Instant::now());
            if cache.input.is_none() {
                self.assist.set_display_state(
                    tr!(
                        "输入分析查询中：队列忙",
                        "Input analysis pending: mailbox busy"
                    ),
                    State::Pending,
                );
                (self.assist.shared.repaint)();
                return plain(line);
            }
        }
        let Some(input) = cache.input.clone() else {
            return plain(line);
        };
        let publication = self
            .assist
            .shared
            .publication
            .try_lock()
            .ok()
            .map(|p| p.clone());
        if publication.is_none() {
            cache.findings = vec![unavailable("diagnostic publication busy")];
            cache.correction = None;
            cache.serial = u64::MAX;
        }
        if let Some(publication) = publication
            && cache.serial != publication.serial
        {
            cache.serial = publication.serial;
            cache.findings.clear();
            cache.correction = publication
                .correction
                .filter(|proposal| proposal.matches(&input));
            let analysis = publication
                .analysis
                .as_ref()
                .filter(|a| a.version == input.version);
            cache.spans = vec![Span {
                range: 0..line.len(),
                role: Role::Text,
            }];
            if let Some(analysis) = analysis {
                cache.spans.clone_from(&analysis.spans);
                cache.findings.clone_from(&analysis.findings);
                if !analysis.queries.is_empty() {
                    if let Some(error) = &publication.lookup_error {
                        cache.findings.push(unavailable(error));
                    } else if let Some((_, observations, _)) = publication
                        .lookup
                        .as_ref()
                        .filter(|(version, _, _)| *version == input.version)
                    {
                        if observations.iter().any(|o| o.role == Some(Role::Ai)) {
                            cache.spans = vec![Span {
                                range: 0..line.len(),
                                role: Role::Ai,
                            }];
                            cache.findings = vec![Finding {
                                range: 0..line.len(),
                                state: State::Known,
                                reason: Reason::Ai,
                            }];
                        } else if analysis.ai_candidate
                            && observations.iter().any(|o| o.role == Some(Role::External))
                        {
                            if let Some(error) = &publication.syntax_error {
                                cache.findings.push(unavailable(error));
                            } else {
                                cache.findings.push(pending());
                            }
                        } else {
                            for observation in observations.iter() {
                                if let Some(role) = observation.role {
                                    cache.spans = overlay(&cache.spans, &observation.range, role);
                                }
                                if let Some(finding) = &observation.finding {
                                    cache.findings.push(finding.clone());
                                }
                            }
                        }
                    } else {
                        cache.findings.push(pending());
                    }
                }
            } else if let Some(error) = &publication.syntax_error {
                cache.findings.push(unavailable(error));
            } else if !line.is_empty() {
                cache.findings.push(pending());
            }
        }
        // A blocked process launch must not make the UI wait for the supervisor.
        // This also exposes the cause on the next edit if startup never returns.
        if cache.findings.iter().any(|f| f.state == State::Pending)
            && cache
                .requested
                .is_some_and(|t| t.elapsed() > INDEX_TIMEOUT * 3)
        {
            cache.findings = vec![unavailable("diagnostic worker is not responding")];
            cache.correction = None;
        }
        let missing_commands: Vec<&Finding> = cache
            .findings
            .iter()
            .filter(|finding| matches!(finding.reason, Reason::MissingCommand))
            .collect();
        let reveal_missing_commands = !missing_commands.is_empty()
            && cache
                .requested
                .is_some_and(|requested| requested.elapsed() >= MISSING_COMMAND_IDLE);
        if !missing_commands.is_empty()
            && !reveal_missing_commands
            && let Some(input) = &cache.input
            && let Some(requested) = cache.requested
        {
            self.assist
                .delay_repaint_until(input.version, requested + MISSING_COMMAND_IDLE);
        }
        self.assist
            .set_legacy_display(&status_text(&cache.findings, cursor));
        self.assist.set_feedback(
            cache.input.clone(),
            primary_finding(&cache.findings, cursor, reveal_missing_commands)
                .map(|finding| finding_feedback(finding, &input, cache.correction.as_ref())),
        );
        if cache.spans.is_empty() {
            return plain(line);
        }
        let mut styled = StyledText::new();
        for span in &cache.spans {
            if let Some(text) = line.get(span.range.clone()) {
                let style = if span.role == Role::Error
                    && missing_commands
                        .iter()
                        .any(|finding| contains_span(finding, span))
                {
                    if reveal_missing_commands {
                        editing_missing_command_style()
                    } else {
                        role_style(Role::Command)
                    }
                } else {
                    role_style(span.role)
                };
                styled.push((style, text.to_owned()));
            }
        }
        styled
    }
}

struct InputEditMode {
    inner: Box<dyn EditMode>,
    shared: Arc<Shared>,
}

impl EditMode for InputEditMode {
    fn parse_event(&mut self, raw: ReedlineRawEvent) -> ReedlineEvent {
        self.parse_event_with_context(raw, EditContext::Editing)
    }

    fn parse_event_with_context(
        &mut self,
        raw: ReedlineRawEvent,
        context: EditContext,
    ) -> ReedlineEvent {
        let raw: crossterm::event::Event = raw.into();
        let right = matches!(&raw, crossterm::event::Event::Key(key)
            if key.code == crossterm::event::KeyCode::Right && key.modifiers.is_empty());
        let event = self.inner.parse_event_with_context(
            ReedlineRawEvent::try_from(raw).expect("normalized raw event"),
            context,
        );
        let correction = if right && right_navigation(&event) {
            self.shared.correction.try_lock().ok().and_then(|mut view| {
                view.take()
                    .filter(|snapshot| snapshot.current(&self.shared))
                    .map(|snapshot| snapshot.proposal.edit())
            })
        } else {
            None
        };
        // Reedline parses a whole key batch before running it. Any earlier key
        // invalidates the painted adoption snapshot, even without a repaint.
        if !matches!(event, ReedlineEvent::Repaint) {
            self.shared.edit_epoch.fetch_add(1, Ordering::AcqRel);
        }
        if let ReedlineEvent::Resize(columns, _) = &event {
            self.shared
                .columns
                .store(usize::from(*columns), Ordering::Relaxed);
        }
        fn starts_search(event: &ReedlineEvent) -> bool {
            match event {
                ReedlineEvent::SearchHistory => true,
                ReedlineEvent::Multiple(events) | ReedlineEvent::UntilFound(events) => {
                    events.iter().any(starts_search)
                }
                _ => false,
            }
        }
        if starts_search(&event) {
            self.shared.show_status.store(false, Ordering::Release);
            self.shared.generation.fetch_add(1, Ordering::AcqRel);
        }
        correction.unwrap_or(event)
    }

    fn edit_mode(&self) -> reedline::PromptEditMode {
        self.inner.edit_mode()
    }

    fn has_pending_input(&self) -> bool {
        self.inner.has_pending_input()
    }

    fn handle_mode_specific_event(&mut self, event: ReedlineEvent) -> EventStatus {
        self.inner.handle_mode_specific_event(event)
    }

    fn after_event(&mut self, context: EditContext, edited: bool) {
        self.inner.after_event(context, edited);
    }
}

struct CorrectionHinter {
    assist: InputAssist,
    inner: reedline::DefaultHinter,
    hidden: bool,
}

impl Hinter for CorrectionHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        history: &dyn History,
        color: bool,
        cwd: &str,
    ) -> String {
        self.hidden = self.assist.correction_matches(line, pos);
        if self.hidden {
            String::new()
        } else {
            self.inner.handle(line, pos, history, color, cwd)
        }
    }

    fn complete_hint(&self) -> String {
        if self.hidden {
            String::new()
        } else {
            self.inner.complete_hint()
        }
    }

    fn next_hint_token(&self) -> String {
        if self.hidden {
            String::new()
        } else {
            self.inner.next_hint_token()
        }
    }
}

fn plain(line: &str) -> StyledText {
    let mut text = StyledText::new();
    text.push((Style::new(), line.to_owned()));
    text
}

fn overlay(spans: &[Span], range: &std::ops::Range<usize>, role: Role) -> Vec<Span> {
    let mut result = Vec::with_capacity(spans.len() + 2);
    for span in spans {
        let start = span.range.start.max(range.start);
        let end = span.range.end.min(range.end);
        let replace = matches!(role, Role::Error | Role::Path)
            || matches!(span.role, Role::Text | Role::Command);
        if start >= end || !replace || (span.role == Role::Error && role != Role::Error) {
            result.push(span.clone());
            continue;
        }
        if span.range.start < start {
            result.push(Span {
                range: span.range.start..start,
                role: span.role,
            });
        }
        result.push(Span {
            range: start..end,
            role,
        });
        if end < span.range.end {
            result.push(Span {
                range: end..span.range.end,
                role: span.role,
            });
        }
    }
    result
}

fn unavailable(error: &str) -> Finding {
    Finding {
        range: 0..0,
        state: State::Unavailable,
        reason: Reason::Worker(error.to_owned()),
    }
}

fn pending() -> Finding {
    Finding {
        range: 0..0,
        state: State::Pending,
        reason: Reason::Pending,
    }
}

fn status_text(findings: &[Finding], cursor: usize) -> String {
    primary_finding(findings, cursor, false)
        .map(finding_text)
        .unwrap_or_default()
}

fn primary_finding(findings: &[Finding], cursor: usize, include_missing: bool) -> Option<&Finding> {
    findings
        .iter()
        .filter(|finding| include_missing || !matches!(finding.reason, Reason::MissingCommand))
        .min_by_key(|finding| {
            let priority = match finding.state {
                State::Error => 0,
                State::Unavailable => 1,
                State::Incomplete => 2,
                State::Pending => 3,
                State::Unknown => 4,
                State::Known => 5,
            };
            (priority, !finding.range.contains(&cursor))
        })
}

fn finding_feedback(finding: &Finding, input: &Input, correction: Option<&Correction>) -> Feedback {
    let correction = correction.filter(|proposal| {
        proposal.matches(input)
            && proposal.range == finding.range
            && matches!(finding.reason, Reason::MissingCommand)
    });
    if let Some(proposal) = correction {
        return Feedback {
            text: format!(
                "{} {}{} {} {}",
                tr!("未找到", "Unknown"),
                proposal.from,
                tr!("，", ";"),
                tr!("建议", "try"),
                proposal.to
            ),
            compact: format!(
                "{} {} {}",
                proposal.from,
                style::stdout().glyph("→", "->"),
                proposal.to
            ),
            state: State::Error,
            correction: Some(proposal.clone()),
        };
    }
    let text = if matches!(finding.reason, Reason::MissingCommand) {
        format!(
            "{}: {}",
            tr!("未找到命令", "No executable command"),
            crate::status::plain(input.text.get(finding.range.clone()).unwrap_or_default())
        )
    } else {
        crate::status::plain(&finding_text(finding))
    };
    let compact = match &finding.reason {
        Reason::MissingCommand => tr!("无此命令", "No cmd").into(),
        Reason::MissingPath => tr!("路径不存在", "Path absent").into(),
        Reason::NotExecutable => tr!("路径不可执行", "Path not executable").into(),
        Reason::AccessDenied(_) => tr!("路径无法访问", "Path inaccessible").into(),
        Reason::NotDirectory => tr!("目标不是目录", "Not a directory").into(),
        Reason::DirectoryOutput => tr!("不能输出到目录", "Output is a directory").into(),
        Reason::Pending => tr!("查询中", "Query pending").into(),
        Reason::Limit => tr!("诊断资源超限", "Diagnostic limit").into(),
        Reason::NewTarget => tr!("输出尚未检查", "Output unchecked").into(),
        Reason::Dynamic => tr!("需要运行时解析", "Runtime resolution needed").into(),
        Reason::Snapshot => tr!("前序命令可能改变快照", "Snapshot may change").into(),
        Reason::Ai => tr!("AI 输入，尚未执行", "AI input, not executed").into(),
        Reason::Abbreviation => tr!("可用缩写，尚未展开", "Abbreviation, not expanded").into(),
        Reason::Syntax(reason) => style::clip_line(
            &format!("{}: {}", tr!("错误", "Error"), crate::status::plain(reason)),
            28,
            0,
            "...",
        ),
        Reason::Incomplete(reason) => style::clip_line(
            &format!(
                "{}: {}",
                tr!("待完成", "Incomplete"),
                crate::status::plain(reason)
            ),
            28,
            0,
            "...",
        ),
        Reason::Worker(reason) | Reason::Io(reason) => style::clip_line(
            &format!(
                "{}: {}",
                tr!("不可用", "Unavailable"),
                crate::status::plain(reason)
            ),
            28,
            0,
            "...",
        ),
    };
    Feedback {
        text,
        compact,
        state: finding.state,
        correction: None,
    }
}

fn finding_text(finding: &Finding) -> String {
    let label = match finding.state {
        State::Known => tr!("输入", "Input"),
        State::Error => tr!("输入错误", "Input error"),
        State::Incomplete => tr!("待完成", "Incomplete"),
        State::Unknown => tr!("未知", "Unknown"),
        State::Pending => tr!("查询中", "Pending"),
        State::Unavailable => tr!("诊断暂不可用", "Diagnostics unavailable"),
    };
    let reason = match &finding.reason {
        Reason::Syntax(s) | Reason::Incomplete(s) | Reason::Io(s) | Reason::Worker(s) => s.as_str(),
        Reason::Dynamic => tr!("需要运行时解析", "requires runtime resolution"),
        Reason::Snapshot => tr!(
            "前序命令可能改变文件状态",
            "prior commands may change this snapshot"
        ),
        Reason::MissingCommand => tr!(
            "当前用户未找到可执行命令",
            "no executable command found for the current user"
        ),
        Reason::MissingPath => tr!("当前快照中路径不存在", "path absent in this snapshot"),
        Reason::NotExecutable => tr!(
            "当前用户不可执行此路径",
            "the current user cannot execute this path"
        ),
        Reason::AccessDenied(path) => {
            return format!(
                "{label}: {}: {}",
                tr!(
                    "当前用户无法访问路径",
                    "path is inaccessible to the current user"
                ),
                style::visible_text(path)
            );
        }
        Reason::NotDirectory => tr!("目标不是目录", "target is not a directory"),
        Reason::DirectoryOutput => tr!(
            "不能将目录作为输出文件",
            "directory cannot be an output file"
        ),
        Reason::NewTarget => tr!(
            "新建输出目标，不代表可写",
            "new output target; writability not checked"
        ),
        Reason::Limit => tr!("达到分析资源上限", "analysis resource limit"),
        Reason::Pending => tr!(
            "正在分析或查询当前输入",
            "analyzing or looking up this input"
        ),
        Reason::Ai => tr!(
            "AI 输入，不会在输入时执行",
            "AI input; nothing runs while editing"
        ),
        Reason::Abbreviation => tr!("可用缩写，尚未展开", "available abbreviation, not expanded"),
    };
    format!("{label}: {}", style::visible_text(reason))
}

fn role_style(role: Role) -> Style {
    match role {
        Role::Builtin | Role::External => Color::Green.normal(),
        Role::Alias | Role::Function => Color::Blue.bold(),
        Role::Abbreviation | Role::Ai => Color::Cyan.normal(),
        Role::Keyword | Role::Operator => Color::Magenta.normal(),
        Role::String => Color::Yellow.normal(),
        Role::Variable => Color::Cyan.normal(),
        Role::Comment => Style::new().dimmed(),
        Role::Path => Style::new().underline(),
        Role::Incomplete => Color::Yellow.underline(),
        Role::Error => invalid_style(),
        Role::Text | Role::Command => Style::new(),
    }
}

fn contains_span(finding: &Finding, span: &Span) -> bool {
    finding.range.start <= span.range.start && span.range.end <= finding.range.end
}

fn editing_missing_command_style() -> Style {
    invalid_style()
}

fn invalid_style() -> Style {
    Color::Red.strikethrough()
}

#[derive(Default)]
struct Slot {
    worker: Option<Worker>,
    request: Option<Request>,
    failures: usize,
    failed_session: u64,
    error: Option<String>,
}

impl Slot {
    fn fail(&mut self, message: String, session: u64) {
        self.failures += 1;
        self.failed_session = session;
        self.error = Some(short_error(message));
        self.request = None;
        if let Some(worker) = &mut self.worker
            && let Err(error) = worker.stop()
        {
            self.error = Some(format!(
                "{}; cleanup: {error}",
                self.error.as_deref().unwrap_or("")
            ));
        }
    }

    fn reap(&mut self, session: u64) {
        if let Some(worker) = &mut self.worker
            && worker.stopping
        {
            match worker.reaped() {
                Ok(true) => self.worker = None,
                Ok(false) => {}
                Err(error) => {
                    self.error = Some(format!("worker not reaped: {error}"));
                }
            }
        }
        if self.worker.is_none() && self.failures == 1 && self.failed_session < session {
            self.error = None;
        }
    }

    fn ready(&self) -> bool {
        self.error.is_none() && self.worker.as_ref().is_none_or(|w| !w.busy && !w.stopping)
    }

    fn start(&mut self, request: Request, launcher: &WorkerCommand, kind: Kind, session: u64) {
        if self.worker.is_none() {
            match Worker::spawn(launcher, kind) {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    self.fail(short_error(error), session);
                    return;
                }
            }
        }
        if let Some(worker) = &mut self.worker {
            match worker.start(&request) {
                Ok(()) => self.request = Some(request),
                Err(error) => self.fail(short_error(error), session),
            }
        }
    }

    fn cancel(&mut self) {
        self.request = None;
        if let Some(worker) = &mut self.worker
            && worker.busy
            && !worker.stopping
            && let Err(error) = worker.stop()
        {
            self.error = Some(format!("cancel cleanup: {error}"));
        }
    }
}

struct ResolvedLookup {
    input: Arc<Input>,
    queries: Vec<Query>,
    observations: Arc<Vec<Observation>>,
}

fn supervise(shared: Arc<Shared>, launcher: WorkerCommand, index: SharedIndex) {
    let mut syntax = Slot::default();
    let mut lookup = Slot::default();
    let mut next_syntax = None;
    let mut next_lookup = None;
    let mut next_index = None;
    let mut next_correction = None;
    let mut resolved: Option<ResolvedLookup> = None;
    let mut seen: Option<(Version, Instant)> = None;
    let mut correction_attempt = None;
    let mut session = 0;
    let mut publication = Publication::default();
    let mut dirty = false;
    loop {
        if shared.stopped.load(Ordering::Acquire) {
            break;
        }
        let update = shared
            .mailbox
            .try_lock()
            .ok()
            .map(|mut mailbox| (mailbox.context.clone(), mailbox.latest.take()));
        if let Some((context, latest)) = update {
            if !shared.context_ok.load(Ordering::Acquire) {
                next_syntax = None;
                next_lookup = None;
                next_index = None;
                next_correction = None;
                resolved = None;
                syntax.cancel();
                lookup.cancel();
            } else {
                let new_session = shared.session.load(Ordering::Acquire);
                if new_session != session {
                    session = new_session;
                    next_index = None;
                    if let Some(context) = &context {
                        let cached = index.try_lock().ok().is_some_and(|guard| {
                            guard
                                .as_ref()
                                .is_some_and(|i| i.path == context.path && i.cwd == context.cwd)
                        });
                        if !cached {
                            next_index = Some(context.clone());
                        }
                    }
                }
                if let Some(input) = latest {
                    seen = Some((input.version, Instant::now()));
                    correction_attempt = None;
                    resolved = None;
                    next_correction = None;
                    publication.correction = None;
                    next_syntax = Some(input);
                    next_lookup = None;
                }
            }
        }
        syntax.reap(session);
        lookup.reap(session);
        for (slot, kind) in [(&mut syntax, Kind::Syntax), (&mut lookup, Kind::Lookup)] {
            let response = match slot.worker.as_mut().map(Worker::poll) {
                Some(Ok(Some(response))) => response,
                Some(Err(error)) => {
                    slot.fail(short_error(error), session);
                    continue;
                }
                _ => continue,
            };
            let request = slot.request.take();
            match (kind, request, response) {
                (Kind::Syntax, Some(Request::Analyze(input)), Response::Analysis(analysis))
                    if valid_analysis(&input, &analysis) =>
                {
                    if shared.current(input.version) {
                        publication.lookup = None;
                        resolved = None;
                        publication.correction = None;
                        next_lookup = None;
                        if !analysis.queries.is_empty() {
                            next_lookup = Some((input, analysis.queries.clone()));
                        }
                        publication.analysis = Some(Arc::new(analysis));
                        publication.serial += 1;
                        dirty = true;
                    }
                }
                (
                    Kind::Lookup,
                    Some(Request::Lookup { input, queries }),
                    Response::Lookup {
                        version,
                        observations,
                        stats,
                    },
                ) if version == input.version
                    && stats.cache_entries <= 512
                    && stats.cache_bytes <= 2 * 1024 * 1024
                    && observations.len() == queries.len()
                    && observations.len() <= MAX_QUERIES
                    && observations.iter().zip(&queries).all(|(o, q)| {
                        o.range == q.range
                            && input.text.get(o.range.clone()).is_some()
                            && o.finding
                                .as_ref()
                                .is_none_or(|f| f.range == q.range && f.reason.bounded())
                    }) =>
                {
                    if shared.current(version) {
                        if publication
                            .analysis
                            .as_ref()
                            .is_some_and(|a| a.version == version && a.ai_candidate)
                            && observations.iter().any(|o| o.role == Some(Role::External))
                        {
                            let mut resolved = (*input).clone();
                            resolved.command_input = true;
                            next_syntax = Some(Arc::new(resolved));
                            publication.analysis = None;
                            publication.lookup = None;
                            publication.serial += 1;
                            dirty = true;
                            continue;
                        }
                        let observations = Arc::new(observations);
                        resolved = Some(ResolvedLookup {
                            input,
                            queries,
                            observations: observations.clone(),
                        });
                        publication.lookup = Some((version, observations, stats));
                        publication.serial += 1;
                        dirty = true;
                    }
                }
                (
                    Kind::Lookup,
                    Some(Request::Correction { input, proposal }),
                    Response::Correction { version, accepted },
                ) if version == input.version && proposal.matches(&input) => {
                    if shared.current(version) {
                        publication.correction = accepted.then_some(proposal);
                        publication.serial += 1;
                        dirty = true;
                    }
                }
                (
                    Kind::Lookup,
                    Some(Request::Index {
                        session: requested_session,
                        context,
                    }),
                    Response::Index(result),
                ) if context.path == result.path
                    && context.cwd == result.cwd
                    && result.names.len() <= MAX_NAMES
                    && result.names.iter().map(String::len).sum::<usize>() <= MAX_INDEX_BYTES =>
                {
                    if shared.session.load(Ordering::Acquire) == requested_session
                        && let Ok(mut cache) = index.try_lock()
                    {
                        *cache = Some(result);
                    }
                }
                (_, _, Response::Failed(error)) => slot.fail(error, session),
                _ => slot.fail("invalid or mismatched worker response".into(), session),
            }
        }
        if shared.correction_enabled.load(Ordering::Acquire)
            && let Some(ResolvedLookup {
                input,
                queries,
                observations,
            }) = &resolved
            && shared.current(input.version)
            && correction_attempt != Some(input.version)
            && seen.is_some_and(|(version, since)| {
                version == input.version && since.elapsed() >= MISSING_COMMAND_IDLE
            })
            && let Some(cached) = index.try_lock().ok().and_then(|cache| cache.clone())
        {
            correction_attempt = Some(input.version);
            if let Some(proposal) = Correction::propose(input, queries, observations, &cached) {
                next_correction = Some((input.clone(), proposal));
            }
        }
        if syntax.ready()
            && let Some(input) = next_syntax.take()
            && shared.current(input.version)
        {
            syntax.start(Request::Analyze(input), &launcher, Kind::Syntax, session);
        }
        if lookup.ready() {
            if let Some((input, queries)) = next_lookup.take() {
                if shared.current(input.version) {
                    lookup.start(
                        Request::Lookup { input, queries },
                        &launcher,
                        Kind::Lookup,
                        session,
                    );
                }
            } else if let Some(context) = next_index.take()
                && shared.context_ok.load(Ordering::Acquire)
            {
                lookup.start(
                    Request::Index { session, context },
                    &launcher,
                    Kind::Lookup,
                    session,
                );
            } else if let Some((input, proposal)) = next_correction.take()
                && shared.current(input.version)
            {
                lookup.start(
                    Request::Correction { input, proposal },
                    &launcher,
                    Kind::Lookup,
                    session,
                );
            }
        }
        if let Ok(mut children) = shared.children.try_lock() {
            *children = [
                syntax.worker.as_ref().map(Worker::handle),
                lookup.worker.as_ref().map(Worker::handle),
            ];
        }
        if publication.syntax_error != syntax.error || publication.lookup_error != lookup.error {
            publication.syntax_error.clone_from(&syntax.error);
            publication.lookup_error.clone_from(&lookup.error);
            if syntax.error.is_some() || lookup.error.is_some() {
                publication.correction = None;
            }
            publication.serial += 1;
            dirty = true;
        }
        if dirty && shared.flush(&publication) {
            dirty = false;
        }
        let delayed = {
            let due_ms = shared.delayed_due_ms.load(Ordering::Acquire);
            if due_ms == 0 {
                None
            } else {
                let version = Version {
                    session: shared.delayed_session.load(Ordering::Acquire),
                    input: shared.delayed_input.load(Ordering::Acquire),
                };
                if !shared.current(version) {
                    shared.delayed_due_ms.store(0, Ordering::Release);
                    None
                } else {
                    let now_ms = Instant::now()
                        .saturating_duration_since(shared.started)
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64;
                    if now_ms + 1 >= due_ms {
                        shared.delayed_due_ms.store(0, Ordering::Release);
                        (shared.repaint)();
                        None
                    } else {
                        Some(Duration::from_millis(due_ms - now_ms - 1))
                    }
                }
            }
        };
        let busy = [&syntax, &lookup]
            .iter()
            .any(|s| s.worker.as_ref().is_some_and(|w| w.busy && !w.stopping));
        let retiring = [&syntax, &lookup]
            .iter()
            .any(|s| s.worker.as_ref().is_some_and(|w| w.stopping));
        let mut interval = if busy {
            Duration::from_millis(5)
        } else if dirty || (retiring && (next_syntax.is_some() || next_lookup.is_some())) {
            Duration::from_millis(100)
        } else {
            // An idle deadline also closes the notify-before-wait shutdown race.
            // No parser or filesystem work is performed on this wakeup.
            Duration::from_secs(1)
        };
        if let Some(delay) = delayed {
            interval = interval.min(delay);
        }
        if let Some((version, since)) = seen
            && shared.current(version)
            && correction_attempt != Some(version)
            && shared.correction_enabled.load(Ordering::Acquire)
        {
            let delay = (since + MISSING_COMMAND_IDLE).saturating_duration_since(Instant::now());
            if !delay.is_zero() {
                interval = interval.min(delay);
            }
        }
        if let Ok(mailbox) = shared.mailbox.try_lock() {
            if mailbox.latest.is_some() || shared.stopped.load(Ordering::Acquire) {
                continue;
            }
            drop(shared.wake.wait_timeout(mailbox, interval));
        } else {
            std::thread::sleep(interval);
        }
    }
    // Drop kills only these two children and never waits for a blocking syscall.
}

fn valid_analysis(input: &Input, analysis: &Analysis) -> bool {
    if analysis.version != input.version
        || analysis.spans.len() > MAX_SPANS
        || analysis.findings.len() > MAX_QUERIES + 1
        || analysis.queries.len() > MAX_QUERIES
    {
        return false;
    }
    let mut end = 0;
    for span in &analysis.spans {
        if span.range.start != end || input.text.get(span.range.clone()).is_none() {
            return false;
        }
        end = span.range.end;
    }
    end == input.text.len()
        && analysis
            .findings
            .iter()
            .all(|f| input.text.get(f.range.clone()).is_some() && f.reason.bounded())
        && analysis
            .queries
            .iter()
            .all(|q| input.text.get(q.range.clone()).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_assist::tests::{Fixture, launcher};
    use reedline::Emacs;

    fn editor() -> (Fixture, InputAssist, InputHighlighter) {
        let fixture = Fixture::new();
        let assist = InputAssist::new(None, Arc::new(Mutex::new(None)), Arc::new(|| {}), 80);
        assist.prepare(Ok(fixture.context()));
        let highlighter = InputHighlighter {
            assist: assist.clone(),
            cache: RefCell::new(RenderCache::default()),
        };
        (fixture, assist, highlighter)
    }

    #[test]
    fn startup_keeps_initial_context_when_the_first_request_is_contended() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let assist = InputAssist::new(
            Some(launcher("input_assist::tests::worker_probe")),
            Arc::new(Mutex::new(None)),
            Arc::new(|| {}),
            80,
        );
        // Only the assist and its lifetime own Shared before a valid snapshot.
        assert_eq!(Arc::strong_count(&assist.shared), 2);
        assist.prepare(Err("snapshot unavailable".into()));
        assert_eq!(Arc::strong_count(&assist.shared), 2);
        assert!(!assist.shared.context_ok.load(Ordering::Acquire));
        assert!(
            assist
                .feedback("")
                .is_some_and(|feedback| feedback.state == State::Unavailable)
        );

        assist.prepare(Ok(context.clone()));
        assert!(assist.shared.context_ok.load(Ordering::Acquire));
        let highlighter = InputHighlighter {
            assist: assist.clone(),
            cache: RefCell::new(RenderCache::default()),
        };
        let mailbox = assist.shared.mailbox.lock().unwrap();
        assert_eq!(mailbox.context.as_deref(), Some(&context));
        assert_eq!(highlighter.highlight("echo ok", 7).raw_string(), "echo ok");
        assert!(highlighter.cache.borrow().input.is_none());
        assert!(assist.shared.context_ok.load(Ordering::Acquire));
        assert_eq!(mailbox.context.as_deref(), Some(&context));
        drop(mailbox);

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert_eq!(highlighter.highlight("echo ok", 7).raw_string(), "echo ok");
            if highlighter.cache.borrow().input.is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "initial context was permanently lost"
            );
            std::thread::yield_now();
        }
        let input = highlighter.cache.borrow().input.clone().unwrap();
        assert_eq!(input.context.as_ref(), &context);
        assert_eq!(input.version.session, 2);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            highlighter.highlight("echo ok", 7);
            if assist
                .shared
                .publication
                .lock()
                .unwrap()
                .analysis
                .as_ref()
                .is_some_and(|analysis| analysis.version == input.version)
            {
                break;
            }
            assert!(Instant::now() < deadline, "first input was not analyzed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn cloned_assists_share_a_single_supervisor_start_attempt() {
        let fixture = Fixture::new();
        let assist = InputAssist::new(
            Some(launcher("input_assist::tests::worker_probe")),
            Arc::new(Mutex::new(None)),
            Arc::new(|| {}),
            80,
        );
        let other = assist.clone();
        assert!(Arc::ptr_eq(
            assist.startup.as_ref().unwrap(),
            other.startup.as_ref().unwrap()
        ));
        assert_eq!(Arc::strong_count(&assist.shared), 3);
        other.prepare(Ok(fixture.context()));
        let mailbox = assist.shared.mailbox.lock().unwrap();
        assert!(assist.shared.context_ok.load(Ordering::Acquire));
        assert!(mailbox.context.is_some());
        assert_eq!(Arc::strong_count(&assist.shared), 4);
        for _ in 0..10 {
            assist.start_supervisor();
            other.start_supervisor();
        }
        assert_eq!(Arc::strong_count(&assist.shared), 4);
    }

    #[test]
    fn redraws_and_cursor_moves_do_not_submit_more_work() {
        let (_, assist, highlighter) = editor();
        let text = "echo e\u{301} 中文";
        assert_eq!(highlighter.highlight(text, text.len()).raw_string(), text);
        let generation = assist.shared.generation.load(Ordering::Acquire);
        for _ in 0..20 {
            for (cursor, _) in text.char_indices() {
                assert_eq!(highlighter.highlight(text, cursor).raw_string(), text);
            }
        }
        assert_eq!(assist.shared.generation.load(Ordering::Acquire), generation);
    }

    fn key(
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> ReedlineRawEvent {
        ReedlineRawEvent::try_from(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(code, modifiers),
        ))
        .unwrap()
    }

    fn correction_context(buffer: &str) -> PromptContext<'_> {
        PromptContext {
            buffer,
            cursor: buffer.len(),
            completion_cursor: buffer.len(),
            selection: None,
            columns: 160,
            rows: 24,
            edit_mode: reedline::PromptEditMode::Emacs,
            interaction: PromptInteraction::Editing,
        }
    }

    fn ready_correction(text: &str) -> (Fixture, InputAssist, InputHighlighter) {
        let (fixture, assist, highlighter) = editor();
        assist.set_correction_enabled(true);
        highlighter.highlight(text, text.len());
        let input = highlighter.cache.borrow().input.clone().unwrap();
        let analysis = analysis::analyze(&input);
        let mut lookup = lookup::Lookup::default();
        let observations = lookup.run(&input, &analysis.queries);
        let index = Index {
            cwd: input.context.cwd.clone(),
            path: input.context.path.clone(),
            complete: true,
            reason: None,
            names: Arc::new(vec!["git".into()]),
        };
        let proposal =
            Correction::propose(&input, &analysis.queries, &observations, &index).unwrap();
        assert!(lookup.confirm_correction(&input, &proposal));
        assist.shared.publish(|p| {
            p.syntax_error = None;
            p.analysis = Some(Arc::new(analysis));
            p.lookup = Some((input.version, Arc::new(observations), lookup.stats()));
            p.correction = Some(proposal);
        });
        highlighter.cache.borrow_mut().requested = Some(Instant::now() - MISSING_COMMAND_IDLE);
        highlighter.highlight(text, text.len());
        assert!(assist.feedback(text).unwrap().correction.is_some());
        (fixture, assist, highlighter)
    }

    #[test]
    fn correction_adoption_is_one_undoable_edit_and_never_a_host_command() {
        let text = "gti status -- '中文 e\u{301} 👩\u{200d}💻' && echo 'tail'";
        let (_fixture, assist, _highlighter) = ready_correction(text);
        assert!(assist.present_correction(&correction_context(text), true));
        let mut mode = assist.edit_mode(Box::new(Emacs::default()));
        let event = mode.parse_event(key(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::NONE,
        ));
        let ReedlineEvent::Edit(commands) = event else {
            panic!("adoption must only edit the draft")
        };
        let mut editor = reedline::Reedline::create();
        editor.run_edit_commands(&[reedline::EditCommand::InsertString(text.into())]);
        editor.run_edit_commands(&commands);
        assert_eq!(
            editor.current_buffer_contents(),
            text.replacen("gti", "git", 1)
        );
        editor.run_edit_commands(&[reedline::EditCommand::Undo]);
        assert_eq!(editor.current_buffer_contents(), text);
    }

    #[test]
    fn earlier_keys_in_a_batch_disable_the_painted_correction_snapshot() {
        use crossterm::event::{KeyCode as K, KeyModifiers as M};
        for first in [
            key(K::Char('x'), M::NONE),
            key(K::Left, M::NONE),
            key(K::Left, M::SHIFT),
            key(K::Tab, M::NONE),
            key(K::Char('r'), M::CONTROL),
            ReedlineRawEvent::try_from(crossterm::event::Event::Paste("new input".into())).unwrap(),
            ReedlineRawEvent::try_from(crossterm::event::Event::Resize(60, 24)).unwrap(),
        ] {
            let (_fixture, assist, _highlighter) = ready_correction("gti status");
            assert!(assist.present_correction(&correction_context("gti status"), true));
            let mut mode = assist.edit_mode(Box::new(Emacs::default()));
            mode.parse_event(first);
            assert!(right_navigation(&mode.parse_event(key(K::Right, M::NONE))));
        }
    }

    #[test]
    fn line_middle_selection_menu_search_and_unpresented_correction_keep_right_navigation() {
        use crossterm::event::{KeyCode as K, KeyModifiers as M};
        let (_fixture, assist, _highlighter) = ready_correction("gti status");
        let mut mode = assist.edit_mode(Box::new(Emacs::default()));
        for case in 0..5 {
            let mut context = correction_context("gti status");
            match case {
                0 => context.cursor = 2,
                1 => context.selection = Some((0, 3)),
                2 => {
                    context.interaction = PromptInteraction::Menu {
                        name: "completion_menu",
                        count: 1,
                        provisional: false,
                    }
                }
                3 => {
                    context.interaction = PromptInteraction::HistorySearch {
                        term: "gti",
                        has_match: true,
                    }
                }
                _ => {}
            }
            assert!(!assist.present_correction(&context, case != 4));
            assert!(right_navigation(&mode.parse_event(key(K::Right, M::NONE))));
        }
    }

    #[test]
    fn local_correction_and_visible_history_hint_never_share_right() {
        let text = "gti status";
        let (_fixture, assist, _highlighter) = ready_correction(text);
        let mut history = reedline::FileBackedHistory::new(8).unwrap();
        history
            .save(reedline::HistoryItem::from_command_line(
                "gti status old-history",
            ))
            .unwrap();
        let mut hinter = assist.hinter(reedline::DefaultHinter::default());
        assert!(
            hinter
                .handle(text, text.len(), &history, false, "")
                .contains("old-history")
        );
        assert!(assist.present_correction(&correction_context(text), true));
        assert_eq!(hinter.handle(text, text.len(), &history, false, ""), "");
        assert_eq!(hinter.complete_hint(), "");
        assist.present_correction(&correction_context(text), false);
        assert!(
            hinter
                .handle(text, text.len(), &history, false, "")
                .contains("old-history")
        );
        assert!(hinter.complete_hint().contains("old-history"));
    }

    #[test]
    fn late_old_candidate_cannot_replace_a_new_draft_or_session() {
        let (_fixture, assist, highlighter) = ready_correction("gti status");
        let old = assist.shared.publication.lock().unwrap().clone();
        assert!(assist.present_correction(&correction_context("gti status"), true));
        highlighter.highlight("gti new-params", 14);
        assist.shared.publish(|p| *p = old);
        highlighter.highlight("gti new-params", 14);
        assert!(
            assist
                .feedback("gti new-params")
                .is_none_or(|feedback| feedback.correction.is_none())
        );
        assert!(!assist.present_correction(&correction_context("gti new-params"), true));
        assist.suspend();
        assert!(!assist.present_correction(&correction_context("gti status"), true));
    }

    #[test]
    fn real_owned_worker_confirms_local_correction_with_ai_disabled() {
        let fixture = Fixture::new();
        let mut context = fixture.context();
        context.ai_enabled = false;
        let assist = InputAssist::new(
            Some(launcher("input_assist::tests::worker_probe")),
            Arc::new(Mutex::new(None)),
            Arc::new(|| {}),
            160,
        );
        assist.prepare(Ok(context));
        assist.set_correction_enabled(true);
        let highlighter = assist.highlighter();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            highlighter.highlight("gti status", 10);
            if assist
                .feedback("gti status")
                .is_some_and(|feedback| feedback.correction.is_some())
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "owned worker never confirmed the local candidate"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(assist.present_correction(&correction_context("gti status"), true));
    }

    #[test]
    fn typed_feedback_waits_for_missing_command_delay_and_rejects_stale_input() {
        let (_, assist, highlighter) = editor();
        let text = "missing_command_for_feedback";
        highlighter.highlight(text, text.len());
        let input = highlighter.cache.borrow().input.clone().unwrap();
        let mut analysis = analysis::analyze(&input);
        analysis.queries.clear();
        analysis.findings = vec![Finding {
            range: 0..text.len(),
            state: State::Error,
            reason: Reason::MissingCommand,
        }];
        assist.shared.publish(|publication| {
            publication.syntax_error = None;
            publication.analysis = Some(Arc::new(analysis));
        });
        highlighter.highlight(text, text.len());
        assert!(assist.feedback(text).is_none());
        highlighter.cache.borrow_mut().requested = Some(Instant::now() - MISSING_COMMAND_IDLE);
        highlighter.highlight(text, text.len());
        let feedback = assist.feedback(text).unwrap();
        assert_eq!(feedback.state, State::Error);
        assert!(!feedback.text.is_empty() && !feedback.compact.is_empty());
        assert!(
            assist.status().is_empty(),
            "disabled status keeps its original feedback contract"
        );
        assert!(assist.feedback("different input").is_none());
        assist.suspend();
        assert!(assist.feedback(text).is_none());
    }

    #[test]
    fn search_hides_feedback_and_editing_restores_it_without_a_new_prompt() {
        let (_, assist, highlighter) = editor();
        highlighter.highlight("echo '", 6);
        assert!(assist.feedback("echo '").is_some());
        let mut mode = assist.edit_mode(Box::new(Emacs::default()));
        let raw = ReedlineRawEvent::try_from(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('r'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        ))
        .unwrap();
        mode.parse_event(raw);
        assert!(assist.feedback("echo '").is_none());
        highlighter.highlight("echo '", 6);
        assert!(assist.feedback("echo '").is_some());
    }

    #[test]
    fn unknown_command_editing_and_search_notices_are_separate() {
        let (_, assist, highlighter) = editor();
        let text = "missing arg";
        highlighter.highlight(text, 3);
        let input = highlighter.cache.borrow().input.clone().unwrap();
        let analysis = analysis::analyze(&input);
        let mut lookup = lookup::Lookup::default();
        let observations = lookup.run(&input, &analysis.queries);
        assist.shared.publish(|p| {
            p.syntax_error = None;
            p.analysis = Some(Arc::new(analysis));
            p.lookup = Some((input.version, Arc::new(observations), lookup.stats()));
        });
        highlighter.cache.borrow_mut().requested = Some(Instant::now());
        let editing = highlighter.highlight(text, 3);
        let editing_status = assist.status();
        assert!(
            highlighter
                .cache
                .borrow()
                .findings
                .iter()
                .any(|f| f.state == State::Error)
        );
        assert_eq!(editing_status, "");
        assert!(
            !editing
                .buffer
                .iter()
                .any(|(style, text)| { text == "missing" && style.foreground == Some(Color::Red) }),
            "typing should not immediately turn command-not-found red"
        );
        std::thread::sleep(MISSING_COMMAND_IDLE + Duration::from_millis(25));
        let idle = highlighter.highlight(text, 3);
        assert!(idle.buffer.iter().any(|(style, text)| {
            text == "missing"
                && style.foreground == Some(Color::Red)
                && style.is_strikethrough
                && !style.is_underline
        }));
        let generation = assist.shared.generation.load(Ordering::Acquire);
        let finished = highlighter.highlight(text, text.len());
        assert_eq!(assist.status(), "");
        assert!(
            finished
                .buffer
                .iter()
                .any(|(s, text)| text == "missing" && s.foreground == Some(Color::Red))
        );
        assert_eq!(generation, assist.shared.generation.load(Ordering::Acquire));
        assert!(
            highlighter
                .cache
                .borrow()
                .findings
                .iter()
                .any(|f| { f.state == State::Error && matches!(f.reason, Reason::MissingCommand) })
        );

        let boundary = "missing ";
        highlighter.highlight(boundary, boundary.len());
        let input = highlighter.cache.borrow().input.clone().unwrap();
        let analysis = analysis::analyze(&input);
        let observations = lookup::Lookup::default().run(&input, &analysis.queries);
        assist.shared.publish(|p| {
            p.syntax_error = None;
            p.analysis = Some(Arc::new(analysis));
            p.lookup = Some((
                input.version,
                Arc::new(observations),
                lookup::Lookup::default().stats(),
            ));
        });
        highlighter.highlight(boundary, boundary.len());
        assert_eq!(assist.status(), "");

        let finding = Finding {
            range: 0..7,
            state: State::Error,
            reason: Reason::MissingCommand,
        };
        let status = status_text(&[finding], 3);
        assert!(!status.contains("Permission denied") && !status.contains("WindowsApps"));
    }

    #[test]
    fn missing_command_delay_does_not_depend_on_cursor_position() {
        let (_, assist, highlighter) = editor();
        let text = "missing; echo ok";
        highlighter.highlight(text, text.len());
        let input = highlighter.cache.borrow().input.clone().unwrap();
        let analysis = analysis::analyze(&input);
        let mut lookup = lookup::Lookup::default();
        let observations = lookup.run(&input, &analysis.queries);
        assist.shared.publish(|p| {
            p.syntax_error = None;
            p.analysis = Some(Arc::new(analysis));
            p.lookup = Some((input.version, Arc::new(observations), lookup.stats()));
        });
        highlighter.cache.borrow_mut().requested = Some(Instant::now());

        let typing = highlighter.highlight(text, text.len());
        assert!(
            !typing
                .buffer
                .iter()
                .any(|(style, text)| text.contains("missing")
                    && style.foreground == Some(Color::Red)),
            "command-not-found should stay quiet while typing elsewhere"
        );

        std::thread::sleep(MISSING_COMMAND_IDLE + Duration::from_millis(25));
        let idle = highlighter.highlight(text, text.len());
        assert!(idle.buffer.iter().any(|(style, text)| {
            text.contains("missing")
                && style.foreground == Some(Color::Red)
                && style.is_strikethrough
        }));
    }

    #[test]
    fn ai_header_full_parse_failure_does_not_leave_header_analysis_pending() {
        let (_, assist, highlighter) = editor();
        let text = "ai < missing";
        highlighter.highlight(text, text.len());
        let input = highlighter.cache.borrow().input.clone().unwrap();
        let header = analysis::analyze(&input);
        assert!(header.ai_candidate);
        let observations = vec![Observation {
            range: 0..2,
            role: Some(Role::External),
            finding: None,
        }];
        assist.shared.publish(|publication| {
            publication.analysis = Some(Arc::new(header.clone()));
            publication.lookup = Some((
                input.version,
                Arc::new(observations),
                LookupStats {
                    metadata_calls: 1,
                    cache_entries: 0,
                    cache_bytes: 0,
                },
            ));
        });
        assist
            .shared
            .publish(|publication| publication.syntax_error = Some("parse failed".into()));
        highlighter.highlight(text, text.len());
        assert!(assist.status().contains("parse failed"));
    }

    #[test]
    fn diagnostic_messages_cannot_inject_more_prompt_lines() {
        let (_, assist, _) = editor();
        assist.set_display(&status_text(
            &[Finding {
                range: 0..0,
                state: State::Incomplete,
                reason: Reason::Incomplete("tag\nnext\tvalue\x1b[2J".into()),
            }],
            0,
        ));
        let status = assist.status();
        assert!(!status.contains(['\n', '\t', '\x1b']));
        assert!(status.contains("\\n") && status.contains("\\t"));
    }

    #[test]
    fn stale_results_and_publication_contention_cannot_corrupt_the_current_line() {
        let (_, assist, highlighter) = editor();
        highlighter.highlight("echo old", 8);
        let old = highlighter.cache.borrow().input.clone().unwrap();
        highlighter.highlight("echo new", 8);
        assist.shared.publish(|p| {
            p.analysis = Some(Arc::new(analysis::analyze(&old)));
            p.syntax_error = None;
        });
        assert_eq!(
            highlighter.highlight("echo new", 8).raw_string(),
            "echo new"
        );
        assert!(
            highlighter
                .cache
                .borrow()
                .spans
                .iter()
                .all(|s| s.role == Role::Text)
        );

        let guard = assist.shared.publication.lock().unwrap();
        let before = Instant::now();
        assert_eq!(
            highlighter.highlight("echo editable", 4).raw_string(),
            "echo editable"
        );
        assert!(before.elapsed() < Duration::from_millis(100));
        assert!(
            highlighter
                .cache
                .borrow()
                .findings
                .iter()
                .any(|f| f.state == State::Unavailable)
        );
        drop(guard);
    }

    #[test]
    fn completion_refreshes_context_and_reverse_search_hides_old_diagnostics() {
        let (mut fixture, assist, highlighter) = editor();
        highlighter.highlight("echo x", 6);
        let previous = assist.shared.session.load(Ordering::Acquire);
        fixture
            .shell
            .run_user_line("PATH=/updated; shopt -s expand_aliases; alias changed='echo'");
        assist.after_completion(&fixture.shell.shared().1);
        assert!(assist.shared.session.load(Ordering::Acquire) > previous);
        highlighter.highlight("changed", 7);
        let context = highlighter
            .cache
            .borrow()
            .input
            .clone()
            .unwrap()
            .context
            .clone();
        assert_eq!(context.path.as_deref(), Some("/updated"));
        assert!(context.aliases.contains("changed"));
        let mut mode = assist.edit_mode(Box::new(Emacs::default()));
        let event = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('r'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        mode.parse_event(event.try_into().unwrap());
        assert_eq!(assist.status(), "");
        highlighter.highlight("echo restored", 0);
        assert!(assist.shared.show_status.load(Ordering::Acquire));
    }

    #[test]
    fn new_session_context_clears_previous_status_text() {
        let (fixture, assist, _) = editor();
        assist.set_display("old error");
        assert_eq!(assist.status(), "old error");
        assist.prepare(Ok(fixture.context()));
        assert_eq!(assist.status(), "");
    }

    #[test]
    fn unreaped_workers_keep_their_quota_and_recovery_is_finite() {
        let launch = launcher("input_assist::tests::worker_probe");
        let worker = Worker::spawn(&launch, Kind::Syntax).unwrap();
        let original = worker.handle();
        let mut slot = Slot {
            worker: Some(worker),
            ..Slot::default()
        };
        slot.worker.as_mut().unwrap().hold_reaping(true);
        slot.fail("injected timeout".into(), 1);
        for session in 2..100 {
            slot.reap(session);
            assert!(!slot.ready());
            assert!(Arc::ptr_eq(
                &original,
                &slot.worker.as_ref().unwrap().handle()
            ));
        }
        slot.worker.as_mut().unwrap().hold_reaping(false);
        let deadline = Instant::now() + Duration::from_secs(2);
        while slot.worker.is_some() {
            slot.reap(100);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(slot.ready());
        slot.fail("second failure".into(), 100);
        for session in 101..200 {
            slot.reap(session);
            assert!(
                !slot.ready(),
                "repeated failures must latch, not restart forever"
            );
        }
    }

    #[test]
    fn input_size_limit_preserves_the_entire_buffer_and_recovers_after_editing() {
        let (_, _, highlighter) = editor();
        let mut text = "x".repeat(MAX_INPUT);
        assert_eq!(highlighter.highlight(&text, text.len()).raw_string(), text);
        assert!(highlighter.cache.borrow().input.is_some());
        text.push('x');
        assert_eq!(highlighter.highlight(&text, text.len()).raw_string(), text);
        assert!(highlighter.cache.borrow().input.is_none());
        highlighter.highlight("echo recovered", 14);
        assert!(highlighter.cache.borrow().input.is_some());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod measurements {
    use super::*;
    use crate::input_assist::tests::{Fixture, launcher};

    fn memory(pid: u32) -> (u64, u64, u64) {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let number = |field: &str| -> u64 {
            status
                .lines()
                .find_map(|line| line.strip_prefix(field))
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        };
        (number("VmRSS:"), number("VmHWM:"), number("Threads:"))
    }

    #[test]
    #[ignore = "fixed-device worker and cache measurements; no model"]
    fn input_assist_resource_comparison() {
        let fixture = Fixture::new();
        let context = fixture.context();
        std::fs::write(context.cwd.join("existing"), "data").unwrap();
        let before = memory(std::process::id());
        let assist = InputAssist::new(
            Some(launcher("input_assist::tests::worker_probe")),
            Arc::new(Mutex::new(None)),
            Arc::new(|| {}),
            80,
        );
        assist.prepare(Ok(context));
        let highlighter = InputHighlighter {
            assist: assist.clone(),
            cache: RefCell::new(RenderCache::default()),
        };
        let mut targets = 0;
        for i in 0..30 {
            let line = if i % 2 == 0 {
                "cat existing"
            } else {
                "cat  existing"
            };
            highlighter.highlight(line, line.len());
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                highlighter.highlight(line, line.len());
                let publication = assist.shared.publication.lock().unwrap();
                let version = highlighter.cache.borrow().input.as_ref().unwrap().version;
                if publication
                    .lookup
                    .as_ref()
                    .is_some_and(|(v, _, _)| *v == version)
                {
                    targets += publication.analysis.as_ref().unwrap().queries.len();
                    break;
                }
                assert!(
                    publication.syntax_error.is_none(),
                    "{:?}",
                    publication.syntax_error
                );
                assert!(
                    publication.lookup_error.is_none(),
                    "{:?}",
                    publication.lookup_error
                );
                drop(publication);
                assert!(Instant::now() < deadline, "measurement watchdog");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let generation = assist.shared.generation.load(Ordering::Acquire);
        for _ in 0..100 {
            highlighter.highlight("cat  existing", 0);
            highlighter.highlight("cat  existing", 5);
        }
        assert_eq!(generation, assist.shared.generation.load(Ordering::Acquire));
        let stats = assist
            .shared
            .publication
            .lock()
            .unwrap()
            .lookup
            .as_ref()
            .unwrap()
            .2;
        let handles = assist.shared.children.lock().unwrap().clone();
        let workers: Vec<_> = handles
            .into_iter()
            .flatten()
            .map(|handle| {
                let pid = handle.lock().unwrap().id();
                memory(pid)
            })
            .collect();
        assert_eq!(workers.len(), 2);
        let after = memory(std::process::id());
        println!(
            "{}",
            serde_json::json!({
                "fixture": "two real workers and the editor highlighter; test host",
                "edits": 30,
                "cursor_only_paints": 200,
                "cursor_only_new_requests": 0,
                "lookup_targets": targets,
                "metadata_calls": stats.metadata_calls,
                "cache_entries": stats.cache_entries,
                "cache_bytes": stats.cache_bytes,
                "parent_before_rss_kib": before.0,
                "parent_after_rss_kib": after.0,
                "parent_peak_rss_kib": after.1,
                "parent_threads_before": before.2,
                "parent_threads_after": after.2,
                "worker_peak_rss_kib": workers.iter().map(|w| w.1).collect::<Vec<_>>(),
                "worker_threads": workers.iter().map(|w| w.2).collect::<Vec<_>>(),
            })
        );
    }

    #[test]
    #[ignore = "fixed-device synchronous paint cost; no model"]
    fn input_assist_paint_budget() {
        let fixture = Fixture::new();
        let assist = InputAssist::new(None, Arc::new(Mutex::new(None)), Arc::new(|| {}), 80);
        assist.prepare(Ok(fixture.context()));
        let highlighter = InputHighlighter {
            assist: assist.clone(),
            cache: RefCell::new(RenderCache::default()),
        };
        for size in [4 * 1024, MAX_INPUT] {
            let content = "a".repeat(size - 32);
            let mut enabled = Vec::new();
            let mut disabled = Vec::new();
            for i in 0..300 {
                let text = format!("echo '{content}{i:04}'");
                let start = Instant::now();
                std::hint::black_box(plain(&text));
                disabled.push(start.elapsed());
                let start = Instant::now();
                std::hint::black_box(highlighter.highlight(&text, text.len()));
                enabled.push(start.elapsed());
            }
            for (on, mut values) in [(false, disabled), (true, enabled)] {
                values.sort_unstable();
                println!(
                    "{}",
                    serde_json::json!({
                        "input_assist": on,
                        "input_size_class": size,
                        "samples": values.len(),
                        "path": "first paint before analysis; includes required StyledText copy",
                        "p50_us": values[values.len() / 2].as_micros(),
                        "p95_us": values[values.len() * 95 / 100].as_micros(),
                        "p99_us": values[values.len() * 99 / 100].as_micros(),
                        "max_us": values.last().unwrap().as_micros(),
                    })
                );
            }
        }
    }
}
