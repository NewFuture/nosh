use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::types::*;
use crate::input_assist::worker::{ChildHandle, Kind, Worker, kill_child};
use crate::input_assist::{Request, Response, WorkerCommand};

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
    mailbox: Mutex<Option<(Query, Instant)>>,
    prepared: Mutex<Option<Prepared>>,
    publication: Mutex<Publication>,
    child: Mutex<Option<ChildHandle>>,
    wake: Condvar,
    epoch: AtomicU64,
    session: AtomicU64,
    snapshot_session: AtomicU64,
    stopped: AtomicBool,
    repaint: Arc<dyn Fn() + Send + Sync>,
    #[cfg(test)]
    turns: AtomicU64,
    #[cfg(test)]
    requests: AtomicU64,
    #[cfg(test)]
    installs: AtomicU64,
    #[cfg(test)]
    spawns: AtomicU64,
}

struct Lifetime(Arc<Shared>);

impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.stopped.store(true, Ordering::Release);
        if let Ok(child) = self.0.child.try_lock()
            && let Some(child) = child.as_ref()
        {
            // The supervisor reports cleanup faults and retains the slot.
            let _ = kill_child(child);
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
            mailbox: Mutex::new(None),
            prepared: Mutex::new(None),
            publication: Mutex::new(Publication::default()),
            child: Mutex::new(None),
            wake: Condvar::new(),
            epoch: AtomicU64::new(0),
            session: AtomicU64::new(0),
            snapshot_session: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            repaint,
            #[cfg(test)]
            turns: AtomicU64::new(0),
            #[cfg(test)]
            requests: AtomicU64::new(0),
            #[cfg(test)]
            installs: AtomicU64::new(0),
            #[cfg(test)]
            spawns: AtomicU64::new(0),
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
            Err("completion worker entry point is unavailable")
        } else if text.len() > MAX_INPUT || !text.is_char_boundary(cursor) {
            Err("completion input/cursor limit")
        } else if query.session == 0
            || self.shared.snapshot_session.load(Ordering::Acquire) != query.session
        {
            Err("completion session snapshot unavailable")
        } else {
            match self.shared.mailbox.try_lock() {
                Ok(mut mailbox) => {
                    *mailbox = Some((query.clone(), Instant::now()));
                    self.shared.wake.notify_one();
                    Ok(())
                }
                Err(_) => Err("completion request mailbox unavailable"),
            }
        };
        if let Err(error) = result {
            self.shared
                .publish(Answer::unavailable(query.clone(), error));
        }
        query
    }

    pub fn cancel(&self) {
        self.shared.epoch.fetch_add(1, Ordering::AcqRel);
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
        let answer = match answer.validate() {
            Ok(()) => answer,
            Err(error) => Answer::failed(answer.query, error),
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

#[derive(Default)]
struct Failure {
    count: u8,
    session: u64,
}

#[derive(Default)]
struct Slot {
    worker: Option<Worker>,
    request: Option<(Query, Instant)>,
    pending: Option<(Query, Instant)>,
    installed: u64,
    installed_script: bool,
    failures: [Failure; 2],
    executing_script: bool,
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
            let failure = &mut self.failures[usize::from(self.executing_script)];
            failure.count = failure.count.saturating_add(1);
            failure.session = shared.session.load(Ordering::Acquire);
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

    fn reap(&mut self, shared: &Shared) {
        if let Some(worker) = &mut self.worker
            && worker.stopping
        {
            match worker.reaped() {
                Ok(true) => {
                    self.worker = None;
                    self.installed = 0;
                    self.installed_script = false;
                    self.cleanup_failed = false;
                    self.cleanup_error = None;
                    if let Ok(mut child) = shared.child.lock() {
                        *child = None;
                    }
                }
                Ok(false) => {}
                Err(error) => self.cleanup_failure(shared, error),
            }
        }
    }
}

fn fail(shared: &Shared, slot: &mut Slot, query: Query, error: impl std::fmt::Display) {
    let failure = &mut slot.failures[usize::from(slot.executing_script)];
    failure.count = failure.count.saturating_add(1);
    failure.session = query.session;
    slot.cleanup_failed = true;
    slot.last_query = Some(query.clone());
    shared.publish(Answer::failed(query, short_error(error)));
    slot.stop(shared);
}

fn start(shared: &Shared, slot: &mut Slot, launcher: &WorkerCommand, snapshot: &Snapshot) {
    let Some(item) = slot.pending.as_ref() else {
        return;
    };
    let query = item.0.clone();
    if !shared.current(&query) {
        slot.pending = None;
        return;
    }
    let script = super::context::Context::parse(&query, &snapshot.native)
        .is_ok_and(|context| context.needs_execution(&snapshot.native));
    if slot.request.is_some()
        || slot
            .worker
            .as_ref()
            .is_some_and(|worker| worker.busy && !worker.stopping)
    {
        return;
    }
    if slot.worker.as_ref().is_some_and(|worker| worker.stopping) {
        let grace = if slot.executing_script {
            crate::input_assist::INDEX_TIMEOUT
        } else {
            crate::input_assist::LOOKUP_TIMEOUT
        };
        if slot.waiting_epoch != query.epoch && (slot.cleanup_failed || item.1.elapsed() >= grace) {
            slot.waiting_epoch = query.epoch;
            if slot.cleanup_failed {
                let message = slot.cleanup_error.as_ref().map_or_else(
                    || "previous completion task has not been reaped".into(),
                    |error| format!("previous completion task has not been reaped: {error}"),
                );
                shared.publish(Answer::unavailable(query, message));
            } else {
                slot.cleanup_failure(shared, "previous completion task has not been reaped");
            }
        }
        return;
    }
    if script && query.trigger == Trigger::Refresh && item.1.elapsed() < Duration::from_millis(300)
    {
        return;
    }
    let Some(item) = slot.pending.take() else {
        return;
    };
    slot.executing_script = script;
    let failure = &mut slot.failures[usize::from(script)];
    if query.trigger == Trigger::Refresh && (failure.count >= 2 || failure.session == query.session)
    {
        shared.publish(Answer::unavailable(
            query,
            "completion provider paused; explicitly request completion to retry",
        ));
        return;
    }
    if query.trigger == Trigger::Explicit {
        *failure = Failure::default();
    }
    if script && let Some(Err(error)) = &snapshot.script {
        shared.publish(Answer::unavailable(query, error.clone()));
        return;
    }
    if slot.worker.is_none() {
        match Worker::spawn(launcher, Kind::Completion) {
            Ok(worker) => {
                #[cfg(test)]
                shared.spawns.fetch_add(1, Ordering::Relaxed);
                slot.worker = Some(worker);
                slot.installed = 0;
                slot.cleanup_failed = false;
                slot.cleanup_error = None;
                if let Ok(mut child) = shared.child.lock() {
                    *child = slot.worker.as_ref().map(Worker::handle);
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
    if slot.installed != query.session {
        slot.installed_script = false;
    }
    let install = if slot.installed != query.session || (script && !slot.installed_script) {
        Some(Snapshot {
            native: snapshot.native.clone(),
            script: script.then(|| snapshot.script.clone()).flatten(),
        })
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
                shared.requests.fetch_add(1, Ordering::Relaxed);
                if installing {
                    shared.installs.fetch_add(1, Ordering::Relaxed);
                }
            }
            slot.installed = query.session;
            slot.installed_script |= script;
            slot.last_query = Some(query);
            slot.waiting_epoch = 0;
            slot.request = Some(item);
        }
        Err(error) => fail(shared, slot, query, error),
    }
}

fn poll(shared: &Shared, slot: &mut Slot, snapshot: &mut Option<Snapshot>) {
    let Some(worker) = &mut slot.worker else {
        return;
    };
    if worker.stopping {
        return;
    }
    let Some((query, _)) = slot.request.clone() else {
        return;
    };
    match worker.poll() {
        Ok(Some(Response::Completion(Outcome::Progress { answer, .. }))) => {
            if answer.query != query {
                fail(shared, slot, query, "completion progress version mismatch");
            } else if let Err(error) = answer.validate() {
                fail(shared, slot, query, error);
            } else if !matches!(answer.state, State::Partial(_)) {
                fail(shared, slot, query, "non-partial completion progress");
            } else {
                shared.publish_with(answer, true);
            }
        }
        Ok(Some(Response::Completion(Outcome::Ready {
            answer,
            snapshot: updated,
        }))) => {
            slot.request = None;
            if answer.query != query {
                fail(shared, slot, query, "completion reply version mismatch");
                return;
            }
            if !shared.current(&query) {
                slot.stop(shared);
                return;
            }
            if let State::Failed(message) | State::Unavailable(message) = &answer.state {
                fail(shared, slot, query, message);
                return;
            }
            if let Err(error) = answer.validate() {
                fail(shared, slot, query, error);
                return;
            }
            match worker.residual_children() {
                Ok(children) if !children.is_empty() => {
                    fail(
                        shared,
                        slot,
                        query,
                        "provider left background processes; provider paused during cleanup",
                    );
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    fail(shared, slot, query, error);
                    return;
                }
            }
            if let Some(updated) = updated {
                if let Err(error) = updated.validate() {
                    fail(shared, slot, query, error);
                    return;
                }
                if shared.current(&query) {
                    *snapshot = Some(updated);
                }
            }
            shared.publish(answer);
        }
        Ok(Some(Response::Failed(error))) => fail(shared, slot, query, error),
        Ok(Some(_)) => fail(shared, slot, query, "unexpected completion worker reply"),
        Ok(None) => {}
        Err(error) => fail(shared, slot, query, error),
    }
}

fn supervise(shared: Arc<Shared>, launcher: WorkerCommand) {
    let mut slot = Slot::default();
    let mut snapshot = None;
    let mut snapshot_version = 0;
    loop {
        let epoch = shared.epoch.load(Ordering::Acquire);
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
            slot.installed = 0;
        }
        if snapshot_version != shared.session.load(Ordering::Acquire) {
            snapshot = None;
        }
        if let Ok(mut mailbox) = shared.mailbox.lock()
            && let Some(item) = mailbox.take()
        {
            slot.pending = Some(item);
        }
        let stopped = shared.stopped.load(Ordering::Acquire);
        if stopped {
            slot.pending = None;
            slot.stop(&shared);
        } else if slot
            .request
            .as_ref()
            .is_some_and(|(query, _)| !shared.current(query))
        {
            // Cancelled scripts may contain half-restored mutable shell state.
            slot.stop(&shared);
        }
        if slot
            .pending
            .as_ref()
            .is_some_and(|(query, _)| !shared.current(query))
        {
            slot.pending = None;
        }
        slot.reap(&shared);
        if !stopped {
            poll(&shared, &mut slot, &mut snapshot);
            if snapshot_version == shared.session.load(Ordering::Acquire)
                && let Some(snapshot) = &snapshot
            {
                start(&shared, &mut slot, &launcher, snapshot);
            }
        }
        if stopped && slot.worker.is_none() {
            return;
        }
        let retiring = slot.worker.as_ref().is_some_and(|worker| worker.stopping);
        let mut interval = if slot.request.is_some() {
            Duration::from_millis(8)
        } else if retiring {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        if !retiring
            && let Some((query, at)) = &slot.pending
            && query.trigger == Trigger::Refresh
            && snapshot.as_ref().is_some_and(|snapshot| {
                super::context::Context::parse(query, &snapshot.native)
                    .is_ok_and(|context| context.needs_execution(&snapshot.native))
            })
        {
            interval = interval
                .min((*at + Duration::from_millis(300)).saturating_duration_since(Instant::now()));
        }
        if let Ok(mailbox) = shared.mailbox.lock()
            && mailbox.is_none()
            && shared.epoch.load(Ordering::Acquire) == epoch
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
                service.shared.requests.load(Ordering::Relaxed),
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
        wait_until(|| shared.child.lock().unwrap().is_none());
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
            service.shared.mailbox.lock().unwrap().as_ref().unwrap().0,
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
        assert_eq!(service.shared.installs.load(Ordering::Relaxed), 1);
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 1);
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
        let original = service.shared.child.lock().unwrap().clone().unwrap();
        complete(&service, "sample a");
        let current = service.shared.child.lock().unwrap().clone().unwrap();
        assert!(Arc::ptr_eq(&original, &current));
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 1);
        assert_eq!(service.shared.requests.load(Ordering::Relaxed), 2);
        assert_eq!(service.shared.installs.load(Ordering::Relaxed), 1);
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
            assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 1);
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
        assert_eq!(service.shared.requests.load(Ordering::Relaxed), 3);
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 1);
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
                    let child = service.shared.child.lock().unwrap();
                    let Some(child) = child.as_ref() else {
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
            wait_until(|| service.shared.child.lock().unwrap().is_none());
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
        wait_until(|| service.shared.child.lock().unwrap().is_none());
        let refresh = request(&service, "sample x", Trigger::Refresh);
        let paused = wait_answer(&service, &refresh, |answer| {
            matches!(answer.state, State::Unavailable(_))
        });
        assert!(matches!(&paused.state, State::Unavailable(message) if message.contains("paused")));
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 1);
        eprintln!("completion script deadline: {timeout_elapsed:?}");
        close(service);
    }

    #[test]
    fn script_failures_do_not_pause_native_queries_in_the_shared_worker() {
        let _guard = worker_test();
        let fixture = Fixture::new("complete -F missing sample");
        fs::write(fixture.file("target"), "").unwrap();
        let service = fixture.service();
        let failed = request(&service, "sample ", Trigger::Explicit);
        wait_answer(&service, &failed, |answer| {
            matches!(answer.state, State::Failed(_))
        });
        wait_until(|| service.shared.child.lock().unwrap().is_none());
        let native = request(&service, "cat targ", Trigger::Refresh);
        let result = wait_answer(&service, &native, |answer| answer.state.is_complete());
        assert_eq!(result.candidates[0].value, "target");
        let script = request(&service, "sample x", Trigger::Refresh);
        wait_answer(&service, &script, |answer| {
            matches!(answer.state, State::Unavailable(_))
        });
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 2);
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
        start(&service.shared, &mut slot, &launcher(), &snapshot);
        let original = slot.worker.as_ref().unwrap().handle();
        slot.worker.as_mut().unwrap().hold_reaping(true);
        fail(&service.shared, &mut slot, first, "injected timeout");
        for index in 0..50 {
            let text = format!("cat target{index}");
            let query = service.request(&text, text.len(), Trigger::Explicit);
            slot.pending = Some((query.clone(), Instant::now()));
            slot.reap(&service.shared);
            start(&service.shared, &mut slot, &launcher(), &snapshot);
            assert_eq!(slot.pending.as_ref().unwrap().0, query);
            assert!(Arc::ptr_eq(
                &original,
                &slot.worker.as_ref().unwrap().handle()
            ));
            assert!(Arc::ptr_eq(
                &original,
                service.shared.child.lock().unwrap().as_ref().unwrap()
            ));
        }
        let query = service.request("cat target", 10, Trigger::Explicit);
        slot.pending = Some((query.clone(), Instant::now()));
        slot.worker.as_mut().unwrap().hold_reaping(false);
        wait_until(|| {
            slot.reap(&service.shared);
            slot.worker.is_none()
        });
        start(&service.shared, &mut slot, &launcher(), &snapshot);
        assert!(slot.pending.is_none());
        assert_eq!(slot.request.as_ref().unwrap().0, query);
        assert!(!Arc::ptr_eq(
            &original,
            &slot.worker.as_ref().unwrap().handle()
        ));
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 2);
        slot.stop(&service.shared);
        wait_until(|| {
            slot.reap(&service.shared);
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
        start(&service.shared, &mut slot, &unavailable, &snapshot);
        assert_eq!(slot.failures[0].count, 1);
        let same = service.request("cd x", 4, Trigger::Refresh);
        slot.pending = Some((same.clone(), Instant::now()));
        start(&service.shared, &mut slot, &launcher(), &snapshot);
        assert!(slot.worker.is_none());
        assert!(matches!(
            service.result(&same).unwrap().1.state,
            State::Unavailable(_)
        ));

        service.prepare(Ok(snapshot.clone()));
        let newer = service.request("cd ", 3, Trigger::Refresh);
        slot.pending = Some((newer, Instant::now()));
        start(&service.shared, &mut slot, &unavailable, &snapshot);
        assert_eq!(slot.failures[0].count, 2);
        service.prepare(Ok(snapshot.clone()));
        let paused = service.request("cd ", 3, Trigger::Refresh);
        slot.pending = Some((paused.clone(), Instant::now()));
        start(&service.shared, &mut slot, &launcher(), &snapshot);
        assert!(slot.worker.is_none());
        assert!(matches!(
            service.result(&paused).unwrap().1.state,
            State::Unavailable(_)
        ));

        let retry = service.request("cd ", 3, Trigger::Explicit);
        slot.pending = Some((retry, Instant::now()));
        start(&service.shared, &mut slot, &launcher(), &snapshot);
        assert!(slot.worker.is_some());
        assert_eq!(slot.failures[0].count, 0);
        slot.stop(&service.shared);
        wait_until(|| {
            slot.reap(&service.shared);
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
        let queries = service.shared.requests.load(Ordering::Relaxed);
        complete(&service, "cat ");
        assert_eq!(service.shared.requests.load(Ordering::Relaxed), queries + 1);
        assert_eq!(service.shared.installs.load(Ordering::Relaxed), 1);
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 1);
        assert_eq!(complete(&service, "sample k").candidates[0].value, "kept");
        assert_eq!(service.shared.installs.load(Ordering::Relaxed), 1);

        let stale = request(&service, "sample hang", Trigger::Explicit);
        wait_until(|| fixture.file("started").exists());
        service.cancel();
        assert!(service.result(&stale).is_none());
        wait_until(|| service.shared.child.lock().unwrap().is_none());
        assert_eq!(complete(&service, "sample ").candidates[0].value, "kept");
        assert_eq!(fs::read_to_string(fixture.file("loads")).unwrap(), "loaded");
        assert_eq!(service.shared.installs.load(Ordering::Relaxed), 2);
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 2);
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
        wait_until(|| service.shared.child.lock().unwrap().is_none());
        assert_eq!(complete(&service, "sample ").candidates[0].value, "safe");
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 2);
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
        let child = shared.child.lock().unwrap().clone().unwrap();
        let owner = child.lock().unwrap();
        let started = Instant::now();
        drop(service);
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(100), "{elapsed:?}");
        assert!(shared.child.lock().unwrap().is_some());
        drop(owner);
        wait_until(|| shared.child.lock().unwrap().is_none());
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
        start(&service.shared, &mut slot, &launcher(), &snapshot);
        let worker = slot.worker.as_mut().unwrap();
        assert!(worker.stopping);
        worker.hold_reaping(true);
        let original = worker.handle();
        assert!(Arc::ptr_eq(
            &original,
            service.shared.child.lock().unwrap().as_ref().unwrap()
        ));
        assert!(matches!(
            service.result(&query).unwrap().1.state,
            State::Failed(_)
        ));
        let pending = service.request("cd x", 4, Trigger::Explicit);
        slot.pending = Some((pending.clone(), Instant::now()));
        slot.reap(&service.shared);
        start(&service.shared, &mut slot, &launcher(), &snapshot);
        assert_eq!(slot.pending.as_ref().unwrap().0, pending);
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 1);
        slot.worker.as_mut().unwrap().hold_reaping(false);
        wait_until(|| {
            slot.reap(&service.shared);
            slot.worker.is_none()
        });
        assert!(service.shared.child.lock().unwrap().is_none());
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
        wait_until(|| service.shared.child.lock().unwrap().is_none());
        let refresh = request(&service, "sample x", Trigger::Refresh);
        wait_answer(&service, &refresh, |answer| {
            matches!(answer.state, State::Unavailable(_))
        });
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 1);
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
            wait_until(|| service.shared.child.lock().unwrap().is_none());
            let refresh = request(&service, "sample x", Trigger::Refresh);
            wait_answer(&service, &refresh, |answer| {
                matches!(answer.state, State::Unavailable(_))
            });
            assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 1);
            let retry = complete(&service, "sample safe");
            assert_eq!(retry.candidates.len(), 1);
            assert_eq!(
                retry.candidates[0].value, "safe",
                "reuse of mutated background-job state"
            );
            assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 2);
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
        start(&service.shared, &mut slot, &launcher(), &snapshot);
        slot.worker.as_mut().unwrap().hold_reaping(true);
        slot.stop(&service.shared);
        let newest = service.request("cd x", 4, Trigger::Explicit);
        slot.pending = Some((newest.clone(), Instant::now()));
        start(&service.shared, &mut slot, &launcher(), &snapshot);
        assert!(service.result(&newest).is_none());
        assert_eq!(slot.pending.as_ref().unwrap().0, newest);
        slot.pending.as_mut().unwrap().1 =
            Instant::now() - crate::input_assist::LOOKUP_TIMEOUT - Duration::from_millis(1);
        start(&service.shared, &mut slot, &launcher(), &snapshot);
        assert!(matches!(
            service.result(&newest).unwrap().1.state,
            State::Failed(_)
        ));
        assert_eq!(slot.failures[0].count, 1);
        assert_eq!(service.shared.spawns.load(Ordering::Relaxed), 1);
        slot.worker.as_mut().unwrap().hold_reaping(false);
        wait_until(|| {
            slot.reap(&service.shared);
            slot.worker.is_none()
        });
    }
}
