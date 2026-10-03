use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::types::*;
use crate::input_assist::worker::{ChildHandle, Kind, Worker, kill_child};
use crate::input_assist::{Request, Response, WorkerCommand};

#[derive(Default)]
struct Mailbox {
    latest: Option<(Query, Instant)>,
    cancel: bool,
}

struct Prepared {
    version: u64,
    snapshot: Option<Snapshot>,
}

#[derive(Default)]
struct Publication {
    answer: Option<Arc<Answer>>,
    serial: u64,
    ongoing: bool,
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    prepared: Mutex<Option<Prepared>>,
    publication: Mutex<Publication>,
    children: Mutex<[Option<ChildHandle>; 2]>,
    wake: Condvar,
    epoch: AtomicU64,
    session: AtomicU64,
    snapshot_session: AtomicU64,
    stopped: AtomicBool,
    repaint: Arc<dyn Fn() + Send + Sync>,
    #[cfg(test)]
    turns: AtomicU64,
    #[cfg(test)]
    requests: [AtomicU64; 2],
    #[cfg(test)]
    installs: [AtomicU64; 2],
    #[cfg(test)]
    spawns: [AtomicU64; 2],
}

struct Lifetime(Arc<Shared>);

impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.stopped.store(true, Ordering::Release);
        if let Ok(children) = self.0.children.try_lock() {
            for child in children.iter().flatten() {
                // The supervisor reports cleanup faults and retains the slot.
                let _ = kill_child(child);
            }
        }
        self.0.wake.notify_one();
    }
}

#[derive(Clone)]
pub(super) struct Service {
    shared: Arc<Shared>,
    _lifetime: Arc<Lifetime>,
    available: bool,
}

impl Service {
    pub fn new(launcher: Option<WorkerCommand>, repaint: Arc<dyn Fn() + Send + Sync>) -> Self {
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox::default()),
            prepared: Mutex::new(None),
            publication: Mutex::new(Publication::default()),
            children: Mutex::new([None, None]),
            wake: Condvar::new(),
            epoch: AtomicU64::new(0),
            session: AtomicU64::new(0),
            snapshot_session: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            repaint,
            #[cfg(test)]
            turns: AtomicU64::new(0),
            #[cfg(test)]
            requests: std::array::from_fn(|_| AtomicU64::new(0)),
            #[cfg(test)]
            installs: std::array::from_fn(|_| AtomicU64::new(0)),
            #[cfg(test)]
            spawns: std::array::from_fn(|_| AtomicU64::new(0)),
        });
        let mut available = launcher.is_some();
        if let Some(launcher) = launcher {
            let supervisor = shared.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("nosh-completion".into())
                .spawn(move || supervise(supervisor, launcher))
            {
                eprintln!("nosh completion supervisor: {error}");
                available = false;
            }
        }
        Self {
            _lifetime: Arc::new(Lifetime(shared.clone())),
            shared,
            available,
        }
    }

    pub fn prepare(&self, snapshot: Result<Snapshot, String>) {
        self.shared.epoch.fetch_add(1, Ordering::AcqRel);
        let snapshot = match snapshot {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                eprintln!("nosh completion: {error}");
                None
            }
        };
        // Prompt snapshots have one independent coalescing slot. The supervisor
        // only takes it; no worker, IPC, or process work occurs under this lock.
        let previous = {
            let mut prepared = self
                .shared
                .prepared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let version = self.shared.session.fetch_add(1, Ordering::AcqRel) + 1;
            let ready = if snapshot.is_some() { version } else { 0 };
            let previous = prepared.replace(Prepared { version, snapshot });
            self.shared.snapshot_session.store(ready, Ordering::Release);
            previous
        };
        drop(previous);
        if let Ok(mut mailbox) = self.shared.mailbox.try_lock() {
            mailbox.latest = None;
            mailbox.cancel = true;
        }
        self.shared.wake.notify_one();
    }

    pub fn request(&self, text: &str, cursor: usize, trigger: Trigger) -> Query {
        let query = Query {
            text: text.into(),
            cursor,
            session: self.shared.session.load(Ordering::Acquire),
            epoch: self.shared.epoch.fetch_add(1, Ordering::AcqRel) + 1,
            trigger,
        };
        let result = if !self.available {
            Err("completion worker entry point is unavailable".into())
        } else if text.len() > MAX_INPUT || !text.is_char_boundary(cursor) {
            Err("completion input/cursor limit".into())
        } else if query.session == 0
            || self.shared.snapshot_session.load(Ordering::Acquire) != query.session
        {
            Err("completion session snapshot unavailable".into())
        } else {
            match self.shared.mailbox.try_lock() {
                Ok(mut mailbox) => {
                    mailbox.latest = Some((query.clone(), Instant::now()));
                    self.shared.wake.notify_one();
                    Ok(())
                }
                Err(_) => Err("completion request mailbox unavailable".into()),
            }
        };
        if let Err(error) = result {
            self.shared.publish(Answer {
                query: query.clone(),
                candidates: Vec::new(),
                state: State::Unavailable(error),
            });
        }
        query
    }

    pub fn cancel(&self) {
        self.shared.epoch.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut mailbox) = self.shared.mailbox.try_lock() {
            mailbox.latest = None;
            mailbox.cancel = true;
        }
        self.shared.wake.notify_one();
    }

    pub fn session(&self) -> u64 {
        self.shared.session.load(Ordering::Acquire)
    }

    pub fn result(&self, query: &Query) -> Option<(u64, Arc<Answer>, bool)> {
        if !self.shared.current(query) {
            return None;
        }
        let publication = self.shared.publication.try_lock().ok()?;
        let answer = publication
            .answer
            .as_ref()
            .filter(|answer| answer.query == *query)?;
        self.shared
            .current(query)
            .then(|| (publication.serial, answer.clone(), publication.ongoing))
    }
}

