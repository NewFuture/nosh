//! One latest-only worker temporarily owns the existing engine, never a second model.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use nosh_llm::CancelHandle;
use nosh_shell::{AssistDisplay, Assistance};

use crate::command_assist::{self, AssistError, AssistRequest, AssistResult};
use crate::handler::{EngineLoader, LoadMode};
use crate::{Agent, AgentConfig, Environment, ToolSet};

pub(crate) struct EngineState {
    pub loader: EngineLoader,
    pub agent: Option<Agent>,
    pub description: Option<String>,
}

struct Job {
    request: AssistRequest,
    cfg: AgentConfig,
    version: u64,
    cancel: CancelHandle,
}

#[derive(Default)]
struct Mailbox {
    job: Option<Job>,
    stopping: bool,
}

pub(crate) struct Worker {
    mailbox: Arc<(Mutex<Mailbox>, Condvar)>,
    display: AssistDisplay,
    thread: Option<JoinHandle<EngineState>>,
}

impl Worker {
    pub fn start(state: EngineState, display: AssistDisplay) -> Self {
        let mailbox = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
        let worker_mailbox = mailbox.clone();
        let worker_display = display.clone();
        let thread = std::thread::spawn(move || serve(state, worker_mailbox, worker_display));
        Self {
            mailbox,
            display,
            thread: Some(thread),
        }
    }

    pub fn submit(&self, request: AssistRequest, cfg: AgentConfig) {
        let version = self.display.invalidate();
        let cancel = CancelHandle::default();
        let flag = cancel.clone();
        self.display
            .on_cancel(version, Arc::new(move || flag.cancel()));
        let mut mailbox = self.mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
        mailbox.job = Some(Job {
            request,
            cfg,
            version,
            cancel,
        });
        self.mailbox.1.notify_one();
    }

    pub fn reclaim(mut self) -> Result<EngineState, String> {
        self.stop();
        self.thread
            .take()
            .expect("worker owns its thread")
            .join()
            .map_err(|_| "command assistance worker panicked; engine unavailable".into())
    }

    fn stop(&self) {
        self.display.invalidate();
        let mut mailbox = self.mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
        mailbox.stopping = true;
        mailbox.job = None;
        self.mailbox.1.notify_one();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("nosh: command assistance worker panicked during shutdown");
        }
    }
}

fn serve(
    mut state: EngineState,
    mailbox: Arc<(Mutex<Mailbox>, Condvar)>,
    display: AssistDisplay,
) -> EngineState {
    loop {
        let job = {
            let mut queued = mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
            while queued.job.is_none() && !queued.stopping {
                queued = mailbox.1.wait(queued).unwrap_or_else(|e| e.into_inner());
            }
            if queued.stopping {
                return state;
            }
            queued.job.take().expect("queued assistance")
        };
        if job.cancel.is_cancelled() {
            continue;
        }
        let result = (|| {
            if state.agent.is_none() {
                let loaded = (state.loader)(LoadMode::Background).map_err(AssistError::Protocol)?;
                state.description = Some(loaded.description);
                state.agent = Some(Agent::new(
                    loaded.engine,
                    job.cfg.clone(),
                    Environment::from_snapshot(&job.request.commands, &job.request.context),
                    ToolSet::Full,
                ));
            }
            if job.cancel.is_cancelled() {
                return Err(AssistError::Cancelled);
            }
            let agent = state.agent.as_mut().expect("loaded agent");
            let engine_cancel = agent.engine_mut().cancel_handle();
            engine_cancel.reset();
            let request_cancel = job.cancel.clone();
            display.on_cancel(
                job.version,
                Arc::new(move || {
                    request_cancel.cancel();
                    engine_cancel.cancel();
                }),
            );
            command_assist::run(agent.engine_mut(), &job.request, &job.cfg, &job.cancel)
        })();
        let result = match result {
            Ok(outcome) => match outcome.result {
                AssistResult::Command(program) => Some(Assistance::Command {
                    command_id: job.request.command.as_ref().expect("completion request").id,
                    intent: job.request.intent.name().into(),
                    program,
                }),
                AssistResult::Clarify(text) => Some(Assistance::Message(text)),
                AssistResult::NoSuggestion => None,
            },
            Err(AssistError::Cancelled) => continue,
            Err(error) => Some(Assistance::Message(format!("nosh: {error}"))),
        };
        display.publish(job.version, result);
    }
}
