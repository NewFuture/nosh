use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use reedline::{AbbreviationEdit, AbbreviationExpander, InputIntent};

use super::{Config, Probe, Registry, command_ranges};
use crate::input_assist::worker::{ChildHandle, Kind, Worker, kill_child};
use crate::input_assist::{Abbreviations, Context, Request, Response, WorkerCommand};

pub(crate) const READY: &str = "__nosh_abbreviations_ready__";

struct State {
    config: Config,
    context: Option<Context>,
    serial: u64,
    pending: Option<Probe>,
    ready: Option<(u64, Registry)>,
    registry: Arc<Registry>,
    approved: Abbreviations,
    checked: Option<Instant>,
    error: Option<String>,
}

#[derive(Default)]
struct ExpansionView {
    rules: Abbreviations,
    prefix: Option<String>,
    deferred: std::collections::BTreeSet<String>,
}

impl State {
    fn expansion_view(&self) -> ExpansionView {
        let deferred = self
            .registry
            .entries
            .iter()
            .filter(|entry| entry.available && !self.approved.applicable.contains(&entry.name))
            .filter(|entry| {
                let override_existing = self
                    .config
                    .rules
                    .get(&entry.name)
                    .is_some_and(|rule| rule.override_existing);
                override_existing
                    || self.context.as_ref().is_none_or(|context| {
                        !context.aliases.contains(&entry.name)
                            && !context.functions.contains(&entry.name)
                            && !context.builtins.contains(&entry.name)
                            && !context.hashed_commands.contains_key(&entry.name)
                    })
            })
            .map(|entry| entry.name.clone())
            .collect();
        ExpansionView {
            rules: self.approved.clone(),
            prefix: self
                .context
                .as_ref()
                .filter(|context| context.ai_enabled)
                .map(|context| context.ai_prefix.clone()),
            deferred,
        }
    }
}

struct Shared {
    state: Mutex<State>,
    // Published only by the editor/host, never by the probe supervisor.
    view: Mutex<Arc<ExpansionView>>,
    input_error: Mutex<Option<String>>,
    child: Mutex<Option<ChildHandle>>,
    wake: Condvar,
    stopped: AtomicBool,
    frozen: AtomicBool,
    notify: Arc<dyn Fn() + Send + Sync>,
    literal: Arc<AtomicBool>,
    expanded: Mutex<Vec<Range<usize>>>,
    #[cfg(test)]
    spawns: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    hold_reaping: AtomicBool,
}

struct Lifetime(Arc<Shared>);

impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.stopped.store(true, Ordering::Release);
        if let Ok(slot) = self.0.child.try_lock()
            && let Some(child) = slot.as_ref()
            && let Err(error) = kill_child(child)
        {
            eprintln!("nosh abbreviation cleanup: {error}");
        }
        self.0.wake.notify_one();
    }
}

#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
    _lifetime: Arc<Lifetime>,
    available: bool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AbbreviationSession")
    }
}

impl Session {
    fn update_view(&self, state: &State) {
        *self.shared.view.lock().unwrap_or_else(|e| e.into_inner()) =
            Arc::new(state.expansion_view());
    }