impl Shared {
    fn current(&self, query: &Query) -> bool {
        !self.stopped.load(Ordering::Acquire)
            && self.epoch.load(Ordering::Acquire) == query.epoch
            && self.session.load(Ordering::Acquire) == query.session
    }

    fn publish(&self, answer: Answer) {
        self.publish_with(answer, false);
    }

    fn publish_with(&self, answer: Answer, ongoing: bool) {
        if !self.current(&answer.query) {
            return;
        }
        let answer = if valid_answer(&answer) {
            answer
        } else {
            Answer::failed(answer.query, "invalid or oversized completion reply")
        };
        if let Ok(mut publication) = self.publication.lock()
            && self.current(&answer.query)
        {
            let ongoing = ongoing && matches!(answer.state, State::Partial(_));
            publication.answer = Some(Arc::new(answer));
            publication.serial += 1;
            publication.ongoing = ongoing;
        }
        (self.repaint)();
    }
}

fn valid_answer(answer: &Answer) -> bool {
    answer.candidates.len() <= MAX_RESULTS
        && answer
            .candidates
            .iter()
            .map(|candidate| candidate.bytes() + candidate.display.as_ref().map_or(0, String::len))
            .sum::<usize>()
            <= MAX_SET_BYTES
        && answer.candidates.iter().all(|candidate| {
            candidate.value.len() <= MAX_WORD
                && candidate.span.start <= candidate.span.end
                && answer.query.text.get(candidate.span.clone()).is_some()
        })
}

#[derive(Default)]
struct Slot {
    worker: Option<Worker>,
    request: Option<(Query, Instant)>,
    pending: Option<(Query, Instant)>,
    installed: u64,
    failures: u8,
    failed_session: u64,
    last_query: Option<Query>,
    waiting_epoch: u64,
    cleanup_failed: bool,
    cleanup_error: Option<String>,
}

fn short_error(error: impl std::fmt::Display) -> String {
    error.to_string().chars().take(240).collect()
}

impl Slot {
    fn cleanup_failure(&mut self, shared: &Shared, error: impl std::fmt::Display) {
        let message = short_error(error);
        if !self.cleanup_failed {
            self.failures = self.failures.saturating_add(1);
            self.failed_session = shared.session.load(Ordering::Acquire);
            self.cleanup_failed = true;
        }
        if self.cleanup_error.as_ref() == Some(&message) {
            return;
        }
        eprintln!("nosh completion cleanup: {message}");
        self.cleanup_error = Some(message.clone());
        let query = self
            .pending
            .as_ref()
            .map(|(query, _)| query)
            .or(self.last_query.as_ref());
        if let Some(query) = query {
            shared.publish(Answer::failed(
                query.clone(),
                format!("completion cleanup incomplete: {message}"),
            ));
        }
    }

    fn stop(&mut self, shared: &Shared) {
        self.request = None;
        if let Some(worker) = &mut self.worker
            && !worker.stopping
            && let Err(error) = worker.stop()
        {
            self.cleanup_failure(shared, error);
        }
    }

    fn reap(&mut self, shared: &Shared, index: usize) {
        if let Some(worker) = &mut self.worker
            && worker.stopping
        {
            match worker.reaped() {
                Ok(true) => {
                    self.worker = None;
                    self.installed = 0;
                    self.cleanup_failed = false;
                    self.cleanup_error = None;
                    if let Ok(mut children) = shared.children.lock() {
                        children[index] = None;
                    }
                }
                Ok(false) => {}
                Err(error) => self.cleanup_failure(shared, error),
            }
        }
    }
}

fn fail(shared: &Shared, slot: &mut Slot, query: Query, error: impl std::fmt::Display) {
    slot.failures = slot.failures.saturating_add(1);
    slot.failed_session = query.session;
    slot.cleanup_failed = true;
    slot.last_query = Some(query.clone());
    shared.publish(Answer::failed(query, short_error(error)));
    slot.stop(shared);
}

fn start(
    shared: &Shared,
    slot: &mut Slot,
    index: usize,
    launcher: &WorkerCommand,
    snapshot: &Snapshot,
) {
    let Some(item) = slot.pending.as_ref() else {
        return;
    };
    let query = item.0.clone();
    if !shared.current(&query) {
        slot.pending = None;
        return;
    }
    if slot.request.is_some()
        || slot
            .worker
            .as_ref()
            .is_some_and(|worker| worker.busy && !worker.stopping)
    {
        return;
    }
    if slot.worker.as_ref().is_some_and(|worker| worker.stopping) {
        let grace = if index == 0 {
            crate::input_assist::LOOKUP_TIMEOUT
        } else {
            crate::input_assist::INDEX_TIMEOUT
        };
        if slot.waiting_epoch != query.epoch && (slot.cleanup_failed || item.1.elapsed() >= grace) {
            slot.waiting_epoch = query.epoch;
            if slot.cleanup_failed {
                let message = slot.cleanup_error.as_ref().map_or_else(
                    || "previous completion task has not been reaped".into(),
                    |error| format!("previous completion task has not been reaped: {error}"),
                );
                shared.publish(Answer {
                    query,
                    candidates: Vec::new(),
                    state: State::Unavailable(message),
                });
            } else {
                slot.cleanup_failure(shared, "previous completion task has not been reaped");
            }
        }
        return;
    }
    if index == 1
        && query.trigger == Trigger::Refresh
        && item.1.elapsed() < Duration::from_millis(300)
    {
        return;
    }
    let Some(item) = slot.pending.take() else {
        return;
    };
    if query.trigger == Trigger::Refresh
        && (slot.failures >= 2 || slot.failed_session == query.session)
    {
        shared.publish(Answer {
            query,
            candidates: Vec::new(),
            state: State::Unavailable(
                "completion provider paused; explicitly request completion to retry".into(),
            ),
        });
        return;
    }
    if query.trigger == Trigger::Explicit {
        slot.failures = 0;
        slot.failed_session = 0;
    }
    if index == 1
        && let Err(error) = &snapshot.script
    {
        shared.publish(Answer {
            query,
            candidates: Vec::new(),
            state: State::Unavailable(error.clone()),
        });
        return;
    }
    if slot.worker.is_none() {
        match Worker::spawn(
            launcher,
            if index == 0 {
                Kind::CompletionNative
            } else {
                Kind::CompletionScript
            },
        ) {
            Ok(worker) => {
                #[cfg(test)]
                shared.spawns[index].fetch_add(1, Ordering::Relaxed);
                slot.worker = Some(worker);
                slot.installed = 0;
                slot.cleanup_failed = false;
                slot.cleanup_error = None;
                if let Ok(mut children) = shared.children.lock() {
                    children[index] = slot.worker.as_ref().map(Worker::handle);
                } else {
                    fail(shared, slot, query, "completion child registry unavailable");
                    return;
                }
            }
            Err(error) => {
                fail(shared, slot, query, error);
                return;
            }
        }
    }
    let install = if slot.installed != query.session {
        if index == 0 {
            Some(Install::Native(snapshot.native.clone()))
        } else {
            match &snapshot.script {
                Ok(state) => Some(Install::Script {
                    native: snapshot.native.clone(),
                    state: state.clone(),
                }),
                Err(_) => return,
            }
        }
    } else {
        None
    };
    let Some(worker) = slot.worker.as_mut() else {
        return;
    };
    #[cfg(test)]
    let installing = install.is_some();
    match worker.start(&Request::Complete {
        query: query.clone(),
        install,
    }) {
        Ok(()) => {
            #[cfg(test)]
            {
                shared.requests[index].fetch_add(1, Ordering::Relaxed);
                if installing {
                    shared.installs[index].fetch_add(1, Ordering::Relaxed);
                }
            }
            slot.installed = query.session;
            slot.last_query = Some(query);
            slot.waiting_epoch = 0;
            slot.request = Some(item);
        }
        Err(error) => fail(shared, slot, query, error),
    }
}

