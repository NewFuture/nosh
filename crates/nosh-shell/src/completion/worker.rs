use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use brush_core::completion::{CompleteAction, CompletionToken};
use brush_core::{CommandArg, ExecutionContext, ExecutionResult};

use super::cache::{Cache, Entry, Set};
use super::context::Context;
use super::types::*;
use super::{native, providers};
use crate::backend::BrushShell;

static UNSUPPORTED: AtomicBool = AtomicBool::new(false);

fn unsupported(
    _context: ExecutionContext<'_>,
    _args: Vec<CommandArg>,
) -> Pin<Box<dyn Future<Output = Result<ExecutionResult, brush_core::Error>> + Send + '_>> {
    UNSUPPORTED.store(true, Ordering::Release);
    Box::pin(async {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "interactive job control is unavailable in a completion worker",
        )
        .into())
    })
}

#[derive(Default)]
pub(crate) struct Server {
    native: Option<Arc<NativeSnapshot>>,
    execution: Option<Execution>,
    cache: Cache,
    session: u64,
}

struct Execution {
    shell: BrushShell,
    runtime: tokio::runtime::Runtime,
}

fn ready(answer: Answer) -> Outcome {
    Outcome::Ready {
        answer,
        registry: None,
        checkpoint: None,
    }
}

impl Server {
    pub fn run(
        &mut self,
        query: Query,
        install: Option<Install>,
        script: bool,
        progress: &mut impl FnMut(Outcome) -> std::io::Result<()>,
    ) -> std::io::Result<Outcome> {
        if let Some(install) = install {
            if query.session != self.session {
                self.cache = Cache::default();
            }
            self.session = query.session;
            match install {
                Install::Native(snapshot) if !script => self.native = Some(snapshot),
                Install::Script { native, state } if script => {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    let mut builtins =
                        brush_builtins::default_builtins(brush_builtins::BuiltinSet::BashMode);
                    for name in ["jobs", "fg", "bg", "wait", "disown"] {
                        if let Some(registration) = builtins.get_mut(name) {
                            registration.execute_func = unsupported;
                        }
                    }
                    let mut shell = runtime
                        .block_on(
                            BrushShell::builder()
                                .do_not_inherit_env(true)
                                .skip_well_known_vars(true)
                                .no_editing(true)
                                .profile(brush_core::ProfileLoadBehavior::Skip)
                                .rc(brush_core::RcLoadBehavior::Skip)
                                .builtins(builtins)
                                .working_dir(native.context.cwd.clone())
                                .build(),
                        )
                        .map_err(std::io::Error::other)?;
                    let state = serde_json::from_str(state.get()).map_err(std::io::Error::other)?;
                    shell
                        .restore_completion_state(state)
                        .map_err(std::io::Error::other)?;
                    if let Ok(marker) = std::env::var(crate::procs::RUN_VAR) {
                        let mut variable = brush_core::ShellVariable::new(marker);
                        variable.export();
                        shell
                            .env_mut()
                            .set_global(crate::procs::RUN_VAR, variable)
                            .map_err(std::io::Error::other)?;
                    }
                    self.native = Some(native);
                    self.execution = Some(Execution { shell, runtime });
                }
                _ => {
                    return Err(std::io::Error::other(
                        "completion snapshot sent to wrong worker",
                    ));
                }
            }
        }
        let snapshot = self
            .native
            .clone()
            .ok_or_else(|| std::io::Error::other("missing completion snapshot"))?;
        if query.session != self.session {
            return Err(std::io::Error::other(
                "completion snapshot version mismatch",
            ));
        }
        let context = match Context::parse(&query, &snapshot) {
            Ok(context) => context,
            Err(error) => return Ok(ready(Answer::failed(query, error))),
        };
        if !script {
            if context.needs_script(&snapshot)
                || (context.quote != Some('\'')
                    && context.word.contains(['$', '`'])
                    && context.word.contains('/'))
            {
                return Ok(Outcome::ScriptRequired(query));
            }
            let answer = self.basic(query, &context, &snapshot, progress)?;
            return Ok(ready(answer));
        }
        let before = Registry::capture(
            &self
                .execution
                .as_ref()
                .ok_or_else(|| std::io::Error::other("missing completion execution"))?
                .shell,
        );
        UNSUPPORTED.store(false, Ordering::Release);
        let mut loaded = false;
        let answer = match self.script(&query, &context, &snapshot, &mut loaded, progress) {
            Ok(answer) => answer,
            Err(error) => Answer::failed(query.clone(), error),
        };
        let shell = &self
            .execution
            .as_ref()
            .ok_or_else(|| std::io::Error::other("missing completion execution"))?
            .shell;
        let answer = if !shell.jobs().jobs.is_empty() {
            Answer::unavailable(
                query,
                "completion provider created background jobs; isolated execution must be reset",
            )
        } else if UNSUPPORTED.load(Ordering::Acquire) {
            Answer::unavailable(
                query,
                "provider needs interactive job control; isolated completion cannot supply it",
            )
        } else {
            answer
        };
        let registry = Registry::capture(shell);
        let checkpoint = if loaded && answer.state.is_complete() {
            Some(
                crate::input_assist::bounded_json(&shell.completion_state(), MAX_SNAPSHOT)
                    .and_then(|bytes| String::from_utf8(bytes).map_err(std::io::Error::other))
                    .and_then(|json| {
                        serde_json::value::RawValue::from_string(json)
                            .map_err(std::io::Error::other)
                    })?,
            )
        } else {
            None
        };
        Ok(Outcome::Ready {
            answer,
            registry: (before != registry).then_some(registry),
            checkpoint,
        })
    }

