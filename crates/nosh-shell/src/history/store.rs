use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak, mpsc};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

const HOT_ITEMS: usize = 1_000;
const HOT_BYTES: usize = 8 * 1024 * 1024;
pub(super) const INPUT_BYTES: usize = 256 * 1024;
pub(super) const PAGE_ITEMS: usize = 64;
const PAGE_BYTES: usize = 1024 * 1024;
const OPERATION_WAIT: Duration = Duration::from_secs(2);
const SCAN_ITEMS: usize = 10_000;
const SCAN_BYTES: usize = 16 * 1024 * 1024;
const SCAN_TIME: Duration = Duration::from_millis(250);
const SCAN_NOTICE: &str = "history search budget exceeded; results are incomplete";
const SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Command,
    Manual,
    Question,
    SuggestionRequest,
}

impl Kind {
    pub(crate) fn is_question(self) -> bool {
        matches!(self, Self::Question | Self::SuggestionRequest)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Outcomes {
    pub succeeded: u64,
    pub failed: u64,
    pub unknown: u64,
}

impl Outcomes {
    fn add(&mut self, exit_code: Option<i32>) {
        match exit_code {
            Some(0) => self.succeeded += 1,
            Some(_) => self.failed += 1,
            None => self.unknown += 1,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub key: String,
    pub ordinal: i64,
    pub legacy_id: i64,
    pub text: String,
    pub session: Option<String>,
    pub cwd: Option<String>,
    pub worktree: Option<String>,
    pub timestamp: Option<i64>,
    pub kind: Kind,
    pub exit_code: Option<i32>,
    /// Display-only aggregation; the command authority keeps individual records.
    grouped_outcomes: Option<Outcomes>,
    sequence: Option<i64>,
}

impl Entry {
    pub(super) fn repeat_count(&self) -> u64 {
        let counts = self.outcomes();
        counts.succeeded + counts.failed + counts.unknown
    }

    pub(super) fn outcomes(&self) -> Outcomes {
        self.grouped_outcomes.unwrap_or_else(|| {
            let mut outcomes = Outcomes::default();
            outcomes.add(self.exit_code);
            outcomes
        })
    }

    fn repeats(&self, other: &Self) -> bool {
        !self.kind.is_question()
            && self.kind == other.kind
            && self.text == other.text
            && self.session == other.session
            && self.cwd == other.cwd
            && self.worktree == other.worktree
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Query {
    pub text: String,
    pub worktree: Option<String>,
    pub before: Option<(i64, String)>,
    pub include_anchor: bool,
    pub older: bool,
    pub snapshot: Option<i64>,
}

impl Query {
    fn matches(&self, entry: &Entry) -> bool {
        entry.text.contains(&self.text)
            && self
                .worktree
                .as_ref()
                .is_none_or(|scope| entry.worktree.as_ref() == Some(scope))
            && self.in_range(entry)
    }

    fn in_range(&self, entry: &Entry) -> bool {
        self.before.as_ref().is_none_or(|(ordinal, key)| {
            match (entry.ordinal, &entry.key).cmp(&(*ordinal, key)) {
                std::cmp::Ordering::Less => self.older,
                std::cmp::Ordering::Equal => self.include_anchor,
                std::cmp::Ordering::Greater => !self.older,
            }
        })
    }

    fn is_anchor(&self, entry: &Entry) -> bool {
        self.before
            .as_ref()
            .is_some_and(|(ordinal, key)| entry.ordinal == *ordinal && entry.key == *key)
    }

    fn compare(&self, left: &Entry, right: &Entry) -> std::cmp::Ordering {
        let order = left
            .ordinal
            .cmp(&right.ordinal)
            .then(left.key.cmp(&right.key));
        if self.older { order.reverse() } else { order }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Page {
    pub entries: Vec<Entry>,
    pub more: bool,
    pub snapshot: i64,
    pub notice: Option<String>,
    pub failed: bool,
}

struct RepeatedPage<'a> {
    query: &'a Query,
    group: Option<Cow<'a, Entry>>,
    outcomes: Outcomes,
    skip: Option<Cow<'a, Entry>>,
    entries: Vec<Entry>,
    bytes: usize,
    more: bool,
    snapshot: i64,
}

impl<'a> RepeatedPage<'a> {
    fn new(query: &'a Query, snapshot: i64, anchor: Option<Cow<'a, Entry>>) -> Self {
        Self {
            query,
            group: None,
            outcomes: Outcomes::default(),
            skip: if query.include_anchor { None } else { anchor },
            entries: Vec::new(),
            bytes: 0,
            more: false,
            snapshot,
        }
    }

    fn push(&mut self, entry: Cow<'a, Entry>) -> bool {
        // A page boundary names the newest record of the preceding group.
        if self
            .skip
            .as_ref()
            .is_some_and(|anchor| anchor.repeats(&entry))
        {
            return true;
        }
        self.skip = None;
        if self
            .group
            .as_ref()
            .is_some_and(|group| group.repeats(&entry))
        {
            self.outcomes.add(entry.exit_code);
            if !self.query.older {
                self.group = Some(entry);
            }
            return true;
        }
        if !self.flush() {
            return false;
        }
        if self.query.matches(&entry) && self.full(&entry) {
            self.more = true;
            return false;
        }
        self.outcomes = Outcomes::default();
        self.outcomes.add(entry.exit_code);
        self.group = Some(entry);
        true
    }

    fn full(&self, entry: &Entry) -> bool {
        self.entries.len() >= PAGE_ITEMS || self.bytes + entry.text.len() > PAGE_BYTES
    }

    fn flush(&mut self) -> bool {
        let Some(group) = self.group.take() else {
            return true;
        };
        if !self.query.matches(&group) {
            return true;
        }
        if self.full(&group) {
            self.more = true;
            return false;
        }
        self.bytes += group.text.len();
        let mut group = group.into_owned();
        group.grouped_outcomes = Some(self.outcomes);
        self.entries.push(group);
        true
    }

    fn finish(mut self) -> Page {
        if !self.more {
            self.flush();
        }
        if self.query.older {
            self.entries.reverse();
        }
        Page {
            entries: self.entries,
            more: self.more,
            snapshot: self.snapshot,
            notice: None,
            failed: false,
        }
    }
}

fn memory_page(query: &Query, mut entries: Vec<Entry>, snapshot: i64) -> Page {
    let anchor = entries.iter().find(|entry| query.is_anchor(entry)).cloned();
    entries.retain(|entry| query.in_range(entry));
    entries.sort_by(|left, right| query.compare(left, right));
    let mut page = RepeatedPage::new(query, snapshot, anchor.map(Cow::Owned));
    for entry in entries {
        if !page.push(Cow::Owned(entry)) {
            break;
        }
    }
    page.finish()
}

fn cached_page(model: &Model, query: &Query) -> Page {
    let all = || model.commands.iter().chain(&model.questions);
    let anchor = all()
        .find(|entry| query.is_anchor(entry))
        .map(Cow::Borrowed);
    let mut entries = all()
        .filter(|entry| query.in_range(entry))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| query.compare(left, right));
    let mut page = RepeatedPage::new(query, model.snapshot, anchor);
    for entry in entries {
        if !page.push(Cow::Borrowed(entry)) {
            break;
        }
    }
    page.finish()
}

struct CommandRecord(Entry);

impl CommandRecord {
    fn new(entry: Entry) -> Option<Self> {
        (!entry.kind.is_question()).then_some(Self(entry))
    }
}

enum Job {
    Open {
        database: Option<PathBuf>,
        epoch: u64,
        retention: Option<usize>,
    },
    Add(
        CommandRecord,
        Vec<String>,
        u64,
        Option<mpsc::Sender<Result<(), String>>>,
    ),
    Outcome {
        key: String,
        text: String,
        epoch: u64,
        exit_code: i32,
    },
    Delete(String, mpsc::Sender<Result<(), String>>),
    Clear {
        snapshot: i64,
        pending: Vec<String>,
        visible: BTreeSet<String>,
        questions_before: i64,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Update(CommandRecord, mpsc::Sender<Result<(), String>>),
    Query(u64, Query),
    Refresh(mpsc::Sender<Result<(), String>>),
    Retention(Option<usize>),
    Sync(mpsc::Sender<Result<(), String>>),
    #[cfg(test)]
    Wait(mpsc::Receiver<()>),
}

struct Model {
    commands: Vec<Entry>,
    questions: Vec<Entry>,
    pending: BTreeSet<String>,
    persisted: BTreeSet<String>,
    exported: BTreeSet<String>,
    errors: VecDeque<String>,
    next_id: i64,
    ordinal: i64,
    snapshot: i64,
    database: Option<PathBuf>,
    unavailable: Option<String>,
    query: Option<(u64, Page)>,
    revision: u64,
    context: Option<PathBuf>,
    worktree: Option<String>,
    scope_error: Option<String>,
    enabled: bool,
    view_limit: usize,
    retention: Option<usize>,
    epoch: u64,
}

impl Model {
    fn brush_items(&self, session: &str) -> Vec<brush_core::history::Item> {
        self.commands
            .iter()
            .skip(self.commands.len().saturating_sub(self.view_limit))
            .map(|entry| brush_core::history::Item {
                id: entry.legacy_id,
                command_line: entry.text.clone(),
                timestamp: entry
                    .timestamp
                    .and_then(chrono::DateTime::from_timestamp_micros),
                dirty: entry.session.as_deref() == Some(session)
                    && !self.exported.contains(&entry.key),
            })
            .collect()
    }

    fn load_commands(&mut self, entries: Vec<Entry>, snapshot: i64) {
        let existing = self
            .commands
            .iter()
            .map(|entry| {
                (
                    entry.key.clone(),
                    (
                        entry.legacy_id,
                        entry.exit_code.map(|code| (entry.text.clone(), code)),
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        self.commands
            .retain(|entry| !self.persisted.contains(&entry.key));
        self.persisted.clear();
        for mut entry in entries.into_iter().rev() {
            self.persisted.insert(entry.key.clone());
            if self.commands.iter().any(|old| old.key == entry.key) {
                continue;
            }
            if let Some((id, outcome)) = existing.get(&entry.key) {
                entry.legacy_id = *id;
                if entry.exit_code.is_none()
                    && let Some((text, code)) = outcome
                    && entry.text == *text
                {
                    entry.exit_code = Some(*code);
                }
            } else {
                entry.legacy_id = self.next_id;
                self.next_id += 1;
            }
            self.ordinal = self.ordinal.max(entry.ordinal);
            self.commands.push(entry);
        }
        self.commands
            .sort_by(|a, b| a.ordinal.cmp(&b.ordinal).then(a.key.cmp(&b.key)));
        trim_commands(self);
        self.snapshot = snapshot;
    }
}

struct Inner {
    model: Mutex<Model>,
    tx: mpsc::SyncSender<Job>,
    generation: Arc<AtomicU64>,
    repaint: Mutex<Option<reedline::RepaintSignal>>,
    session: String,
}

impl Inner {
    fn wake(&self) {
        let has_errors = !lock(&self.model).errors.is_empty();
        if let Some(signal) = &*lock(&self.repaint) {
            if has_errors {
                signal.request_host_command(super::NOTICE.into());
            } else {
                signal.request_repaint();
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct Store(Arc<Inner>);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn database_path(file: &Path) -> PathBuf {
    let mut path = file.as_os_str().to_owned();
    path.push(".nosh.sqlite3");
    PathBuf::from(path)
}

impl Store {
    pub(crate) fn new(
        histfile: Option<&Path>,
        history_size: Option<&str>,
        file_size: Option<&str>,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel(128);
        let store = Self(Arc::new(Inner {
            model: Mutex::new(Model {
                commands: Vec::new(),
                questions: Vec::new(),
                pending: BTreeSet::new(),
                persisted: BTreeSet::new(),
                exported: BTreeSet::new(),
                errors: VecDeque::new(),
                next_id: 0,
                ordinal: 0,
                snapshot: 0,
                database: None,
                unavailable: None,
                query: None,
                revision: 0,
                context: None,
                worktree: None,
                scope_error: None,
                enabled: true,
                view_limit: HOT_ITEMS,
                retention: None,
                epoch: 0,
            }),
            tx,
            generation: Arc::new(AtomicU64::new(0)),
            repaint: Mutex::new(None),
            session: Uuid::new_v4().to_string(),
        }));
        let weak = Arc::downgrade(&store.0);
        let generation = store.0.generation.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("nosh-history".into())
            .spawn(move || worker(weak, rx, generation))
        {
            store.report(format!("history worker unavailable: {error}"));
        }
        if let Err(error) = store.configure(histfile, history_size, file_size) {
            store.report(format!("history storage configuration failed: {error}"));
        }
        store
    }

    pub(crate) fn configure(
        &self,
        histfile: Option<&Path>,
        history_size: Option<&str>,
        file_size: Option<&str>,
    ) -> io::Result<()> {
        let histfile = histfile.filter(|file| *file != Path::new("/dev/null"));
        let database = histfile.map(database_path);
        let view_limit = history_size
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(HOT_ITEMS)
            .min(HOT_ITEMS);
        let retention = file_size.and_then(|value| value.trim().parse::<usize>().ok());
        {
            let mut model = lock(&self.0.model);
            if model.database == database && model.revision != 0 && model.unavailable.is_none() {
                if model.retention != retention {
                    self.0
                        .tx
                        .try_send(Job::Retention(retention))
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    model.retention = retention;
                    model.revision += 1;
                    model.query = None;
                    self.cancel_query();
                }
                model.view_limit = view_limit;
                return Ok(());
            }
            let epoch = model.epoch.wrapping_add(1);
            self.0
                .tx
                .try_send(Job::Open {
                    database: database.clone(),
                    epoch,
                    retention,
                })
                .map_err(|error| io::Error::other(error.to_string()))?;
            model.database = database.clone();
            model.revision += 1;
            model.snapshot = 0;
            model.persisted.clear();
            model.epoch = epoch;
            model.query = None;
            model.view_limit = view_limit;
            model.retention = retention;
        }
        self.cancel_query();
        Ok(())
    }

    pub(crate) fn repaint(&self, signal: reedline::RepaintSignal) {
        *lock(&self.0.repaint) = Some(signal);
        self.0.wake();
    }

    pub(crate) fn context(&self, cwd: &Path, enabled: bool) {
        let result = worktree(cwd);
        let mut model = lock(&self.0.model);
        model.enabled = enabled;
        model.context = Some(cwd.to_path_buf());
        match result {
            Ok(root) => {
                model.worktree = root;
                model.scope_error = None;
            }
            Err(error) => {
                model.worktree = None;
                model.scope_error = Some(format!("project history unavailable: {error}"));
            }
        }
    }

    pub(super) fn scope(&self) -> Result<Option<String>, String> {
        let model = lock(&self.0.model);
        match &model.scope_error {
            Some(error) => Err(error.clone()),
            None => Ok(model.worktree.clone()),
        }
    }

    pub(super) fn session(&self) -> &str {
        &self.0.session
    }

    #[cfg(test)]
    pub(crate) fn record(&self, text: &str, kind: Kind) -> Option<i64> {
        self.record_replacing(text, kind, &[])
    }

    pub(crate) fn record_replacing(&self, text: &str, kind: Kind, replace: &[i64]) -> Option<i64> {
        if text.trim().is_empty() {
            return None;
        }
        if text.len() > INPUT_BYTES {
            self.report(format!(
                "history input not recorded: exceeds {INPUT_BYTES} bytes; execution is unchanged"
            ));
            return None;
        }
        let entry = {
            let mut model = lock(&self.0.model);
            if !model.enabled {
                return None;
            }
            let timestamp = chrono::Utc::now().timestamp_micros();
            model.ordinal = timestamp.max(model.ordinal.saturating_add(1));
            let entry = Entry {
                key: Uuid::new_v4().to_string(),
                ordinal: model.ordinal,
                legacy_id: model.next_id,
                text: text.to_owned(),
                session: Some(self.0.session.clone()),
                cwd: model
                    .context
                    .as_deref()
                    .and_then(Path::to_str)
                    .map(str::to_owned),
                worktree: model.worktree.clone(),
                timestamp: Some(timestamp),
                kind,
                exit_code: None,
                grouped_outcomes: None,
                sequence: None,
            };
            if !kind.is_question() {
                model.next_id += 1;
            }
            entry
        };
        let id = (!kind.is_question()).then_some(entry.legacy_id);
        if let Err(error) = self.add_entry(entry, false, replace) {
            self.report(format!("command history not saved: {error}"));
        }
        id
    }

    fn add_entry(&self, entry: Entry, confirm: bool, replace: &[i64]) -> io::Result<()> {
        if entry.kind.is_question() && !replace.is_empty() {
            return Err(io::Error::other(
                "temporary questions cannot replace command history",
            ));
        }
        let (persist, epoch, replaced) = {
            let mut model = lock(&self.0.model);
            let replaced = model
                .commands
                .iter()
                .filter(|old| replace.contains(&old.legacy_id) && old.text == entry.text)
                .map(|old| old.key.clone())
                .collect::<BTreeSet<_>>();
            let persist =
                !entry.kind.is_question() && model.database.is_some() && model.retention != Some(0);
            if entry.kind.is_question() {
                model.questions.push(entry.clone());
                if trim(&mut model.questions) {
                    notice(
                        &mut model,
                        "oldest temporary questions evicted from bounded history".into(),
                    );
                }
            } else {
                model.commands.retain(|old| !replaced.contains(&old.key));
                model.persisted.retain(|key| !replaced.contains(key));
                model.exported.retain(|key| !replaced.contains(key));
                model.commands.push(entry.clone());
                trim_commands(&mut model);
                if persist {
                    model.pending.insert(entry.key.clone());
                }
            }
            model.revision += 1;
            (persist, model.epoch, replaced.into_iter().collect())
        };
        if persist && let Some(command) = CommandRecord::new(entry) {
            let key = command.0.key.clone();
            let (reply, receiver) = if confirm {
                let (tx, rx) = mpsc::channel();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
            if let Err(error) = self
                .0
                .tx
                .try_send(Job::Add(command, replaced, epoch, reply))
            {
                lock(&self.0.model).pending.remove(&key);
                return Err(io::Error::other(error.to_string()));
            }
            if let Some(receiver) = receiver {
                receiver
                    .recv_timeout(OPERATION_WAIT)
                    .map_err(|error| {
                        io::Error::other(format!("history write not confirmed: {error}"))
                    })?
                    .map_err(io::Error::other)?;
            }
        }
        self.0.wake();
        Ok(())
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        self.control(Job::Sync)
    }

    fn complete_execution(
        &self,
        key: String,
        text: String,
        epoch: u64,
        persist: bool,
        exit_code: i32,
    ) -> io::Result<()> {
        if !(0..=255).contains(&exit_code) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shell exit code must be in 0..255",
            ));
        }
        {
            let mut model = lock(&self.0.model);
            if let Some(entry) = model
                .commands
                .iter_mut()
                .find(|entry| entry.key == key && entry.text == text)
            {
                entry.exit_code = Some(exit_code);
                entry.grouped_outcomes = None;
            }
            model.revision += 1;
            model.query = None;
        }
        self.cancel_query();
        let result = if persist {
            self.0
                .tx
                .try_send(Job::Outcome {
                    key,
                    text,
                    epoch,
                    exit_code,
                })
                .map_err(|error| io::Error::other(error.to_string()))
        } else {
            Ok(())
        };
        self.0.wake();
        result
    }

    fn control(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<(), String>>) -> Job,
    ) -> io::Result<()> {
        self.cancel_query();
        let (tx, rx) = mpsc::channel();
        self.0
            .tx
            .try_send(make(tx))
            .map_err(|error| io::Error::other(error.to_string()))?;
        match rx.recv_timeout(OPERATION_WAIT) {
            Ok(result) => result.map_err(io::Error::other),
            Err(error) => Err(io::Error::other(format!(
                "history operation not confirmed ({error}); storage may still be completing it"
            ))),
        }
    }

    pub(crate) fn report_errors(&self) {
        for error in self.take_errors() {
            eprintln!("nosh: {}", crate::style::visible_text(&error));
        }
    }

    pub(crate) fn take_errors(&self) -> Vec<String> {
        lock(&self.0.model).errors.drain(..).collect()
    }

    pub(crate) fn report(&self, error: String) {
        notice(&mut lock(&self.0.model), error);
        self.0.wake();
    }

    pub(crate) fn brush_view(&self, history: &mut brush_core::history::History) {
        let model = lock(&self.0.model);
        history.replace_view(
            model.brush_items(self.session()),
            model.next_id,
            Arc::new(self.clone()),
            model.database.clone(),
        );
    }

    #[cfg(test)]
    pub(super) fn cached(&self) -> Vec<Entry> {
        lock(&self.0.model).commands.clone()
    }

    pub(super) fn with_commands<R>(&self, read: impl FnOnce(&[Entry]) -> R) -> R {
        let model = lock(&self.0.model);
        read(&model.commands[model.commands.len().saturating_sub(model.view_limit)..])
    }

    pub(super) fn preview(&self, query: &Query) -> Page {
        let model = lock(&self.0.model);
        cached_page(&model, query)
    }

    pub(super) fn latest_own_anchor(&self) -> Option<(i64, String)> {
        let model = lock(&self.0.model);
        model
            .commands
            .iter()
            .chain(&model.questions)
            .filter(|entry| entry.session.as_deref() == Some(self.session()))
            .max_by_key(|entry| entry.ordinal)
            .map(|entry| (entry.ordinal, entry.key.clone()))
    }

    pub(super) fn revision(&self) -> u64 {
        lock(&self.0.model).revision
    }

    pub(super) fn query(&self, query: Query) -> u64 {
        let generation = self.0.generation.fetch_add(1, Ordering::AcqRel) + 1;
        lock(&self.0.model).query = None;
        if let Err(error) = self.0.tx.try_send(Job::Query(generation, query)) {
            let mut model = lock(&self.0.model);
            model.query = Some((
                generation,
                Page {
                    entries: Vec::new(),
                    more: false,
                    snapshot: model.snapshot,
                    notice: Some(format!("history search unavailable: {error}")),
                    failed: true,
                },
            ));
        }
        generation
    }

    pub(super) fn poll(&self, generation: u64) -> Option<Page> {
        lock(&self.0.model)
            .query
            .as_ref()
            .filter(|(current, _)| *current == generation)
            .map(|(_, page)| page.clone())
    }

    pub(super) fn cancel_query(&self) {
        self.0.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn entry(&self, id: i64) -> io::Result<Entry> {
        lock(&self.0.model)
            .commands
            .iter()
            .find(|entry| entry.legacy_id == id)
            .cloned()
            .ok_or_else(|| io::Error::other("history record is no longer in the current view"))
    }
}

impl brush_core::history::Observer for Store {
    fn execution_started(
        &self,
        id: i64,
    ) -> io::Result<Option<brush_core::history::CompletionCallback>> {
        let (key, text, epoch, persist) = {
            let model = lock(&self.0.model);
            let entry = model
                .commands
                .iter()
                .find(|entry| entry.legacy_id == id)
                .ok_or_else(|| {
                    io::Error::other("history execution record is no longer available")
                })?;
            (
                entry.key.clone(),
                entry.text.clone(),
                model.epoch,
                model.database.is_some() && model.retention != Some(0),
            )
        };
        let store = self.clone();
        Ok(Some(Box::new(move |exit_code| {
            store.complete_execution(key, text, epoch, persist, exit_code)
        })))
    }

    fn configure(
        &self,
        file: Option<&Path>,
        size: Option<&str>,
        file_size: Option<&str>,
    ) -> io::Result<()> {
        Store::configure(self, file, size, file_size)
    }

    fn refreshed(&self) -> io::Result<(Vec<brush_core::history::Item>, i64)> {
        self.control(Job::Refresh)?;
        let model = lock(&self.0.model);
        Ok((model.brush_items(self.session()), model.next_id))
    }

    fn validate_export(&self, path: &Path) -> io::Result<()> {
        let model = lock(&self.0.model);
        if model
            .database
            .as_ref()
            .is_some_and(|database| brush_core::history::protected_export_target(database, path))
        {
            return Err(io::Error::other(
                "cannot export text over the history database",
            ));
        }
        Ok(())
    }

    fn exported(&self, ids: &[i64]) {
        let mut model = lock(&self.0.model);
        let keys = model
            .commands
            .iter()
            .filter(|entry| ids.contains(&entry.legacy_id))
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>();
        model.exported.extend(keys);
    }

    fn added(
        &self,
        item: &mut brush_core::history::Item,
        directory: Option<&Path>,
        replace: &[i64],
    ) -> io::Result<()> {
        if item.command_line.len() > INPUT_BYTES {
            return Err(io::Error::other(
                "manual history entry exceeds the input limit",
            ));
        }
        let ordinal = {
            let mut model = lock(&self.0.model);
            item.id = model.next_id;
            model.next_id += 1;
            model.ordinal = chrono::Utc::now()
                .timestamp_micros()
                .max(model.ordinal.saturating_add(1));
            model.ordinal
        };
        let worktree = match directory.map(worktree).transpose() {
            Ok(worktree) => worktree.flatten(),
            Err(error) => {
                self.report(format!("history worktree unavailable: {error}"));
                None
            }
        };
        self.add_entry(
            Entry {
                key: Uuid::new_v4().to_string(),
                ordinal,
                legacy_id: item.id,
                text: item.command_line.clone(),
                session: Some(self.session().to_owned()),
                cwd: directory.and_then(Path::to_str).map(str::to_owned),
                worktree,
                timestamp: item.timestamp.map(|timestamp| timestamp.timestamp_micros()),
                kind: if directory.is_some() {
                    Kind::Command
                } else {
                    Kind::Manual
                },
                exit_code: None,
                grouped_outcomes: None,
                sequence: None,
            },
            true,
            replace,
        )
    }

    fn updated(&self, item: &brush_core::history::Item) -> io::Result<()> {
        let mut entry = self.entry(item.id)?;
        if item.command_line.len() > INPUT_BYTES {
            return Err(io::Error::other(
                "updated history entry exceeds the input limit",
            ));
        }
        if entry.text != item.command_line {
            entry.exit_code = None;
            entry.grouped_outcomes = None;
        }
        entry.text = item.command_line.clone();
        self.control(|reply| Job::Update(CommandRecord(entry), reply))
    }

    fn removed(&self, id: i64) -> io::Result<()> {
        let entry = self.entry(id)?;
        self.control(|reply| Job::Delete(entry.key, reply))
    }

    fn cleared(&self) -> io::Result<()> {
        let (snapshot, pending, visible, questions_before) = {
            let model = lock(&self.0.model);
            (
                model.snapshot,
                model
                    .commands
                    .iter()
                    .filter(|entry| !model.persisted.contains(&entry.key))
                    .map(|entry| entry.key.clone())
                    .chain(model.pending.iter().cloned())
                    .collect(),
                model
                    .commands
                    .iter()
                    .map(|entry| entry.key.clone())
                    .collect(),
                model.ordinal,
            )
        };
        self.control(|reply| Job::Clear {
            snapshot,
            pending,
            visible,
            questions_before,
            reply,
        })
    }
}

fn notice(model: &mut Model, error: String) {
    if model.errors.back() != Some(&error) {
        if model.errors.len() == 8 {
            model.errors.pop_front();
        }
        model.errors.push_back(error);
    }
}

fn trim(entries: &mut Vec<Entry>) -> bool {
    let mut bytes = entries.iter().map(|entry| entry.text.len()).sum::<usize>();
    let mut remove = entries.len().saturating_sub(HOT_ITEMS);
    bytes -= entries
        .iter()
        .take(remove)
        .map(|entry| entry.text.len())
        .sum::<usize>();
    while bytes > HOT_BYTES && remove < entries.len() {
        bytes -= entries[remove].text.len();
        remove += 1;
    }

    entries.drain(..remove);
    remove != 0
}

fn trim_commands(model: &mut Model) {
    trim(&mut model.commands);
    let keys = model
        .commands
        .iter()
        .map(|entry| entry.key.as_str())
        .collect::<BTreeSet<_>>();
    model.persisted.retain(|key| keys.contains(key.as_str()));
    model.exported.retain(|key| keys.contains(key.as_str()));
}

fn worktree(cwd: &Path) -> io::Result<Option<String>> {
    let cwd = cwd.canonicalize()?;
    for (index, directory) in cwd.ancestors().enumerate() {
        if index == 32 {
            return Err(io::Error::other("Git worktree lookup exceeds 32 ancestors"));
        }
        let marker = directory.join(".git");
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_dir() => {
                return directory
                    .to_str()
                    .map(|value| Some(value.to_owned()))
                    .ok_or_else(|| io::Error::other("worktree path is not UTF-8"));
            }
            Ok(metadata) if metadata.is_file() && metadata.len() <= 4096 => {
                let text = std::fs::read_to_string(marker)?;
                if text
                    .trim()
                    .strip_prefix("gitdir:")
                    .is_some_and(|path| !path.trim().is_empty())
                {
                    return directory
                        .to_str()
                        .map(|value| Some(value.to_owned()))
                        .ok_or_else(|| io::Error::other("worktree path is not UTF-8"));
                }
                return Err(io::Error::other("invalid Git worktree marker"));
            }
            Ok(_) => return Err(io::Error::other("unsupported Git worktree marker")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn open_database(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        nosh_platform::paths::ensure_private_dir(parent).map_err(|error| error.to_string())?;
    }
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err("history database must be a regular, non-symlink file".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|error| error.to_string())?;
    let mut connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(Duration::ZERO)
        .map_err(|error| error.to_string())?;
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if version != 0 && version != SCHEMA_VERSION {
        return Err(format!("unsupported history database version {version}"));
    }
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA temp_store=MEMORY;")
        .map_err(|error| error.to_string())?;
    if version == 0 {
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let current: i64 = transaction
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if current == 0 {
            transaction
                .execute_batch(
                    "CREATE TABLE commands (
                         seq INTEGER PRIMARY KEY AUTOINCREMENT,
                         id TEXT NOT NULL UNIQUE,
                         ordinal INTEGER NOT NULL,
                         command TEXT NOT NULL,
                         session TEXT,
                         cwd TEXT,
                         worktree TEXT,
                         timestamp INTEGER,
                         source TEXT NOT NULL CHECK(source IN ('command','manual')),
                         exit_code INTEGER CHECK(exit_code IS NULL OR (typeof(exit_code)='integer' AND exit_code BETWEEN 0 AND 255))
                     );
                     CREATE INDEX commands_order ON commands(ordinal DESC,id DESC);",
                )
                .map_err(|error| error.to_string())?;
            transaction
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|error| error.to_string())?;
        } else if current != SCHEMA_VERSION {
            return Err(format!("unsupported history database version {current}"));
        }
        transaction.commit().map_err(|error| error.to_string())?;
    }
    Ok(connection)
}

fn insert(connection: &Connection, entry: &Entry) -> Result<(), String> {
    if entry.kind.is_question() {
        return Err("temporary questions cannot be persisted".into());
    }
    connection
        .execute(
            "INSERT INTO commands(id,ordinal,command,session,cwd,worktree,timestamp,source,exit_code)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                entry.key,
                entry.ordinal,
                entry.text,
                entry.session,
                entry.cwd,
                entry.worktree,
                entry.timestamp,
                if entry.kind == Kind::Manual {
                    "manual"
                } else {
                    "command"
                },
                entry.exit_code,
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn row_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        key: row.get(0)?,
        ordinal: row.get(1)?,
        legacy_id: 0,
        text: row.get(2)?,
        session: row.get(3)?,
        cwd: row.get(4)?,
        worktree: row.get(5)?,
        timestamp: row.get(6)?,
        kind: if row.get::<_, String>(7)? == "manual" {
            Kind::Manual
        } else {
            Kind::Command
        },
        exit_code: row.get(9)?,
        grouped_outcomes: None,
        sequence: Some(row.get(8)?),
    })
}

const COMMAND_SELECT: &str =
    "SELECT id,ordinal,CASE WHEN length(CAST(command AS BLOB))<=?1 THEN command END,
            session,cwd,worktree,timestamp,source,seq,exit_code FROM commands";

fn query_snapshot(connection: &Connection, snapshot: Option<i64>) -> Result<i64, String> {
    match snapshot {
        Some(snapshot) => Ok(snapshot),
        None => connection
            .query_row("SELECT coalesce(max(seq),0) FROM commands", [], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string()),
    }
}

fn recent_commands(connection: &Connection) -> Result<(Vec<Entry>, i64), String> {
    let snapshot = query_snapshot(connection, None)?;
    let mut statement = connection
        .prepare_cached(&format!(
            "{COMMAND_SELECT} WHERE seq<=?2 ORDER BY ordinal DESC,id DESC LIMIT ?3"
        ))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(
            params![INPUT_BYTES as i64, snapshot, HOT_ITEMS as i64],
            row_entry,
        )
        .map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    let mut bytes = 0;
    for entry in rows {
        let entry = entry.map_err(|error| error.to_string())?;
        bytes += entry.text.len();
        if bytes > HOT_BYTES {
            break;
        }
        entries.push(entry);
    }
    Ok((entries, snapshot))
}

fn select_repeated(
    connection: &Connection,
    query: &Query,
    mut memory: Vec<Entry>,
    known_outcomes: &BTreeMap<String, (String, i32)>,
) -> Result<Page, String> {
    let started = Instant::now();
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let snapshot = query_snapshot(&transaction, query.snapshot)?;
    let anchor = if let Some((ordinal, id)) = &query.before
        && !query.include_anchor
    {
        transaction
            .query_row(
                &format!("{COMMAND_SELECT} WHERE seq<=?2 AND ordinal=?3 AND id=?4"),
                params![INPUT_BYTES as i64, snapshot, ordinal, id],
                row_entry,
            )
            .optional()
            .map_err(|error| error.to_string())?
            .or_else(|| memory.iter().find(|entry| query.is_anchor(entry)).cloned())
    } else {
        None
    };
    memory.retain(|entry| query.in_range(entry));
    memory.sort_by(|left, right| query.compare(left, right));
    let mut memory = memory.into_iter().peekable();
    let (comparison, direction) = if query.older {
        ("<", "DESC")
    } else {
        (">", "ASC")
    };
    let mut statement = transaction
        .prepare_cached(&format!(
            "{COMMAND_SELECT} WHERE seq<=?2
             AND (?3 IS NULL OR (ordinal,id) {comparison} (?3,?4)
                  OR (?5 AND ordinal=?3 AND id=?4))
             ORDER BY ordinal {direction},id {direction}"
        ))
        .map_err(|error| error.to_string())?;
    // Questions stay in Rust memory, never in a SQLite temp table or window sort.
    let mut rows = statement
        .query_map(
            params![
                INPUT_BYTES as i64,
                snapshot,
                query.before.as_ref().map(|(ordinal, _)| *ordinal),
                query.before.as_ref().map(|(_, id)| id),
                query.include_anchor,
            ],
            row_entry,
        )
        .map_err(|error| error.to_string())?;
    let mut disk = rows.next().transpose().map_err(|error| error.to_string())?;
    let mut page = RepeatedPage::new(query, snapshot, anchor.map(Cow::Owned));
    let mut scanned = 0usize;
    let mut bytes = 0usize;
    let mut limited = false;
    while disk.is_some() || memory.peek().is_some() {
        let from_disk = match (disk.as_ref(), memory.peek()) {
            (Some(left), Some(right)) => query.compare(left, right).is_le(),
            (Some(_), None) => true,
            _ => false,
        };
        let mut entry = if from_disk {
            let entry = disk.take().expect("available disk row");
            if memory.peek().is_some_and(|other| other.key == entry.key) {
                memory.next();
            }
            entry
        } else {
            memory.next().expect("available memory row")
        };
        if scanned == SCAN_ITEMS
            || bytes.saturating_add(entry.text.len()) > SCAN_BYTES
            || started.elapsed() >= SCAN_TIME
        {
            limited = true;
            page.more = true;
            break;
        }
        scanned += 1;
        bytes += entry.text.len();
        if entry.exit_code.is_none()
            && let Some((text, exit_code)) = known_outcomes.get(&entry.key)
            && entry.text == *text
        {
            entry.exit_code = Some(*exit_code);
        }
        if !page.push(Cow::Owned(entry)) {
            break;
        }
        if from_disk {
            disk = rows.next().transpose().map_err(|error| error.to_string())?;
        }
    }
    drop(rows);
    drop(statement);
    transaction.commit().map_err(|error| error.to_string())?;
    let mut page = page.finish();
    if limited {
        page.failed = true;
        page.notice = Some(SCAN_NOTICE.into());
    }
    Ok(page)
}

fn update_model(inner: &Weak<Inner>, update: impl FnOnce(&mut Model)) {
    if let Some(inner) = inner.upgrade() {
        update(&mut lock(&inner.model));
        inner.wake();
    }
}

fn prune(connection: &Connection, limit: Option<usize>) -> Result<i64, String> {
    let Some(limit) = limit else { return Ok(0) };
    let cutoff: Option<i64> = connection
        .query_row(
            "SELECT seq FROM commands ORDER BY seq DESC LIMIT 1 OFFSET ?1",
            [limit.min(i64::MAX as usize) as i64],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some(cutoff) = cutoff {
        connection
            .execute("DELETE FROM commands WHERE seq<=?1", [cutoff])
            .map_err(|error| error.to_string())?;
        Ok(cutoff)
    } else {
        Ok(0)
    }
}

fn mark_pruned(model: &mut Model, cutoff: i64) {
    for entry in &model.commands {
        if entry.sequence.is_some_and(|sequence| sequence <= cutoff) {
            model.persisted.remove(&entry.key);
        }
    }
}

fn worker(inner: Weak<Inner>, rx: mpsc::Receiver<Job>, generation: Arc<AtomicU64>) {
    let mut database: Option<Connection> = None;
    let mut wanted_database = false;
    let mut failure: Option<String> = None;
    let mut write_failure: Option<String> = None;
    let mut retention = None;
    let mut active_epoch = 0;
    while let Ok(job) = rx.recv() {
        match job {
            Job::Open {
                database: path,
                epoch,
                retention: limit,
            } => {
                active_epoch = epoch;
                retention = limit;
                wanted_database = path.is_some();
                database = None;
                let loaded =
                    path.as_deref()
                        .map(open_database)
                        .transpose()
                        .and_then(|connection| {
                            database = connection;
                            write_failure = None;
                            database
                                .as_ref()
                                .map(|connection| {
                                    prune(connection, retention)?;
                                    recent_commands(connection)
                                })
                                .transpose()
                        });
                match loaded {
                    Ok(rows) => {
                        failure = None;
                        update_model(&inner, |model| {
                            if model.epoch != epoch {
                                return;
                            }
                            model.unavailable = None;
                            if let Some((rows, snapshot)) = rows {
                                model.load_commands(rows, snapshot);
                            }
                            model.revision += 1;
                        });
                    }
                    Err(error) => {
                        failure = Some(error.clone());
                        update_model(&inner, |model| {
                            if model.epoch != epoch {
                                return;
                            }
                            model.unavailable = Some(error.clone());
                            notice(model, format!("history unavailable: {error}"));
                        });
                    }
                }
            }
            Job::Add(command, replace, epoch, reply) => {
                let entry = command.0;
                let result = if epoch != active_epoch {
                    Err(
                        "history destination changed before the write; input remains in memory"
                            .into(),
                    )
                } else {
                    match &mut database {
                        Some(connection) => (|| -> Result<(i64, i64), String> {
                            let transaction = connection
                                .transaction()
                                .map_err(|error| error.to_string())?;
                            for key in &replace {
                                transaction
                                    .execute(
                                        "DELETE FROM commands WHERE id=?1 AND command=?2",
                                        params![key, entry.text],
                                    )
                                    .map_err(|error| error.to_string())?;
                            }
                            insert(&transaction, &entry)?;
                            let sequence = transaction.last_insert_rowid();
                            let cutoff = prune(&transaction, retention)?;
                            transaction.commit().map_err(|error| error.to_string())?;
                            Ok((sequence, cutoff))
                        })(),
                        None => Err("database is unavailable; command remains in memory".into()),
                    }
                };
                if let Err(error) = &result {
                    write_failure = Some(error.clone());
                }
                let confirmed = result.as_ref().map(|_| ()).map_err(|error| {
                    format!("history not saved; input remains in memory: {error}")
                });
                update_model(&inner, |model| {
                    model.pending.remove(&entry.key);
                    match result {
                        Ok((sequence, cutoff)) => {
                            for key in &replace {
                                model.pending.remove(key);
                                model.persisted.remove(key);
                                model.exported.remove(key);
                            }
                            if model.epoch == epoch {
                                model.snapshot = model.snapshot.max(sequence);
                                model.persisted.insert(entry.key.clone());
                            }
                            if let Some(saved) = model
                                .commands
                                .iter_mut()
                                .find(|saved| saved.key == entry.key)
                            {
                                saved.sequence = Some(sequence);
                            }
                            if model.epoch == epoch {
                                mark_pruned(model, cutoff);
                            }
                        }
                        Err(error) => notice(model, format!("command history not saved: {error}")),
                    }
                });
                if let Some(reply) = reply {
                    let _ = reply.send(confirmed);
                }
            }
            Job::Outcome {
                key,
                text,
                epoch,
                exit_code,
            } => {
                let result = if epoch != active_epoch {
                    Err("history destination changed before the execution status was saved".into())
                } else {
                    match &database {
                        Some(connection) => connection
                            .execute(
                                "UPDATE commands SET exit_code=?3 WHERE id=?1 AND command=?2",
                                params![key, text, exit_code],
                            )
                            .map(|_| ())
                            .map_err(|error| error.to_string()),
                        None => Err("history database is unavailable".into()),
                    }
                };
                if let Err(error) = result {
                    write_failure = Some(error.clone());
                    update_model(&inner, |model| {
                        notice(
                            model,
                            format!("history execution status not saved: {error}"),
                        );
                    });
                }
            }
            Job::Delete(key, reply) => {
                let result = match &database {
                    Some(connection) => connection
                        .execute("DELETE FROM commands WHERE id=?1", [&key])
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    None if !wanted_database => Ok(()),
                    None => Err("persistent history deletion unavailable".into()),
                };
                if result.is_ok() {
                    update_model(&inner, |model| {
                        model.commands.retain(|entry| entry.key != key);
                        model.pending.remove(&key);
                        model.persisted.remove(&key);
                        model.exported.remove(&key);
                        model.revision += 1;
                    });
                }
                let _ = reply.send(result);
            }
            Job::Clear {
                snapshot,
                pending,
                visible,
                questions_before,
                reply,
            } => {
                let result = if let Some(connection) = &mut database {
                    (|| -> Result<(), String> {
                        let transaction = connection
                            .transaction()
                            .map_err(|error| error.to_string())?;
                        transaction
                            .execute("DELETE FROM commands WHERE seq<=?1", [snapshot])
                            .map_err(|error| error.to_string())?;
                        for key in pending {
                            transaction
                                .execute("DELETE FROM commands WHERE id=?1", [key])
                                .map_err(|error| error.to_string())?;
                        }
                        transaction.commit().map_err(|error| error.to_string())
                    })()
                } else if wanted_database {
                    Err("persistent history clear unavailable".into())
                } else {
                    Ok(())
                };
                if result.is_ok() {
                    update_model(&inner, |model| {
                        model.commands.retain(|entry| !visible.contains(&entry.key));
                        model
                            .questions
                            .retain(|entry| entry.ordinal > questions_before);
                        model.pending.retain(|key| !visible.contains(key));
                        model.persisted.retain(|key| !visible.contains(key));
                        model.exported.retain(|key| !visible.contains(key));
                        model.revision += 1;
                    });
                }
                let _ = reply.send(result);
            }
            Job::Update(command, reply) => {
                let entry = command.0;
                let result = match &database {
                    Some(connection) => match connection.execute(
                        "UPDATE commands SET command=?2,exit_code=?3 WHERE id=?1",
                        params![entry.key, entry.text, entry.exit_code],
                    ) {
                        Ok(0) => Err("history item was deleted; it was not recreated".into()),
                        Ok(_) => Ok(()),
                        Err(error) => Err(error.to_string()),
                    },
                    None if !wanted_database => Ok(()),
                    None => Err("persistent history update unavailable".into()),
                };
                if result.is_ok() {
                    update_model(&inner, |model| {
                        if let Some(old) =
                            model.commands.iter_mut().find(|old| old.key == entry.key)
                        {
                            *old = entry;
                        }
                        trim_commands(model);
                        model.revision += 1;
                    });
                }
                let _ = reply.send(result);
            }
            Job::Query(current, query) => {
                if current != generation.load(Ordering::Acquire) {
                    continue;
                }
                let (memory, known_outcomes, snapshot) = match inner.upgrade() {
                    Some(shared) => {
                        let model = lock(&shared.model);
                        (
                            model
                                .commands
                                .iter()
                                .chain(&model.questions)
                                .filter(|entry| {
                                    database.is_none() || !model.persisted.contains(&entry.key)
                                })
                                .cloned()
                                .collect(),
                            model
                                .commands
                                .iter()
                                .filter_map(|entry| {
                                    entry
                                        .exit_code
                                        .map(|code| (entry.key.clone(), (entry.text.clone(), code)))
                                })
                                .collect(),
                            model.snapshot,
                        )
                    }
                    None => break,
                };
                let result = if let Some(connection) = &database {
                    let generation = generation.clone();
                    let started = Instant::now();
                    let installed = connection
                        .progress_handler(
                            1_000,
                            Some(move || {
                                current != generation.load(Ordering::Acquire)
                                    || started.elapsed() >= SCAN_TIME
                            }),
                        )
                        .map_err(|error| error.to_string());
                    let result = installed.and_then(|()| {
                        select_repeated(connection, &query, memory, &known_outcomes)
                    });
                    let result = if result.is_err() && started.elapsed() >= SCAN_TIME {
                        Err(SCAN_NOTICE.into())
                    } else {
                        result
                    };
                    match connection.progress_handler(0, None::<fn() -> bool>) {
                        Ok(()) => result,
                        Err(error) => {
                            Err(format!("history query cancellation reset failed: {error}"))
                        }
                    }
                } else {
                    Ok(memory_page(&query, memory, snapshot))
                };
                if current != generation.load(Ordering::Acquire) {
                    continue;
                }
                update_model(&inner, |model| {
                    let mut page = match result {
                        Ok(page) if page.failed && page.entries.is_empty() => {
                            let mut cached = cached_page(model, &query);
                            for entry in &mut cached.entries {
                                entry.grouped_outcomes = None;
                            }
                            cached.failed = true;
                            cached.notice = page
                                .notice
                                .map(|notice| format!("{notice}; cached matches only"));
                            cached
                        }
                        Ok(page) => page,
                        Err(error) => {
                            let mut page = cached_page(model, &query);
                            if error.contains(SCAN_NOTICE) {
                                for entry in &mut page.entries {
                                    entry.grouped_outcomes = None;
                                }
                            }
                            page.notice = Some(format!("history search failed: {error}"));
                            page.failed = true;
                            page
                        }
                    };
                    if model.unavailable.is_some() {
                        page.failed = true;
                        page.notice = page.notice.or_else(|| model.unavailable.clone());
                    }
                    if page.notice.is_none() {
                        page.notice = model.errors.back().cloned();
                    }
                    model.snapshot = model.snapshot.max(page.snapshot);
                    model.query = Some((current, page));
                });
            }
            Job::Sync(reply) => {
                let result = if let Some(error) = &failure {
                    Err(error.clone())
                } else if wanted_database && database.is_none() {
                    Err("history database unavailable".into())
                } else if let Some(error) = &write_failure {
                    Err(format!("some commands were not saved: {error}"))
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
            }
            Job::Refresh(reply) => {
                let result = match &database {
                    Some(connection) => recent_commands(connection).map(|(entries, snapshot)| {
                        update_model(&inner, |model| {
                            model.load_commands(entries, snapshot);
                            model.revision += 1;
                        });
                    }),
                    None if !wanted_database => Ok(()),
                    None => Err("persistent history refresh unavailable".into()),
                };
                let _ = reply.send(result);
            }
            Job::Retention(limit) => {
                retention = limit;
                if let Some(connection) = &database {
                    match prune(connection, limit) {
                        Ok(cutoff) => update_model(&inner, |model| mark_pruned(model, cutoff)),
                        Err(error) => update_model(&inner, |model| {
                            notice(model, format!("history retention not applied: {error}"));
                        }),
                    }
                }
            }
            #[cfg(test)]
            Job::Wait(release) => {
                let _ = release.recv();
            }
        }
    }
}

#[cfg(test)]
mod tests;