fn supervise(shared: Arc<Shared>, launcher: WorkerCommand) {
    let mut slots = [Slot::default(), Slot::default()];
    let mut snapshot = None;
    let mut snapshot_version = 0;
    loop {
        #[cfg(test)]
        shared.turns.fetch_add(1, Ordering::Relaxed);
        let prepared = shared
            .prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(prepared) = prepared
            && prepared.version == shared.session.load(Ordering::Acquire)
        {
            snapshot = prepared.snapshot;
            snapshot_version = prepared.version;
            for slot in &mut slots {
                slot.installed = 0;
            }
        }
        if snapshot_version != shared.session.load(Ordering::Acquire) {
            snapshot = None;
        }
        if let Ok(mut mailbox) = shared.mailbox.lock() {
            if mailbox.cancel {
                slots[0].pending = None;
                slots[1].pending = None;
                mailbox.cancel = false;
            }
            if let Some(item) = mailbox.latest.take() {
                slots[0].pending = Some(item);
                slots[1].pending = None;
            }
        }
        let stopped = shared.stopped.load(Ordering::Acquire);
        for slot in &mut slots {
            if stopped {
                slot.pending = None;
                slot.stop(&shared);
            } else if slot
                .request
                .as_ref()
                .is_some_and(|(query, _)| !shared.current(query))
            {
                // In particular, never reuse a cancelled or stale script's
                // potentially half-restored mutable shell.
                slot.stop(&shared);
            }
            if slot
                .pending
                .as_ref()
                .is_some_and(|(query, _)| !shared.current(query))
            {
                slot.pending = None;
            }
        }
        if stopped && slots.iter().all(|slot| slot.worker.is_none()) {
            return;
        }
        let mut registry_changed = false;
        let mut routed = None;
        for (index, slot) in slots.iter_mut().enumerate() {
            slot.reap(&shared, index);
            let Some(worker) = &mut slot.worker else {
                continue;
            };
            if worker.stopping || stopped {
                continue;
            }
            let Some((query, requested)) = slot.request.clone() else {
                continue;
            };
            match worker.poll() {
                Ok(Some(Response::Completion(Outcome::Progress { answer, .. }))) => {
                    if answer.query != query {
                        fail(&shared, slot, query, "completion progress version mismatch");
                    } else if !valid_answer(&answer) {
                        fail(
                            &shared,
                            slot,
                            query,
                            "invalid or oversized completion progress",
                        );
                    } else if !matches!(answer.state, State::Partial(_)) {
                        fail(&shared, slot, query, "non-partial completion progress");
                    } else {
                        shared.publish_with(answer, true);
                    }
                }
                Ok(Some(Response::Completion(Outcome::ScriptRequired(returned)))) if index == 0 => {
                    slot.request = None;
                    if returned != query {
                        fail(&shared, slot, query, "completion routing version mismatch");
                    } else if shared.current(&query) {
                        routed = Some((query, requested));
                    }
                }
                Ok(Some(Response::Completion(Outcome::Ready {
                    answer,
                    registry,
                    checkpoint,
                }))) => {
                    slot.request = None;
                    if answer.query != query {
                        fail(&shared, slot, query, "completion reply version mismatch");
                        continue;
                    }
                    if !shared.current(&query) {
                        if index == 1 {
                            slot.stop(&shared);
                        }
                        continue;
                    }
                    if let State::Failed(message) | State::Unavailable(message) = &answer.state {
                        fail(&shared, slot, query, message);
                        continue;
                    }
                    if !valid_answer(&answer) {
                        fail(
                            &shared,
                            slot,
                            query,
                            "invalid or oversized completion reply",
                        );
                        continue;
                    }
                    match worker.residual_children() {
                        Ok(children) if !children.is_empty() => {
                            fail(
                                &shared,
                                slot,
                                query,
                                "provider left background processes; provider paused during cleanup",
                            );
                            continue;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            fail(&shared, slot, query, error);
                            continue;
                        }
                    }
                    if index == 0 && (registry.is_some() || checkpoint.is_some()) {
                        fail(&shared, slot, query, "native worker returned script state");
                        continue;
                    }
                    if checkpoint
                        .as_ref()
                        .is_some_and(|state| state.get().len() > MAX_SNAPSHOT)
                    {
                        fail(&shared, slot, query, "completion checkpoint limit exceeded");
                        continue;
                    }
                    if let Some(current) = &mut snapshot {
                        if let Some(registry) = registry {
                            let mut native = current.native.as_ref().clone();
                            native.registry = registry;
                            if let Err(error) = crate::input_assist::write_json(
                                &native,
                                &mut std::io::sink(),
                                crate::input_assist::MAX_CONTEXT,
                            ) {
                                fail(&shared, slot, query, error);
                                continue;
                            }
                            current.native = Arc::new(native);
                            registry_changed = true;
                        }
                        if let Some(state) = checkpoint {
                            current.script = Ok(Arc::from(state));
                        }
                    }
                    shared.publish(answer);
                }
                Ok(Some(Response::Failed(error))) => fail(&shared, slot, query, error),
                Ok(Some(_)) => fail(&shared, slot, query, "unexpected completion worker reply"),
                Ok(None) => {}
                Err(error) => fail(&shared, slot, query, error),
            }
        }
        if registry_changed {
            // Install once per worker/session: retire the old native generation
            // before dispatching with the successfully loaded registry.
            slots[0].stop(&shared);
        }
        if let Some(item) = routed {
            slots[1].pending = Some(item);
        }
        if !stopped
            && snapshot_version == shared.session.load(Ordering::Acquire)
            && let Some(snapshot) = &snapshot
        {
            for (index, slot) in slots.iter_mut().enumerate() {
                start(&shared, slot, index, &launcher, snapshot);
            }
        }
        if stopped && slots.iter().all(|slot| slot.worker.is_none()) {
            return;
        }
        let busy = slots.iter().any(|slot| slot.request.is_some());
        let retiring = slots
            .iter()
            .any(|slot| slot.worker.as_ref().is_some_and(|worker| worker.stopping));
        let mut interval = if busy {
            Duration::from_millis(8)
        } else if retiring {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        if let Some((query, at)) = &slots[1].pending
            && query.trigger == Trigger::Refresh
            && !slots[1]
                .worker
                .as_ref()
                .is_some_and(|worker| worker.stopping)
        {
            interval = interval
                .min((*at + Duration::from_millis(300)).saturating_duration_since(Instant::now()));
        }
        if let Ok(mailbox) = shared.mailbox.lock()
            && mailbox.latest.is_none()
            && !mailbox.cancel
            && shared.stopped.load(Ordering::Acquire) == stopped
            && shared.session.load(Ordering::Acquire) == snapshot_version
        {
            drop(shared.wake.wait_timeout(mailbox, interval));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;

    use super::*;
    use crate::{EmbeddedShell, ShellOptions};

    static WORKERS: Mutex<()> = Mutex::new(());

    fn worker_test() -> std::sync::MutexGuard<'static, ()> {
        WORKERS.lock().unwrap_or_else(|error| error.into_inner())
    }

    struct Fixture {
        directory: tempfile::TempDir,
        shell: EmbeddedShell,
    }

    impl Fixture {
        fn new(definition: &str) -> Self {
            let directory = tempfile::Builder::new()
                .prefix("completion-lifecycle-")
                .tempdir_in(std::env::current_dir().unwrap())
                .unwrap();
            let mut shell = EmbeddedShell::new(ShellOptions {
                working_dir: Some(directory.path().into()),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(shell.run_user_line("PATH=/usr/bin:/bin").exit_code, 0);
            assert_eq!(shell.run_user_line(definition).exit_code, 0);
            Self { directory, shell }
        }

        fn snapshot(&self) -> Snapshot {
            super::super::snapshot::capture(&self.shell, true, &Default::default()).unwrap()
        }

        fn service(&self) -> Service {
            let service = Service::new(Some(launcher()), Arc::new(|| {}));
            service.prepare(Ok(self.snapshot()));
            service
        }

        fn file(&self, name: &str) -> std::path::PathBuf {
            self.directory.path().join(name)
        }
    }

    fn launcher() -> WorkerCommand {
        WorkerCommand {
            program: std::env::current_exe().unwrap(),
            args: vec![
                "--exact".into(),
                "completion::service::tests::worker_probe".into(),
                "--nocapture".into(),
            ],
        }
    }

    #[test]
    fn worker_probe() {
        if let Some(code) = crate::input_assist::run_worker_from_env() {
            std::process::exit(code);
        }
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < deadline, "completion lifecycle watchdog");
            thread::sleep(Duration::from_millis(4));
        }
    }

    fn wait_answer(
        service: &Service,
        query: &Query,
        accept: impl Fn(&Answer) -> bool,
    ) -> Arc<Answer> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let publication = service.result(query);
            if let Some((_, current, ongoing)) = &publication
                && !ongoing
                && accept(current)
            {
                return current.clone();
            }
            assert!(
                Instant::now() < deadline,
                "completion reply watchdog: {:?}; worker requests: {:?}",
                publication.map(|(_, answer, ongoing)| (answer.state.clone(), ongoing)),
                service
                    .shared
                    .requests
                    .each_ref()
                    .map(|count| count.load(Ordering::Relaxed)),
            );
            thread::sleep(Duration::from_millis(4));
        }
    }

    fn complete(service: &Service, text: &str) -> Arc<Answer> {
        let query = request(service, text, Trigger::Explicit);
        wait_answer(service, &query, |answer| answer.state == State::Complete)
    }

    fn request(service: &Service, text: &str, trigger: Trigger) -> Query {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let query = service.request(text, text.len(), trigger);
            let busy = service.result(&query).is_some_and(|(_, answer, _)| {
                matches!(&answer.state, State::Unavailable(message)
                    if message == "completion request mailbox unavailable")
            });
            if !busy {
                return query;
            }
            assert!(Instant::now() < deadline, "request admission watchdog");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn close(service: Service) {
        let shared = service.shared.clone();
        let started = Instant::now();
        drop(service);
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(100), "{elapsed:?}");
        wait_until(|| shared.children.lock().unwrap().iter().all(Option::is_none));
    }

    fn isolated_service(snapshot: &Snapshot) -> Service {
        let mut service = Service::new(None, Arc::new(|| {}));
        service.available = true;
        service.prepare(Ok(snapshot.clone()));
        service
    }

    #[test]
    fn prepare_preserves_the_current_snapshot_while_the_request_mailbox_is_busy() {
        let fixture = Fixture::new(":");
        let service = isolated_service(&fixture.snapshot());
        let previous = service.request("cd ", 3, Trigger::Explicit);
        let mailbox = service.shared.mailbox.lock().unwrap();
        let snapshot = fixture.snapshot();
        service.prepare(Ok(snapshot.clone()));
        let prepared = service.shared.prepared.lock().unwrap();
        assert_eq!(prepared.as_ref().unwrap().version, service.session());
        assert!(Arc::ptr_eq(
            &prepared.as_ref().unwrap().snapshot.as_ref().unwrap().native,
            &snapshot.native
        ));
        assert_eq!(
            service.shared.snapshot_session.load(Ordering::Acquire),
            service.session()
        );
        drop(prepared);
        drop(mailbox);
        let current = service.request("cd ", 3, Trigger::Explicit);
        assert_eq!(
            service
                .shared
                .mailbox
                .lock()
                .unwrap()
                .latest
                .as_ref()
                .unwrap()
                .0,
            current
        );
        assert!(service.result(&current).is_none());
        assert!(service.result(&previous).is_none());
        service.prepare(Err("snapshot failure".into()));
        let failed = service.request("cd ", 3, Trigger::Explicit);
        assert!(matches!(
            service.result(&failed).unwrap().1.state,
            State::Unavailable(_)
        ));
    }

    #[test]
    fn prompt_snapshots_coalesce_and_reach_the_worker_despite_mailbox_contention() {
        let _guard = worker_test();
        let fixture = Fixture::new(":");
        let mut first = fixture.snapshot();
        Arc::make_mut(&mut first.native).variables = ["FIRST".into()].into_iter().collect();
        let mut newest = first.clone();
        Arc::make_mut(&mut newest.native).variables = ["SECOND".into()].into_iter().collect();
        let service = Service::new(Some(launcher()), Arc::new(|| {}));
        let mailbox = service.shared.mailbox.lock().unwrap();
        let started = Instant::now();
        service.prepare(Ok(first));
        service.prepare(Ok(newest));
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(service.session(), 2);
        assert_eq!(service.shared.snapshot_session.load(Ordering::Acquire), 2);
        drop(mailbox);
        let answer = complete(&service, "echo $");
        assert_eq!(answer.candidates.len(), 1);
        assert_eq!(answer.candidates[0].value, "$SECOND");
        assert_eq!(service.shared.installs[0].load(Ordering::Relaxed), 1);
        assert_eq!(service.shared.spawns[0].load(Ordering::Relaxed), 1);
        close(service);
    }

    #[test]
    fn publication_rejects_stale_input_cursor_session_and_cancelled_epochs() {
        let fixture = Fixture::new(":");
        let service = isolated_service(&fixture.snapshot());
        let first = service.request("cat x tail", 5, Trigger::Explicit);
        service.shared.publish(Answer {
            query: first.clone(),
            candidates: Vec::new(),
            state: State::Complete,
        });
        assert!(service.result(&first).is_some());
        for query in [
            Query {
                text: "cat y tail".into(),
                ..first.clone()
            },
            Query {
                cursor: 4,
                ..first.clone()
            },
            Query {
                session: first.session + 1,
                ..first.clone()
            },
            Query {
                epoch: first.epoch + 1,
                ..first.clone()
            },
        ] {
            assert!(service.result(&query).is_none());
        }
        service.cancel();
        assert!(service.result(&first).is_none());
        service
            .shared
            .publish(Answer::failed(first.clone(), "late reply"));
        assert!(service.result(&first).is_none());
    }

    #[test]
    fn workers_and_session_installs_are_reused_without_per_query_spawning() {
        let _guard = worker_test();
        let fixture =
            Fixture::new("values() { COMPREPLY=(alpha beta); }; complete -F values sample");
        let service = fixture.service();
        complete(&service, "sample ");
        let children = service.shared.children.lock().unwrap().clone();
        complete(&service, "sample a");
        for (index, original) in children.iter().enumerate() {
            let current = service.shared.children.lock().unwrap()[index]
                .clone()
                .unwrap();
            assert!(Arc::ptr_eq(original.as_ref().unwrap(), &current));
            assert_eq!(service.shared.spawns[index].load(Ordering::Relaxed), 1);
            assert_eq!(service.shared.requests[index].load(Ordering::Relaxed), 2);
            assert_eq!(service.shared.installs[index].load(Ordering::Relaxed), 1);
        }
        close(service);
    }

    #[test]
    fn refresh_coalesces_to_the_newest_script_after_idle_but_native_is_immediate() {
        let _guard = worker_test();
        let fixture = Fixture::new(
            "values() { printf '%s\\n' \"$COMP_LINE\" >> calls; COMPREPLY=(newest); }; complete -F values sample",
        );
        let service = fixture.service();
        complete(&service, "cd ");
        let started = Instant::now();
        let native = request(&service, "cd ", Trigger::Refresh);
        wait_answer(&service, &native, |answer| answer.state == State::Complete);
        assert!(started.elapsed() < Duration::from_millis(280));

        let mut latest = native;
        let mut changed = Instant::now();
        for index in 0..40 {
            let text = format!("sample value{index}");
            changed = Instant::now();
            latest = request(&service, &text, Trigger::Refresh);
            assert!(
                service
                    .shared
                    .children
                    .lock()
                    .unwrap()
                    .iter()
                    .flatten()
                    .count()
                    <= 2
            );
            thread::sleep(Duration::from_millis(2));
        }
        thread::sleep(Duration::from_millis(200));
        assert!(!fixture.file("calls").exists());
        wait_answer(&service, &latest, |answer| answer.state == State::Complete);
        assert!(changed.elapsed() >= Duration::from_millis(300));
        assert_eq!(
            fs::read_to_string(fixture.file("calls")).unwrap(),
            "sample value39\n"
        );
        assert_eq!(service.shared.requests[1].load(Ordering::Relaxed), 1);
        assert_eq!(service.shared.spawns[1].load(Ordering::Relaxed), 1);
        close(service);
    }

    #[test]
    fn builtin_loop_and_external_sleep_cancellation_do_not_wait_on_the_editor() {
        let _guard = worker_test();
        for (body, external) in [("while :; do :; done", false), ("/bin/sleep 30", true)] {
            let fixture = Fixture::new(&format!(
                "slow() {{ printf started > started; {body}; }}; complete -F slow sample"
            ));
            let service = fixture.service();
            let query = request(&service, "sample ", Trigger::Explicit);
            wait_until(|| fixture.file("started").exists());
            let mut helpers = Vec::new();
            if external {
                wait_until(|| {
                    let children = service.shared.children.lock().unwrap();
                    let Some(child) = &children[1] else {
                        return false;
                    };
                    let owner = child.lock().unwrap();
                    let marker = owner.marker().unwrap();
                    helpers = crate::procs::owned_procs(owner.id() as i32, marker)
                        .unwrap()
                        .into_iter()
                        .filter_map(|process| {
                            crate::procs::TaggedProcess::open_in_session(
                                process,
                                marker,
                                owner.id() as i32,
                            )
                            .unwrap()
                        })
                        .collect();
                    !helpers.is_empty()
                });
            }
            let started = Instant::now();
            service.cancel();
            let elapsed = started.elapsed();
            assert!(elapsed < Duration::from_millis(100), "{elapsed:?}");
            assert!(service.result(&query).is_none());
            wait_until(|| service.shared.children.lock().unwrap()[1].is_none());
            assert!(helpers.iter().all(|helper| helper.gone().unwrap()));
            eprintln!("completion cancellation external={external}: {elapsed:?}");
            close(service);
        }
    }

    #[test]
    fn script_timeout_is_a_fault_and_does_not_automatically_restart_the_same_snapshot() {
        let _guard = worker_test();
        let fixture = Fixture::new("slow() { /bin/sleep 30; }; complete -F slow sample");
        let service = fixture.service();
        let started = Instant::now();
        let query = request(&service, "sample ", Trigger::Explicit);
        let answer = wait_answer(&service, &query, |answer| {
            matches!(answer.state, State::Failed(_))
        });
        let timeout_elapsed = started.elapsed();
        assert!(matches!(&answer.state, State::Failed(message) if message.contains("deadline")));
        assert!(timeout_elapsed < Duration::from_millis(2300));
        wait_until(|| service.shared.children.lock().unwrap()[1].is_none());
        let refresh = request(&service, "sample x", Trigger::Refresh);
        let paused = wait_answer(&service, &refresh, |answer| {
            matches!(answer.state, State::Unavailable(_))
        });
        assert!(matches!(&paused.state, State::Unavailable(message) if message.contains("paused")));
        assert_eq!(service.shared.spawns[1].load(Ordering::Relaxed), 1);
        eprintln!("completion script deadline: {timeout_elapsed:?}");
        close(service);
    }

    #[test]
    fn unreaped_slots_keep_the_latest_pending_request_and_their_registered_identity() {
        let _guard = worker_test();
        let fixture = Fixture::new(":");
        fs::write(fixture.file("target"), "").unwrap();
        let snapshot = fixture.snapshot();
        let service = isolated_service(&snapshot);
        let mut slot = Slot::default();
        let first = service.request("cat t", 5, Trigger::Explicit);
        slot.pending = Some((first.clone(), Instant::now()));
        start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
        let original = slot.worker.as_ref().unwrap().handle();
        slot.worker.as_mut().unwrap().hold_reaping(true);
        fail(&service.shared, &mut slot, first, "injected timeout");
        for index in 0..50 {
            let text = format!("cat target{index}");
            let query = service.request(&text, text.len(), Trigger::Explicit);
            slot.pending = Some((query.clone(), Instant::now()));
            slot.reap(&service.shared, 0);
            start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
            assert_eq!(slot.pending.as_ref().unwrap().0, query);
            assert!(Arc::ptr_eq(
                &original,
                &slot.worker.as_ref().unwrap().handle()
            ));
            assert!(Arc::ptr_eq(
                &original,
                service.shared.children.lock().unwrap()[0].as_ref().unwrap()
            ));
        }
        let query = service.request("cat target", 10, Trigger::Explicit);
        slot.pending = Some((query.clone(), Instant::now()));
        slot.worker.as_mut().unwrap().hold_reaping(false);
        wait_until(|| {
            slot.reap(&service.shared, 0);
            slot.worker.is_none()
        });
        start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
        assert!(slot.pending.is_none());
        assert_eq!(slot.request.as_ref().unwrap().0, query);
        assert!(!Arc::ptr_eq(
            &original,
            &slot.worker.as_ref().unwrap().handle()
        ));
        assert_eq!(service.shared.spawns[0].load(Ordering::Relaxed), 2);
        slot.stop(&service.shared);
        wait_until(|| {
            slot.reap(&service.shared, 0);
            slot.worker.is_none()
        });
    }

    #[test]
    fn automatic_recovery_is_once_on_a_new_snapshot_and_explicit_retry_resets_it() {
        let _guard = worker_test();
        let fixture = Fixture::new(":");
        let snapshot = fixture.snapshot();
        let service = isolated_service(&snapshot);
        let unavailable = WorkerCommand {
            program: fixture.file("missing-worker"),
            args: Vec::new(),
        };
        let mut slot = Slot::default();
        let first = service.request("cd ", 3, Trigger::Explicit);
        slot.pending = Some((first, Instant::now()));
        start(&service.shared, &mut slot, 0, &unavailable, &snapshot);
        assert_eq!(slot.failures, 1);
        let same = service.request("cd x", 4, Trigger::Refresh);
        slot.pending = Some((same.clone(), Instant::now()));
        start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
        assert!(slot.worker.is_none());
        assert!(matches!(
            service.result(&same).unwrap().1.state,
            State::Unavailable(_)
        ));

        service.prepare(Ok(snapshot.clone()));
        let newer = service.request("cd ", 3, Trigger::Refresh);
        slot.pending = Some((newer, Instant::now()));
        start(&service.shared, &mut slot, 0, &unavailable, &snapshot);
        assert_eq!(slot.failures, 2);
        service.prepare(Ok(snapshot.clone()));
        let paused = service.request("cd ", 3, Trigger::Refresh);
        slot.pending = Some((paused.clone(), Instant::now()));
        start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
        assert!(slot.worker.is_none());
        assert!(matches!(
            service.result(&paused).unwrap().1.state,
            State::Unavailable(_)
        ));

        let retry = service.request("cd ", 3, Trigger::Explicit);
        slot.pending = Some((retry, Instant::now()));
        start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
        assert!(slot.worker.is_some());
        assert_eq!(slot.failures, 0);
        slot.stop(&service.shared);
        wait_until(|| {
            slot.reap(&service.shared, 0);
            slot.worker.is_none()
        });
    }

    #[test]
    fn autoload_checkpoint_survives_queries_and_cancelled_mutation_without_mailbox_rollback() {
        let _guard = worker_test();
        let fixture = Fixture::new(
            "actual() { if [[ $COMP_LINE == *hang* ]]; then TRANSIENT=dirty; printf started > started; while :; do :; done; fi; COMPREPLY=(\"${TRANSIENT:-kept}\"); }; \
             autoload() { printf loaded >> loads; complete -r -D; complete -F actual sample; return 124; }; complete -D -F autoload",
        );
        let service = fixture.service();
        assert_eq!(complete(&service, "sample ").candidates[0].value, "kept");
        assert_eq!(fs::read_to_string(fixture.file("loads")).unwrap(), "loaded");
        let script_requests = service.shared.requests[1].load(Ordering::Relaxed);
        complete(&service, "cat ");
        assert_eq!(
            service.shared.requests[1].load(Ordering::Relaxed),
            script_requests
        );
        assert_eq!(service.shared.installs[0].load(Ordering::Relaxed), 2);
        assert_eq!(service.shared.spawns[0].load(Ordering::Relaxed), 2);
        assert_eq!(complete(&service, "sample k").candidates[0].value, "kept");
        assert_eq!(service.shared.installs[1].load(Ordering::Relaxed), 1);

        let stale = request(&service, "sample hang", Trigger::Explicit);
        wait_until(|| fixture.file("started").exists());
        service.cancel();
        assert!(service.result(&stale).is_none());
        wait_until(|| service.shared.children.lock().unwrap()[1].is_none());
        assert_eq!(complete(&service, "sample ").candidates[0].value, "kept");
        assert_eq!(fs::read_to_string(fixture.file("loads")).unwrap(), "loaded");
        assert_eq!(service.shared.installs[1].load(Ordering::Relaxed), 2);
        assert_eq!(service.shared.spawns[1].load(Ordering::Relaxed), 2);
        close(service);
    }

    #[test]
    fn failed_script_instances_are_discarded_before_explicit_retry() {
        let _guard = worker_test();
        let fixture = Fixture::new(
            "mutate() { complete -F missing sample; COMPREPLY=(safe); }; complete -F mutate sample",
        );
        let service = fixture.service();
        assert_eq!(complete(&service, "sample ").candidates[0].value, "safe");
        let failed = request(&service, "sample x", Trigger::Explicit);
        wait_answer(&service, &failed, |answer| {
            matches!(answer.state, State::Failed(_))
        });
        wait_until(|| service.shared.children.lock().unwrap()[1].is_none());
        assert_eq!(complete(&service, "sample ").candidates[0].value, "safe");
        assert_eq!(service.shared.spawns[1].load(Ordering::Relaxed), 2);
        close(service);
    }

    #[test]
    fn terminal_partial_results_do_not_leave_an_idle_polling_loop() {
        let _guard = worker_test();
        let fixture = Fixture::new(":");
        for index in 0..300 {
            fs::write(fixture.file(&format!("item{index:03}")), "").unwrap();
        }
        let service = fixture.service();
        let query = request(&service, "cat item", Trigger::Explicit);
        let answer = wait_answer(&service, &query, |answer| {
            matches!(answer.state, State::Partial(_))
        });
        assert_eq!(answer.candidates.len(), MAX_RESULTS);
        let turns = service.shared.turns.load(Ordering::Relaxed);
        thread::sleep(Duration::from_millis(200));
        assert!(service.shared.turns.load(Ordering::Relaxed) - turns <= 2);
        assert!(!service.result(&query).unwrap().2);
        close(service);
    }

    #[test]
    fn dropping_a_running_service_is_bounded_even_when_a_child_handle_is_busy() {
        let _guard = worker_test();
        let fixture = Fixture::new(
            "slow() { printf started > started; while :; do :; done; }; complete -F slow sample",
        );
        let service = fixture.service();
        request(&service, "sample ", Trigger::Explicit);
        wait_until(|| fixture.file("started").exists());
        let shared = service.shared.clone();
        let child = shared.children.lock().unwrap()[1].clone().unwrap();
        let owner = child.lock().unwrap();
        let started = Instant::now();
        drop(service);
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(100), "{elapsed:?}");
        assert!(shared.children.lock().unwrap()[1].is_some());
        drop(owner);
        wait_until(|| shared.children.lock().unwrap().iter().all(Option::is_none));
        eprintln!("completion drop with busy child handle: {elapsed:?}");
    }

    #[test]
    fn a_request_startup_failure_keeps_the_spawned_handle_until_verified_reaping() {
        let _guard = worker_test();
        let fixture = Fixture::new(":");
        let mut snapshot = fixture.snapshot();
        Arc::make_mut(&mut snapshot.native)
            .environment
            .insert("OVERSIZED".into(), "x".repeat(MAX_FRAME));
        let service = isolated_service(&snapshot);
        let query = service.request("cd ", 3, Trigger::Explicit);
        let mut slot = Slot {
            pending: Some((query.clone(), Instant::now())),
            ..Default::default()
        };
        start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
        let worker = slot.worker.as_mut().unwrap();
        assert!(worker.stopping);
        worker.hold_reaping(true);
        let original = worker.handle();
        assert!(Arc::ptr_eq(
            &original,
            service.shared.children.lock().unwrap()[0].as_ref().unwrap()
        ));
        assert!(matches!(
            service.result(&query).unwrap().1.state,
            State::Failed(_)
        ));
        let pending = service.request("cd x", 4, Trigger::Explicit);
        slot.pending = Some((pending.clone(), Instant::now()));
        slot.reap(&service.shared, 0);
        start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
        assert_eq!(slot.pending.as_ref().unwrap().0, pending);
        assert_eq!(service.shared.spawns[0].load(Ordering::Relaxed), 1);
        slot.worker.as_mut().unwrap().hold_reaping(false);
        wait_until(|| {
            slot.reap(&service.shared, 0);
            slot.worker.is_none()
        });
        assert!(service.shared.children.lock().unwrap()[0].is_none());
    }

    #[test]
    fn a_successful_reply_with_residual_background_processes_is_failed_and_paused() {
        let _guard = worker_test();
        let fixture = Fixture::new(
            "background() { /bin/sleep 30 & /bin/sleep 0.05; COMPREPLY=(unsafe); }; complete -F background sample",
        );
        let service = fixture.service();
        let query = request(&service, "sample ", Trigger::Explicit);
        let answer = wait_answer(&service, &query, |answer| {
            matches!(answer.state, State::Failed(_))
        });
        assert!(answer.candidates.is_empty());
        assert!(matches!(&answer.state, State::Failed(message) if !message.is_empty()));
        wait_until(|| service.shared.children.lock().unwrap()[1].is_none());
        let refresh = request(&service, "sample x", Trigger::Refresh);
        wait_answer(&service, &refresh, |answer| {
            matches!(answer.state, State::Unavailable(_))
        });
        assert_eq!(service.shared.spawns[1].load(Ordering::Relaxed), 1);
        close(service);
    }

    #[test]
    fn deferred_background_jobs_are_failed_before_publication_and_reset_before_retry() {
        let _guard = worker_test();
        for body in ["/bin/sleep 30", "while :; do :; done"] {
            let fixture = Fixture::new(&format!(
                "background() {{ if [[ $COMP_LINE != *safe* ]]; then TRANSIENT=dirty; {body} & fi; \
                 COMPREPLY=(\"${{TRANSIENT:-safe}}\"); }}; complete -F background sample",
            ));
            let service = fixture.service();
            let query = request(&service, "sample ", Trigger::Explicit);
            let answer = wait_answer(&service, &query, |answer| {
                assert!(
                    answer.candidates.is_empty(),
                    "background-job candidates leaked"
                );
                matches!(answer.state, State::Failed(_))
            });
            assert!(matches!(&answer.state, State::Failed(message)
                if message.contains("created background jobs")));
            wait_until(|| service.shared.children.lock().unwrap()[1].is_none());
            let refresh = request(&service, "sample x", Trigger::Refresh);
            wait_answer(&service, &refresh, |answer| {
                matches!(answer.state, State::Unavailable(_))
            });
            assert_eq!(service.shared.spawns[1].load(Ordering::Relaxed), 1);
            let retry = complete(&service, "sample safe");
            assert_eq!(retry.candidates.len(), 1);
            assert_eq!(
                retry.candidates[0].value, "safe",
                "reuse of mutated background-job state"
            );
            assert_eq!(service.shared.spawns[1].load(Ordering::Relaxed), 2);
            close(service);
        }
    }

    #[test]
    fn healthy_retirement_waits_without_a_spurious_fault_but_unreaped_cleanup_is_bounded() {
        let _guard = worker_test();
        let fixture = Fixture::new(":");
        let snapshot = fixture.snapshot();
        let service = isolated_service(&snapshot);
        let query = service.request("cd ", 3, Trigger::Explicit);
        let mut slot = Slot {
            pending: Some((query, Instant::now())),
            ..Default::default()
        };
        start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
        slot.worker.as_mut().unwrap().hold_reaping(true);
        slot.stop(&service.shared);
        let newest = service.request("cd x", 4, Trigger::Explicit);
        slot.pending = Some((newest.clone(), Instant::now()));
        start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
        assert!(service.result(&newest).is_none());
        assert_eq!(slot.pending.as_ref().unwrap().0, newest);
        slot.pending.as_mut().unwrap().1 =
            Instant::now() - crate::input_assist::LOOKUP_TIMEOUT - Duration::from_millis(1);
        start(&service.shared, &mut slot, 0, &launcher(), &snapshot);
        assert!(matches!(
            service.result(&newest).unwrap().1.state,
            State::Failed(_)
        ));
        assert_eq!(slot.failures, 1);
        assert_eq!(service.shared.spawns[0].load(Ordering::Relaxed), 1);
        slot.worker.as_mut().unwrap().hold_reaping(false);
        wait_until(|| {
            slot.reap(&service.shared, 0);
            slot.worker.is_none()
        });
    }
}