    fn basic(
        &mut self,
        query: Query,
        context: &Context,
        snapshot: &NativeSnapshot,
        progress: &mut impl FnMut(Outcome) -> std::io::Result<()>,
    ) -> std::io::Result<Answer> {
        if context.word.starts_with('$')
            && context.quote != Some('\'')
            && !context.word.contains('/')
        {
            let brace = context.word.starts_with("${");
            let prefix = context
                .word
                .trim_start_matches('$')
                .trim_start_matches('{')
                .trim_end_matches('}');
            let entries = snapshot
                .variables
                .iter()
                .filter(|name| name.starts_with(prefix))
                .map(|name| Entry {
                    value: if brace {
                        format!("${{{name}}}")
                    } else {
                        format!("${name}")
                    },
                    kind: Kind::Variable,
                    description: None,
                })
                .collect();
            let mut answer = native::select(
                query,
                context,
                &Set {
                    entries,
                    reason: None,
                },
                Source::Variable,
                false,
                false,
            );
            if !snapshot.variables_complete {
                answer.state = State::Partial("variable name snapshot limit reached".into());
            }
            for candidate in &mut answer.candidates {
                candidate.noquote = true;
                candidate.nospace = true;
                if context.quote == Some('"') {
                    candidate.value = format!("\"{}\"", candidate.value);
                }
            }
            return Ok(answer);
        }
        if context.index == 0
            && !context.redirect
            && !context.word.contains('/')
            && !context.word.starts_with('~')
        {
            native::commands(query, context, snapshot, &mut self.cache, progress)
        } else if !context.redirect
            && let Some(answer) =
                providers::generate(query.clone(), context, snapshot, &mut self.cache)
        {
            Ok(answer)
        } else {
            Ok(native::paths(
                query,
                context,
                snapshot,
                &mut self.cache,
                context.command.as_deref() == Some("cd") && !context.redirect,
            ))
        }
    }

