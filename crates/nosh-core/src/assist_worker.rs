//! One latest-only worker temporarily owns the existing engine, never a second model.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use nosh_engine::CancelHandle;
use nosh_shell::{AssistDisplay, Assistance};

use crate::command_assist::{self, AssistError, AssistRequest, AssistResult};
use crate::handler::{EngineLoadError, EngineLoader, LoadMode, ModelAvailability};
use crate::{Agent, AgentConfig, Environment, ToolSet};

pub(crate) struct EngineState {
    pub theme: nosh_shell::theme::ThemeHandle,
    pub loader: EngineLoader,
    pub agent: Option<Agent>,
    pub availability: Arc<Mutex<ModelAvailability>>,
}

impl EngineState {
    pub fn load(
        &mut self,
        mode: LoadMode,
        config: &AgentConfig,
        environment: impl FnOnce() -> Environment,
    ) -> Result<(), EngineLoadError> {
        if mode == LoadMode::Background {
            let availability = self
                .availability
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(error) = availability.loading_error() {
                return Err(error);
            }
        }
        let loaded = match (self.loader)(mode) {
            Ok(loaded) => loaded,
            Err(error) => {
                *self
                    .availability
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = match &error {
                    EngineLoadError::NotReady(reason) => {
                        ModelAvailability::NotReady(reason.clone())
                    }
                    EngineLoadError::Failed(reason) => ModelAvailability::Failed(reason.clone()),
                    EngineLoadError::Cancelled(_) => ModelAvailability::NotLoaded,
                };
                return Err(error);
            }
        };
        self.agent = Some(
            Agent::new(loaded.engine, config.clone(), environment(), ToolSet::Full)
                .with_user_input(Box::new(
                    crate::user_input::TerminalUserInput.with_theme(self.theme.clone()),
                )),
        );
        *self
            .availability
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            ModelAvailability::Ready(loaded.description);
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum WorkerError {
    #[error(transparent)]
    Load(#[from] EngineLoadError),
    #[error(transparent)]
    Assist(#[from] AssistError),
}

impl WorkerError {
    fn presentation(&self) -> Option<Assistance> {
        let reason = match self {
            Self::Assist(error) => return assist_failure(error),
            Self::Load(EngineLoadError::Cancelled(_)) => return None,
            Self::Load(EngineLoadError::NotReady(reason) | EngineLoadError::Failed(reason)) => {
                reason
            }
        };
        Some(Assistance::Failure(format!("AI: {reason}")))
    }
}

pub(crate) fn assist_failure(error: &AssistError) -> Option<Assistance> {
    if matches!(error, AssistError::Cancelled) {
        return None;
    }
    let reason = match error {
        AssistError::Protocol(reason) => reason.clone(),
        error => error.to_string(),
    };
    Some(Assistance::Failure(format!("AI: {reason}")))
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
    context: Option<(usize, usize)>,
}

impl Worker {
    pub fn start(state: EngineState, display: AssistDisplay) -> Self {
        let context = state.agent.as_ref().and_then(Agent::context_usage);
        let mailbox = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
        let worker_mailbox = mailbox.clone();
        let worker_display = display.clone();
        let thread = std::thread::spawn(move || serve(state, worker_mailbox, worker_display));
        Self {
            mailbox,
            display,
            thread: Some(thread),
            context,
        }
    }

    pub fn submit(&self, request: AssistRequest, cfg: AgentConfig) {
        let version = self.display.invalidate();
        let cancel = CancelHandle::default();
        let flag = cancel.clone();
        self.display
            .on_cancel(version, Arc::new(move || flag.cancel()));
        self.display.pending(version);
        let mut mailbox = self.mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
        mailbox.job = Some(Job {
            request,
            cfg,
            version,
            cancel,
        });
        self.mailbox.1.notify_one();
    }

    pub fn context_usage(&self) -> Option<(usize, usize)> {
        self.context
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
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
        if let Err(error) = run_job(&mut state, &job, &display) {
            display.publish(job.version, error.presentation());
        }
    }

    fn run_job(
        state: &mut EngineState,
        job: &Job,
        display: &AssistDisplay,
    ) -> Result<(), WorkerError> {
        if job.cancel.is_cancelled() {
            return Err(AssistError::Cancelled.into());
        }
        if state.agent.is_none() {
            state.load(LoadMode::Background, &job.cfg, || {
                Environment::from_snapshot(&job.request.commands, &job.request.context)
            })?;
        }
        if job.cancel.is_cancelled() {
            return Err(AssistError::Cancelled.into());
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
        let command_id = job.request.command.as_ref().expect("completion request").id;
        command_assist::run(
            agent.engine_mut(),
            &job.request,
            &job.cfg,
            &job.cancel,
            |result| {
                let presentation = match result {
                    Ok(outcome) => match &outcome.result {
                        AssistResult::Command(program) => Some(Assistance::Command {
                            command_id,
                            intent: job.request.intent.name().into(),
                            program: program.clone(),
                        }),
                        AssistResult::NoSuggestion => None,
                    },
                    Err(error) => assist_failure(error),
                };
                display.publish(job.version, presentation)
            },
        )
        .map(|_| ())
        .map_err(WorkerError::Assist)
    }
}