    pub fn new(launcher: Option<WorkerCommand>, notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                config: Config::default(),
                context: None,
                serial: 0,
                pending: None,
                ready: None,
                registry: Arc::new(Registry::default()),
                approved: Abbreviations::default(),
                checked: None,
                error: None,
            }),
            view: Mutex::new(Arc::new(ExpansionView::default())),
            input_error: Mutex::new(None),
            child: Mutex::new(None),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            frozen: AtomicBool::new(false),
            notify,
            literal: Arc::new(AtomicBool::new(false)),
            expanded: Mutex::new(Vec::new()),
            #[cfg(test)]
            spawns: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            hold_reaping: AtomicBool::new(false),
        });
        let mut available = launcher.is_some();
        if let Some(launcher) = launcher {
            let background = shared.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("nosh-abbreviations".into())
                .spawn(move || supervise(background, launcher))
            {
                eprintln!("nosh abbreviation supervisor: {error}");
                available = false;
            }
        }
        Self {
            _lifetime: Arc::new(Lifetime(shared.clone())),
            shared,
            available,
        }
    }

    pub(crate) fn begin_prompt(
        &self,
        shell: &crate::EmbeddedShell,
        config: Config,
        trigger: &crate::TriggerConfig,
    ) {
        self.shared.literal.store(false, Ordering::Release);
        *self
            .shared
            .input_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.prepare(shell, config, trigger, true);
    }

    pub(crate) fn environment_changed(
        &self,
        shell: &crate::EmbeddedShell,
        trigger: &crate::TriggerConfig,
    ) {
        let config = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .clone();
        self.prepare(shell, config, trigger, false);
    }

    fn prepare(
        &self,
        shell: &crate::EmbeddedShell,
        config: Config,
        trigger: &crate::TriggerConfig,
        new_input: bool,
    ) {
        let context = shell.input_context(trigger, &Abbreviations::default());
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let unchanged = context.as_ref().is_ok_and(|context| {
            state.context.as_ref().is_some_and(|previous| {
                let registry = state
                    .ready
                    .as_ref()
                    .filter(|(serial, _)| *serial == state.serial)
                    .map_or(state.registry.as_ref(), |(_, registry)| registry);
                registry.compatible_context(previous, context, &config)
            })
        }) && state.config == config;
        if new_input {
            self.shared.frozen.store(false, Ordering::Release);
            if let Some((serial, registry)) = state.ready.take()
                && serial == state.serial
                && unchanged
            {
                state.registry = Arc::new(registry);
                state.checked = Some(Instant::now());
            }
        }
        let retain = unchanged
            && state
                .checked
                .is_some_and(|checked| checked.elapsed() < Duration::from_secs(5));
        if retain && !new_input {
            return;
        }
        state.serial = state.serial.wrapping_add(1);
        state.config = config.clone();
        if !retain {
            state.checked = None;
        }
        state.approved = if retain {
            state.registry.approved(state.serial)
        } else {
            Abbreviations {
                revision: state.serial,
                ..Default::default()
            }
        };
        if !retain {
            state.registry = Arc::new(Registry::pending(&config));
        }
        state.error = None;
        match context {
            Ok(context) if self.available && config.enabled => {
                state.pending = Some(Probe {
                    serial: state.serial,
                    config,
                    context: context.clone(),
                });
                state.context = Some(context);
            }
            Ok(context) if !config.enabled => {
                state.context = Some(context);
                state.pending = None;
                state.checked = Some(Instant::now());
            }
            result => {
                let error = result
                    .err()
                    .unwrap_or_else(|| "abbreviation worker entry point unavailable".into());
                state.registry = Arc::new(Registry::unavailable(&config, &error));
                state.error = Some(error);
                state.context = None;
                state.pending = None;
            }
        }
        self.update_view(&state);
        drop(state);
        self.shared.wake.notify_one();
    }

    pub(crate) fn refresh(&self) -> bool {
        let Ok(mut state) = self.shared.state.try_lock() else {
            return false;
        };
        let Some((serial, registry)) = state.ready.take() else {
            return false;
        };
        if serial != state.serial {
            return false;
        }
        state.checked = Some(Instant::now());
        state.registry = Arc::new(registry);
        let next = state.registry.approved(serial);
        let previous = state.approved.clone();
        if !self.shared.frozen.load(Ordering::Acquire) {
            state.approved = next;
        } else {
            state
                .approved
                .definitions
                .retain(|name, definition| next.definitions.get(name) == Some(definition));
            let names = state.approved.definitions.keys().cloned().collect();
            state.approved.applicable = names;
        }
        self.update_view(&state);
        previous != state.approved
    }

    pub fn registry(&self) -> Arc<Registry> {
        self.refresh();
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .registry
            .clone()
    }

    pub(crate) fn approved(&self) -> Abbreviations {
        self.shared
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rules
            .clone()
    }

    pub(crate) fn literal(&self) -> Arc<AtomicBool> {
        self.shared.literal.clone()
    }

    pub(crate) fn is_literal(&self) -> bool {
        self.shared.literal.load(Ordering::Acquire)
    }

    pub(crate) fn expanded(&self) -> Vec<Range<usize>> {
        self.shared
            .expanded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.shared
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rules
            .applicable
            .contains(name)
    }

    pub(crate) fn expansion(&self, name: &str, revision: u64) -> Option<String> {
        let view = self.shared.view.lock().unwrap_or_else(|e| e.into_inner());
        (view.rules.revision == revision && view.rules.applicable.contains(name))
            .then(|| {
                view.rules
                    .definitions
                    .get(name)
                    .map(|rule| rule.expansion.clone())
            })
            .flatten()
    }

    pub(crate) fn take_error(&self) -> Option<String> {
        self.shared
            .input_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .or_else(|| self.shared.state.try_lock().ok()?.error.take())
    }

    pub(crate) fn accepts(&self, name: &str, revision: u64, value: &str) -> bool {
        if self.refresh() {
            (self.shared.notify)();
        }
        let view = self.shared.view.lock().unwrap_or_else(|e| e.into_inner());
        view.rules.revision == revision
            && view
                .rules
                .definitions
                .get(name)
                .is_some_and(|rule| rule.expansion == value)
    }

    pub fn inspect(
        shell: &crate::EmbeddedShell,
        config: Config,
        launcher: WorkerCommand,
    ) -> Result<Registry, String> {
        let session = Self::new(Some(launcher), Arc::new(|| {}));
        session.begin_prompt(shell, config, &crate::TriggerConfig::default());
        let started = Instant::now();
        loop {
            session.refresh();
            {
                let state = session
                    .shared
                    .state
                    .lock()
                    .map_err(|_| "abbreviation state unavailable")?;
                if let Some(error) = &state.error {
                    return Err(error.clone());
                }
                if state.checked.is_some() {
                    return Ok((*state.registry).clone());
                }
            }
            if started.elapsed() > crate::input_assist::INDEX_TIMEOUT * 3 {
                return Err(
                    "abbreviation inspection timed out; no rule was assumed available".into(),
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl AbbreviationExpander for Session {
    fn expanded_ranges(&mut self, ranges: &[Range<usize>]) {
        let mut expanded = self
            .shared
            .expanded
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if expanded.as_slice() != ranges {
            expanded.clear();
            expanded.extend_from_slice(ranges);
        }
    }

    fn error(&mut self, message: &str) {
        *self
            .shared
            .input_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(message.into());
        (self.shared.notify)();
    }

    fn input_changed(&mut self, input: &str, intent: InputIntent) {
        self.shared
            .literal
            .store(intent == InputIntent::Literal, Ordering::Release);
        let was_frozen = self.shared.frozen.swap(!input.is_empty(), Ordering::AcqRel);
        if input.is_empty() {
            let mut changed = self.refresh();
            if was_frozen {
                let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.checked.is_some() {
                    let approved = state.registry.approved(state.serial);
                    changed |= approved != state.approved;
                    state.approved = approved;
                    self.update_view(&state);
                }
            }
            if changed {
                (self.shared.notify)();
            }
        }
    }

    fn expand(
        &mut self,
        input: &str,
        cursor: usize,
        submitted: bool,
        excluded: &[Range<usize>],
    ) -> Result<Vec<AbbreviationEdit>, String> {
        let view = self
            .shared
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if view.rules.applicable.is_empty() && view.deferred.is_empty() {
            return Ok(Vec::new());
        }
        let ranges = command_ranges(input, view.prefix.as_deref())?;
        let mut edits = Vec::new();
        let mut bytes = input.len() + usize::from(!submitted);
        for range in ranges {
            if (!submitted && range.end != cursor)
                || excluded
                    .iter()
                    .any(|span| span.start < range.end && range.start < span.end)
            {
                continue;
            }
            let name = &input[range.clone()];
            if view.deferred.contains(name) {
                return Err(format!(
                    "{name}: abbreviation is not confirmed for this draft; clear and retry, or use Ctrl+Space to keep it literal"
                ));
            }
            let Some(definition) = view.rules.definitions.get(name) else {
                continue;
            };
            bytes = bytes - range.len() + definition.expansion.len();
            if bytes > super::MAX_BYTES || excluded.len() + edits.len() >= super::MAX_REPLACEMENTS {
                return Err("abbreviation replacement limit; draft unchanged".into());
            }
            edits.push(AbbreviationEdit {
                range,
                replacement: definition.expansion.clone(),
            });
        }
        Ok(edits)
    }
}

fn publish(shared: &Shared, serial: u64, result: Result<Registry, String>) {
    let mut notify = false;
    if let Ok(mut state) = shared.state.lock()
        && state.serial == serial
    {
        let registry = match result {
            Ok(registry) => registry,
            Err(error) => {
                eprintln!("nosh abbreviations: {error}");
                state.error = Some(error.clone());
                notify = true;
                Registry::unavailable(&state.config, &error)
            }
        };
        let next = registry.approved(serial);
        notify |= if shared.frozen.load(Ordering::Acquire) {
            state
                .approved
                .definitions
                .iter()
                .any(|(name, rule)| next.definitions.get(name) != Some(rule))
        } else {
            state.approved != next
        };
        state.ready = Some((serial, registry));
    }
    if notify {
        (shared.notify)();
    }
}

fn supervise(shared: Arc<Shared>, launcher: WorkerCommand) {
    let mut worker: Option<Worker> = None;
    let mut active = None;
    let mut failures = 0u8;
    while !shared.stopped.load(Ordering::Acquire) {
        if let Some(child) = &mut worker {
            #[cfg(test)]
            child.hold_reaping(shared.hold_reaping.load(Ordering::Acquire));
            if child.stopping {
                match child.reaped() {
                    Ok(true) => {
                        worker = None;
                        *shared.child.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        if let Some(serial) = active.take() {
                            publish(&shared, serial, Err(error.to_string()));
                        }
                    }
                }
            } else if let Some(serial) = active {
                match child.poll() {
                    Ok(Some(Response::Abbreviations {
                        serial: received,
                        registry,
                    })) if received == serial => {
                        match child.residual_children() {
                            Ok(children) if children.is_empty() => {
                                failures = 0;
                                publish(&shared, serial, Ok(registry))
                            }
                            result => {
                                failures = failures.saturating_add(1);
                                let error = result.err().map_or_else(
                                    || "capability probe left subprocesses running".into(),
                                    |error| error.to_string(),
                                );
                                publish(&shared, serial, Err(error));
                                if let Err(error) = child.stop() {
                                    eprintln!("nosh abbreviation cleanup: {error}");
                                }
                            }
                        }
                        active = None;
                    }
                    Ok(None) => {}
                    other => {
                        let error = match other {
                            Err(error) => error.to_string(),
                            Ok(Some(Response::Failed(error))) => error,
                            _ => "invalid abbreviation worker response".into(),
                        };
                        failures = failures.saturating_add(1);
                        publish(&shared, serial, Err(error));
                        active = None;
                        if let Err(error) = child.stop() {
                            eprintln!("nosh abbreviation cleanup: {error}");
                        }
                    }
                }
            }
        }
        if active.is_none() && worker.as_ref().is_none_or(|worker| !worker.stopping) {
            let pending = shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending
                .take();
            if let Some(probe) = pending {
                let serial = probe.serial;
                if failures >= 2 {
                    publish(&shared, serial, Err("capability checks paused after repeated failures; restart the session to retry".into()));
                } else {
                    let result = (|| {
                        if worker.is_none() {
                            worker = Some(Worker::spawn(&launcher, Kind::Abbreviations)?);
                            #[cfg(test)]
                            shared.spawns.fetch_add(1, Ordering::Relaxed);
                            *shared.child.lock().map_err(|_| {
                                std::io::Error::other("worker registry unavailable")
                            })? = worker.as_ref().map(Worker::handle);
                        }
                        worker
                            .as_mut()
                            .expect("worker initialized")
                            .start(&Request::Abbreviations(Box::new(probe)))
                    })();
                    match result {
                        Ok(()) => active = Some(serial),
                        Err(error) => {
                            failures = failures.saturating_add(1);
                            publish(&shared, serial, Err(error.to_string()));
                            if let Some(worker) = &mut worker
                                && let Err(error) = worker.stop()
                            {
                                eprintln!("nosh abbreviation cleanup: {error}");
                            }
                        }
                    }
                }
            }
        }
        let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if shared.stopped.load(Ordering::Acquire) {
            break;
        }
        let busy = active.is_some() || worker.as_ref().is_some_and(|worker| worker.stopping);
        let interval = if busy {
            Duration::from_millis(5)
        } else {
            Duration::from_secs(60)
        };
        if state.pending.is_none() || busy {
            drop(shared.wake.wait_timeout(state, interval));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abbreviations::Entry;

    fn registry(expansion: &str, available: bool) -> Registry {
        Registry {
            entries: vec![Entry {
                name: "gst".into(),
                expansion: expansion.into(),
                available,
                source: "user".into(),
                reason: "fixture".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn abbreviation_frozen_draft_never_switches_to_a_new_expansion() {
        let mut session = Session::new(None, Arc::new(|| {}));
        {
            let mut state = session.shared.state.lock().unwrap();
            state.serial = 1;
            state.approved = registry("git status", true).approved(1);
            session.update_view(&state);
        }
        assert_eq!(session.expansion("gst", 1).as_deref(), Some("git status"));
        assert!(session.expansion("gst", 2).is_none());
        session.input_changed("gst", InputIntent::Typed);
        session.shared.state.lock().unwrap().ready =
            Some((1, registry("git status --short", true)));
        session.refresh();
        assert!(session.approved().applicable.is_empty());
        assert!(session.expansion("gst", 1).is_none());
        assert!(!session.accepts("gst", 1, "git status"));
        session.input_changed("", InputIntent::Typed);
        session.shared.state.lock().unwrap().ready =
            Some((1, registry("git status --short", true)));
        session.refresh();
        assert_eq!(
            session.approved().definitions["gst"].expansion,
            "git status --short"
        );
    }

    #[test]
    fn rapid_new_prompts_keep_the_confirmed_cache_until_its_original_expiry() {
        let (_directory, shell) = crate::abbreviations::tests::fixture();
        let context = shell
            .input_context(&crate::TriggerConfig::default(), &Default::default())
            .unwrap();
        let config = crate::abbreviations::tests::probe_config();
        let registry = super::super::check(&Probe {
            serial: 1,
            config: config.clone(),
            context: context.clone(),
        });
        let checked = Instant::now();
        let mut session = Session::new(None, Arc::new(|| {}));
        session.available = true;
        {
            let mut state = session.shared.state.lock().unwrap();
            state.config = config.clone();
            state.context = Some(context);
            state.registry = Arc::new(registry);
            state.serial = 1;
            state.checked = Some(checked);
            state.approved = state.registry.approved(1);
            session.update_view(&state);
        }
        for _ in 0..4 {
            session.begin_prompt(&shell, config.clone(), &crate::TriggerConfig::default());
            assert!(session.approved().applicable.contains("la"));
            assert_eq!(session.shared.state.lock().unwrap().checked, Some(checked));
        }
        let disabled = Config {
            probe_commands: false,
            ..config
        };
        session.begin_prompt(&shell, disabled, &crate::TriggerConfig::default());
        assert!(session.approved().applicable.is_empty());
        assert!(session.shared.state.lock().unwrap().checked.is_none());
    }

    #[test]
    fn clearing_a_draft_can_use_the_already_consumed_ready_registry() {
        let mut session = Session::new(None, Arc::new(|| {}));
        {
            let mut state = session.shared.state.lock().unwrap();
            state.serial = 1;
            state.approved = registry("git status", true).approved(1);
            session.update_view(&state);
        }
        session.input_changed("gst", InputIntent::Typed);
        session.shared.state.lock().unwrap().ready =
            Some((1, registry("git status --short", true)));
        session.refresh();
        assert!(session.approved().applicable.is_empty());
        session.input_changed("", InputIntent::Typed);
        assert_eq!(
            session.approved().definitions["gst"].expansion,
            "git status --short"
        );
    }

    #[test]
    fn abbreviation_expansion_is_exact_nonrecursive_and_preserves_sides() {
        let mut session = Session::new(None, Arc::new(|| {}));
        session.shared.state.lock().unwrap().approved = registry("git status", true).approved(1);
        session.update_view(&session.shared.state.lock().unwrap());
        let input = "echo 中文;gst | cat";
        let cursor = input.find(" |").unwrap();
        let edits = session.expand(input, cursor, false, &[]).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(&input[edits[0].range.clone()], "gst");
        assert_eq!(edits[0].replacement, "git status");
        assert!(session.expand("gts", 3, true, &[]).unwrap().is_empty());
        let excluded = 0..3;
        assert!(
            session
                .expand("gst", 3, true, std::slice::from_ref(&excluded))
                .unwrap()
                .is_empty()
        );
    }

    fn launcher() -> WorkerCommand {
        WorkerCommand {
            program: std::env::current_exe().unwrap(),
            args: vec![
                "--exact".into(),
                "abbreviations::service::tests::abbreviation_worker_probe".into(),
                "--nocapture".into(),
            ],
        }
    }

    #[test]
    fn abbreviation_worker_probe() {
        if let Some(code) = crate::input_assist::run_worker_from_env() {
            std::process::exit(code);
        }
    }

    #[test]
    fn cold_probe_has_bounded_startup_allowance_and_warm_probe_keeps_normal_deadline() {
        let (_directory, shell) = crate::abbreviations::tests::fixture();
        let probe = Probe {
            serial: 1,
            config: crate::abbreviations::tests::probe_config(),
            context: shell
                .input_context(&crate::TriggerConfig::default(), &Default::default())
                .unwrap(),
        };
        let mut worker = Worker::spawn(&launcher(), Kind::Abbreviations).unwrap();
        worker
            .start(&Request::Abbreviations(Box::new(probe.clone())))
            .unwrap();
        assert_eq!(worker.timeout, crate::input_assist::INDEX_TIMEOUT * 2);
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.poll().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        worker
            .start(&Request::Abbreviations(Box::new(probe)))
            .unwrap();
        assert_eq!(worker.timeout, crate::input_assist::INDEX_TIMEOUT);
    }

    #[test]
    fn successful_probe_resets_consecutive_failures_before_later_transient_failures() {
        let (directory, shell) = crate::abbreviations::tests::fixture();
        let git = directory.path().join("git");
        let session = Session::new(Some(launcher()), Arc::new(|| {}));
        for fail in [true, false, true, false] {
            std::fs::write(
                &git,
                if fail {
                    "#!/bin/sh\nexec /bin/sleep 60\n"
                } else {
                    "#!/bin/sh\nprintf 'git version 2.40.0\\n'\n"
                },
            )
            .unwrap();
            session.begin_prompt(
                &shell,
                crate::abbreviations::tests::probe_config(),
                &crate::TriggerConfig::default(),
            );
            let deadline = Instant::now() + Duration::from_secs(6);
            loop {
                session.refresh();
                let state = session.shared.state.lock().unwrap();
                let ready = if fail {
                    state
                        .error
                        .as_ref()
                        .is_some_and(|error| error.contains("deadline"))
                } else {
                    state
                        .registry
                        .entries
                        .iter()
                        .any(|entry| entry.name == "gst" && entry.available)
                        && state.error.is_none()
                };
                drop(state);
                if ready {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "probe did not recover after a clean response"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            if fail {
                while session.shared.child.lock().unwrap().is_some() {
                    assert!(
                        Instant::now() < deadline,
                        "failed worker did not release its slot"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    #[test]
    fn abbreviation_unreaped_worker_holds_its_slot_and_latest_queue_is_bounded() {
        let (directory, mut shell) = crate::abbreviations::tests::fixture();
        std::fs::write(
            directory.path().join("git"),
            "#!/bin/sh\nexec /bin/sleep 60\n",
        )
        .unwrap();
        let session = Session::new(Some(launcher()), Arc::new(|| {}));
        session.shared.hold_reaping.store(true, Ordering::Release);
        session.begin_prompt(
            &shell,
            crate::abbreviations::tests::probe_config(),
            &crate::TriggerConfig::default(),
        );
        let started = Instant::now();
        while session.shared.state.lock().unwrap().error.is_none() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(session.shared.spawns.load(Ordering::Acquire), 1);
        for index in 0..20 {
            shell.run_user_line(&format!("alias latest{index}='echo unchanged'"));
            session.begin_prompt(
                &shell,
                crate::abbreviations::tests::probe_config(),
                &crate::TriggerConfig::default(),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(session.shared.spawns.load(Ordering::Acquire), 1);
        let state = session.shared.state.lock().unwrap();
        assert_eq!(state.pending.as_ref().unwrap().serial, state.serial);
        assert!(session.shared.child.lock().unwrap().is_some());
    }

    #[test]
    fn abbreviation_expansion_freeze_survives_a_busy_publication_lock() {
        let mut session = Session::new(None, Arc::new(|| {}));
        let other = session.clone();
        let _locked = other.shared.state.lock().unwrap();
        session.input_changed("gst", InputIntent::Typed);
        assert!(session.shared.frozen.load(Ordering::Acquire));
    }

    #[test]
    fn abbreviation_unchanged_results_do_not_interrupt_completion() {
        let notifications = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let received = notifications.clone();
        let session = Session::new(
            None,
            Arc::new(move || {
                received.fetch_add(1, Ordering::Relaxed);
            }),
        );
        {
            let mut state = session.shared.state.lock().unwrap();
            state.serial = 1;
            state.approved = registry("git status", true).approved(1);
        }
        publish(&session.shared, 1, Ok(registry("git status", true)));
        assert_eq!(notifications.load(Ordering::Relaxed), 0);
        publish(&session.shared, 1, Ok(registry("git status", false)));
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn abbreviation_replacement_limit_rejects_the_entire_transaction() {
        let mut session = Session::new(None, Arc::new(|| {}));
        session.shared.state.lock().unwrap().approved = registry("git status", true).approved(1);
        session.update_view(&session.shared.state.lock().unwrap());
        let input = "gst;".repeat(super::super::MAX_REPLACEMENTS + 1);
        assert!(session.expand(&input, input.len(), true, &[]).is_err());
    }

    #[test]
    fn pending_rules_are_inert_and_confirmed_rules_respect_draft_freezing() {
        let mut session = Session::new(None, Arc::new(|| {}));
        {
            let mut state = session.shared.state.lock().unwrap();
            state.registry = Arc::new(Registry::pending(&Config::default()));
            session.update_view(&state);
        }
        assert!(session.expand("gst", 3, true, &[]).unwrap().is_empty());
        assert!(session.expand("echo gst", 8, true, &[]).unwrap().is_empty());
        {
            let mut state = session.shared.state.lock().unwrap();
            state.registry = Arc::new(registry("git status", true));
            session.update_view(&state);
        }
        assert!(session.expand("gst", 3, true, &[]).is_err());
        session.shared.state.lock().unwrap().approved = registry("git status", true).approved(1);
        session.update_view(&session.shared.state.lock().unwrap());
        assert_eq!(
            session.expand("gst", 3, true, &[]).unwrap()[0].replacement,
            "git status"
        );
    }

    #[test]
    fn editing_does_not_depend_on_the_probe_publication_lock() {
        let mut session = Session::new(None, Arc::new(|| {}));
        session.shared.state.lock().unwrap().approved = registry("git status", true).approved(1);
        session.update_view(&session.shared.state.lock().unwrap());
        let background = session.clone();
        let _held = background.shared.state.lock().unwrap();
        assert!(session.expand("exit 0", 6, true, &[]).unwrap().is_empty());
        assert!(session.accepts("gst", 1, "git status"));
        assert!(!session.accepts("gst", 2, "git status"));
        assert!(!session.accepts("gst", 1, "git diff"));
        assert_eq!(
            session.expand("gst", 3, true, &[]).unwrap()[0].replacement,
            "git status"
        );
        session.error("explicit editing failure");
        assert_eq!(
            session.take_error().as_deref(),
            Some("explicit editing failure")
        );
    }

    #[test]
    fn pending_nonoverride_rule_does_not_block_an_unhashed_executable() {
        let (directory, shell) = crate::abbreviations::tests::fixture();
        let program = directory.path().join("gst");
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let context = shell
            .input_context(&crate::TriggerConfig::default(), &Default::default())
            .unwrap();
        assert!(!context.hashed_commands.contains_key("gst"));
        let mut session = Session::new(None, Arc::new(|| {}));
        {
            let mut state = session.shared.state.lock().unwrap();
            state.context = Some(context);
            state.registry = Arc::new(Registry::pending(&Config::default()));
            session.update_view(&state);
        }
        assert!(session.expand("gst", 3, true, &[]).unwrap().is_empty());
        assert!(matches!(shell.resolve("gst"), crate::Resolution::File(path) if path == program));
    }
}