    fn script(
        &mut self,
        query: &Query,
        context: &Context,
        snapshot: &NativeSnapshot,
        loaded: &mut bool,
        progress: &mut impl FnMut(Outcome) -> std::io::Result<()>,
    ) -> Result<Answer, String> {
        let raw = context.script_tokens(query, snapshot);
        let mut words: Vec<_> = raw
            .iter()
            .map(|token| brush_parser::unquote_str(token.text))
            .collect();
        let cursor = query.cursor - context.command_start;
        let index = raw
            .iter()
            .position(|token| token.start <= cursor && cursor <= token.end())
            .unwrap_or_else(|| raw.iter().take_while(|token| token.end() < cursor).count());
        let current = raw
            .get(index)
            .filter(|token| token.start <= cursor && cursor <= token.end());
        let start = current.map_or(cursor, |token| token.start);
        let end = current.map_or(cursor, |token| token.end());
        let token = query
            .text
            .get(context.command_start + start..query.cursor)
            .ok_or("invalid script completion span")?;
        let token = brush_parser::unquote_str(token);
        if current.is_none() {
            words.insert(index, String::new());
        }
        let tokens: Vec<_> = words
            .iter()
            .enumerate()
            .map(|(index, text)| CompletionToken {
                text,
                start: raw.get(index).map_or(cursor, |token| token.start),
            })
            .collect();
        let refs: Vec<_> = tokens.iter().collect();
        let request = brush_core::completion::Context {
            token_to_complete: &token,
            command_name: context.command.as_deref(),
            preceding_token: index
                .checked_sub(1)
                .and_then(|index| words.get(index))
                .map(String::as_str),
            token_index: index,
            input_line: &query.text,
            cursor_index: query.cursor,
            tokens: &refs,
            trigger: brush_core::completion::CompletionTrigger::InteractiveComplete,
        };
        for _ in 0..10 {
            let execution = self
                .execution
                .as_mut()
                .ok_or("missing completion execution")?;
            let shell = &mut execution.shell;
            let Some(spec) = shell.completion_config().specification(&request).cloned() else {
                if context.word.contains(['$', '`']) && context.word.contains('/') {
                    let expanded = execution
                        .runtime
                        .block_on(shell.expand_completion_path(&query.text[context.span.clone()]))
                        .map_err(|error| error.to_string())?;
                    let expanded_context = Context {
                        word: expanded,
                        ..context.clone()
                    };
                    return Ok(native::paths(
                        query.clone(),
                        &expanded_context,
                        snapshot,
                        &mut self.cache,
                        false,
                    ));
                }
                return self
                    .basic(query.clone(), context, snapshot, progress)
                    .map_err(|error| error.to_string());
            };
            if !snapshot.scripts {
                return Ok(Answer::unavailable(
                    query.clone(),
                    "programmable completion is disabled",
                ));
            }
            if spec
                .actions
                .iter()
                .any(|action| matches!(action, CompleteAction::Job | CompleteAction::Running))
            {
                return Ok(Answer::unavailable(
                    query.clone(),
                    "job candidates require the live interactive shell",
                ));
            }
            let result = execution
                .runtime
                .block_on(spec.get_completions_without_fallback(shell, &request))
                .map_err(|error| error.to_string())?;
            let brush_core::completion::Answer::Candidates(values, options) = result else {
                *loaded = true;
                continue;
            };
            let generation = options.generation.clone().unwrap_or_default();
            let mut seen = HashSet::new();
            let mut candidates = Vec::new();
            let mut bytes = 0;
            let mut reason = None;
            for value in values {
                if !seen.insert(value.clone()) {
                    continue;
                }
                if value.len() > MAX_WORD
                    || candidates.len() >= MAX_RESULTS
                    || bytes + value.len() + 128 > MAX_SET_BYTES
                {
                    reason = Some("script candidate/result limit reached".into());
                    break;
                }
                let kind = if options.treat_as_filenames {
                    match std::fs::metadata(snapshot.context.cwd.join(&value)) {
                        Ok(metadata) if metadata.is_dir() => Kind::Directory,
                        Ok(_) => Kind::File,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Kind::File,
                        Err(error) => {
                            reason.get_or_insert_with(|| error.to_string());
                            Kind::File
                        }
                    }
                } else {
                    Kind::Value
                };
                let mut value = value;
                if kind == Kind::Directory && !value.ends_with('/') {
                    value.push('/');
                }
                bytes += value.len() + 128;
                candidates.push(Candidate {
                    source: Source::Script(context.command.clone().unwrap_or_default()),
                    value,
                    kind,
                    description: None,
                    span: context.command_start + start..context.command_start + end,
                    filenames: options.treat_as_filenames,
                    noquote: options.no_autoquote_filenames,
                    nospace: options.no_trailing_space_at_end_of_line || kind == Kind::Directory,
                    matches: Vec::new(),
                    display: None,
                });
            }
            let mut answer = Answer {
                query: query.clone(),
                candidates,
                state: reason.map_or(State::Complete, State::Partial),
            };
            let empty = answer.candidates.is_empty() && answer.state.is_complete();
            if (empty && generation.dir_names) || generation.plus_dirs {
                let directories =
                    native::paths(query.clone(), context, snapshot, &mut self.cache, true);
                if !directories.state.is_complete() {
                    answer.state = directories.state;
                }
                answer.candidates.extend(directories.candidates);
            }
            if empty
                && answer.candidates.is_empty()
                && generation.bash_default
                && context.index == 0
            {
                answer =
                    native::commands(query.clone(), context, snapshot, &mut self.cache, progress)
                        .map_err(|error| error.to_string())?;
            }
            if empty && answer.candidates.is_empty() && generation.default {
                answer = native::paths(query.clone(), context, snapshot, &mut self.cache, false);
            }
            if answer.candidates.len() > MAX_RESULTS {
                answer.candidates.truncate(MAX_RESULTS);
                answer.state = State::Partial("combined result limit reached".into());
            }
            return Ok(answer);
        }
        Err("completion autoload restart limit reached".into())
    }
}
