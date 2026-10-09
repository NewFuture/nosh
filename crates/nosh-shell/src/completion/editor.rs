use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use brush_core::escape::QuoteMode;
use nosh_platform::tr;
use reedline::{
    Completer, CompletionAcceptance, CompletionResult, CompletionStatus, Partial, Span, Suggestion,
    Suggestions,
};
use unicode_segmentation::UnicodeSegmentation;

use super::service::Service;
use super::types::*;
use super::{Config, Phase, snapshot};

/// An explicit choice from the abbreviation owner's approved snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbbreviationSelection {
    pub name: String,
    pub revision: u64,
}

/// Called on the editor thread after acceptance; observers must not block.
pub type SelectionObserver = Arc<dyn Fn(&AbbreviationSelection) + Send + Sync>;

// Two generations may overlap during refresh. The other half of each 1 MiB
// generation covers Reedline's buffer/base copies and derived menu metrics.
const DISPLAY_GENERATION_BYTES: usize = 512 * 1024;

#[derive(Clone)]
pub struct Completion {
    service: Service,
    config: Config,
    state: Arc<Mutex<Option<crate::status::Completion>>>,
    current: Option<Query>,
    serial: u64,
    values: Suggestions,
    selections: Vec<(usize, AbbreviationSelection)>,
    explicit: bool,
    automatic: bool,
}

fn quote_before(line: &str, position: usize) -> Option<char> {
    let mut quote = None;
    let mut escape = false;
    for character in line[..position].chars() {
        if escape {
            escape = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escape = true;
            continue;
        }
        if Some(character) == quote {
            quote = None;
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
        }
    }
    quote
}

#[cfg(test)]
fn insert(candidate: &Candidate, query: &Query) -> String {
    insert_in_quote(
        candidate,
        query,
        quote_before(&query.text, candidate.span.start),
    )
}

fn insert_in_quote(candidate: &Candidate, query: &Query, quote: Option<char>) -> String {
    if candidate.noquote {
        return candidate.value.clone();
    }
    if let Some(quote) = quote {
        return if quote == '\'' {
            candidate.value.replace('\'', "'\\''")
        } else {
            candidate
                .value
                .chars()
                .fold(String::new(), |mut output, character| {
                    if matches!(character, '$' | '`' | '"' | '\\') {
                        output.push('\\');
                    }
                    output.push(character);
                    output
                })
        };
    }
    let original = &query.text[candidate.span.clone()];
    let mode = match original.chars().next() {
        Some('\'') => Some(QuoteMode::SingleQuote),
        Some('"') => Some(QuoteMode::DoubleQuote),
        _ if candidate.value.contains(['\n', '\r']) => Some(QuoteMode::SingleQuote),
        _ => None,
    };
    if let Some(mode) = mode {
        brush_core::escape::force_quote(&candidate.value, mode)
    } else {
        brush_core::escape::quote_if_needed(&candidate.value, QuoteMode::BackslashEscape)
            .into_owned()
    }
}

fn description(text: &str) -> &str {
    match text {
        "alias" => tr!("别名", "alias"),
        "function" => tr!("函数", "function"),
        "builtin" => tr!("内建命令", "builtin"),
        "executable" => tr!("外部命令", "executable"),
        "hashed executable" => tr!("已索引外部命令", "hashed executable"),
        "static Make target" => tr!("静态 Make 目标", "static Make target"),
        _ => text,
    }
}

fn display_text_limited(
    text: &str,
    matches: &[usize],
    limit: usize,
    bytes: usize,
) -> (String, Vec<usize>) {
    let mut display = String::new();
    let mut highlights = Vec::new();
    let mut position = 0;
    'text: for (index, grapheme) in text.graphemes(true).enumerate() {
        let visible = crate::style::visible_text(grapheme)
            .replace('\n', "\\n")
            .replace('\t', "\\t");
        for grapheme in visible.graphemes(true) {
            let matched = matches.contains(&index);
            let required = display.len().saturating_add(grapheme.len()).saturating_add(
                (highlights.len() + usize::from(matched)) * std::mem::size_of::<usize>(),
            );
            if position == limit || required > bytes {
                break 'text;
            }
            display.push_str(grapheme);
            if matched {
                highlights.push(position);
            }
            position += 1;
        }
    }
    display.shrink_to_fit();
    highlights.shrink_to_fit();
    (display, highlights)
}

fn identity(candidate: &Candidate, scope: u64) -> String {
    let value = if candidate.kind == Kind::Directory {
        candidate
            .value
            .strip_suffix('/')
            .unwrap_or(&candidate.value)
    } else {
        &candidate.value
    };
    let fingerprint = |domain: u8| {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        domain.hash(&mut hash);
        candidate.source.hash(&mut hash);
        value.hash(&mut hash);
        hash.finish()
    };
    format!(
        "{scope:016x}:{:016x}:{:016x}",
        fingerprint(0),
        fingerprint(1)
    )
}

fn common_prefix(query: &Query, values: &[Suggestion]) -> Partial {
    let unchanged = Partial {
        span: Span {
            start: query.cursor,
            end: query.cursor,
        },
        insert: String::new(),
    };
    let Some(first) = values.first() else {
        return unchanged;
    };
    let span = first.span;
    if span.end != query.cursor || values.iter().any(|value| value.span != span) {
        return unchanged;
    }
    let mut prefix = first.value.as_str();
    for value in &values[1..] {
        let end = prefix
            .char_indices()
            .zip(value.value.chars())
            .find_map(|((index, left), right)| (left != right).then_some(index))
            .unwrap_or(prefix.len().min(value.value.len()));
        prefix = &prefix[..end];
    }
    let Some(entered) = query.text.get(span.start..span.end) else {
        return unchanged;
    };
    // Compare insertion text, not display/fuzzy matches. Never replace a
    // subsequence with a prefix, or leave half of a backslash escape behind.
    let extends = prefix.len() > entered.len()
        && prefix.get(..entered.len()).is_some_and(|start| {
            start == entered
                || (!entered
                    .chars()
                    .any(|character| character.is_ascii_uppercase())
                    && start.eq_ignore_ascii_case(entered))
        })
        && prefix.chars().rev().take_while(|c| *c == '\\').count() % 2 == 0;
    if extends {
        Partial {
            span,
            insert: prefix.into(),
        }
    } else {
        unchanged
    }
}

fn suggestions(answer: &Answer) -> Suggestions {
    let mut scopes = HashMap::new();
    let count = answer.candidates.len().min(MAX_RESULTS);
    let fixed = count * (std::mem::size_of::<Suggestion>() + 128) + 256;
    let share = DISPLAY_GENERATION_BYTES.saturating_sub(fixed) / count.max(1);
    answer
        .candidates
        .iter()
        .take(count)
        .map(|candidate| {
            let scope = scopes
                .entry((candidate.span.start, candidate.span.end))
                .or_insert_with(|| {
                    let mut hash = std::collections::hash_map::DefaultHasher::new();
                    answer.query.text[..candidate.span.start].hash(&mut hash);
                    answer.query.text[candidate.span.end..].hash(&mut hash);
                    answer.query.session.hash(&mut hash);
                    (
                        hash.finish(),
                        quote_before(&answer.query.text, candidate.span.start),
                    )
                });
            let (scope, quote) = *scope;
            let mut completion_id = identity(candidate, scope);
            completion_id.shrink_to_fit();
            let available = share.saturating_sub(completion_id.capacity());
            let reserve = if candidate.description.is_some() {
                128.min(available / 4)
            } else {
                0
            };
            let raw_display = candidate.display.as_deref().unwrap_or(&candidate.value);
            let (display, matches) = display_text_limited(
                raw_display,
                if raw_display == candidate.value
                    || matches!(&candidate.source, Source::Abbreviation { name, .. } if name == raw_display)
                {
                    &candidate.matches
                } else {
                    &[]
                },
                256,
                available - reserve,
            );
            let remaining = available.saturating_sub(
                display.capacity() + matches.capacity() * std::mem::size_of::<usize>(),
            );
            let description = candidate.description.as_deref()
                .map(|text| display_text_limited(description(text), &[], 240, remaining).0);
            Suggestion {
                completion_id: Some(completion_id),
                value: insert_in_quote(candidate, &answer.query, quote),
                display_override: Some(display),
                description,
                span: Span {
                    start: candidate.span.start,
                    end: candidate.span.end,
                },
                append_whitespace: answer.query.cursor == answer.query.text.len()
                    && !candidate.nospace,
                match_indices: Some(matches),
                style: (candidate.kind == Kind::Directory)
                    .then(|| nu_ansi_term::Color::Green.normal()),
                ..Suggestion::default()
            }
        })
        .collect()
}

impl Completion {
    pub(crate) fn new(
        config: Config,
        state: Arc<Mutex<Option<crate::status::Completion>>>,
        repaint: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            service: Service::new(config.worker.clone(), repaint),
            config,
            state,
            current: None,
            serial: 0,
            values: Vec::<Suggestion>::new().into(),
            selections: Vec::new(),
            explicit: false,
            automatic: false,
        }
    }

    pub(crate) fn prepare(&self, shell: &crate::EmbeddedShell) {
        if self.config.enabled {
            if shell.project_env_loading() {
                self.service.suspend();
                return;
            }
            self.service.prepare(snapshot::capture(
                shell,
                self.config.scripts,
                &self.config.abbreviations,
            ));
        }
    }

    fn status(&self, query: &Query, phase: Phase, error: Option<String>, count: usize) {
        if let Ok(mut state) = self.state.try_lock() {
            *state = Some(crate::status::Completion {
                input: query.text.to_string(),
                cursor: query.cursor,
                error,
                count,
                phase,
            });
        }
    }

    fn update_values(&mut self, answer: Arc<Answer>) {
        self.values = suggestions(&answer);
        self.selections = answer
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| match &candidate.source {
                Source::Abbreviation { name, revision } => Some((
                    index,
                    AbbreviationSelection {
                        name: name.clone(),
                        revision: *revision,
                    },
                )),
                _ => None,
            })
            .collect();
    }

    fn partial(&self, query: &Query) -> Partial {
        if !self.selections.is_empty() {
            Partial {
                span: Span::new(query.cursor, query.cursor),
                insert: String::new(),
            }
        } else {
            common_prefix(query, &self.values)
        }
    }
}

impl Completer for Completion {
    fn completion_accepted(&mut self, suggestion: &Suggestion) -> CompletionAcceptance {
        let Some((_, selection)) = self
            .selections
            .iter()
            .find(|(index, _)| self.values[*index].completion_id == suggestion.completion_id)
        else {
            return CompletionAcceptance::Continue;
        };
        if let Some(observer) = &self.config.selection_observer {
            observer(selection);
        }
        CompletionAcceptance::SuppressAbbreviationExpansion
    }

    fn completion_requested(&mut self) {
        self.explicit = true;
        self.automatic = true;
    }
    fn completion_navigated(&mut self) {
        self.automatic = false;
    }
    fn completion_cancelled(&mut self) {
        self.automatic = false;
        self.explicit = false;
        self.current = None;
        self.service.cancel();
    }
    fn automatic_completion_allowed(&self) -> bool {
        self.automatic
    }

    fn complete(&mut self, line: &str, cursor: usize) -> CompletionResult {
        if !self.config.enabled {
            let message = tr!("补全已关闭", "Completion disabled").to_string();
            self.status(
                &Query {
                    text: line.into(),
                    cursor,
                    session: self.service.session(),
                    epoch: 0,
                    trigger: Trigger::Explicit,
                },
                Phase::Unavailable,
                Some(message.clone()),
                0,
            );
            return CompletionResult::Unavailable { message };
        }
        if let Some(prefix) = &self.config.inline_prefix
            && crate::inline_commands::is_command(line, prefix, true)
        {
            self.service.cancel();
            self.current = None;
            self.selections.clear();
            let query = Query {
                text: line.into(),
                cursor,
                session: self.service.session(),
                epoch: 0,
                trigger: if self.explicit {
                    Trigger::Explicit
                } else {
                    Trigger::Refresh
                },
            };
            self.explicit = false;
            if line.len() > MAX_INPUT || line.get(..cursor).is_none() {
                let message = tr!(
                    "内置命令补全输入无效或超限",
                    "invalid or oversized inline command completion input"
                )
                .to_string();
                self.status(&query, Phase::Unavailable, Some(message.clone()), 0);
                return CompletionResult::Unavailable { message };
            }
            let scope = crate::inline_commands::completion_scope(line, cursor, prefix)
                .expect("command completion scope");
            let mut candidates = scope
                .values
                .iter()
                .filter_map(|(value, description)| {
                    let (rank, matches) = super::matching::rank(value, scope.word, true, false)?;
                    Some((
                        rank,
                        Candidate {
                            source: Source::Command,
                            value: (*value).into(),
                            kind: if scope.command_name {
                                Kind::Subcommand
                            } else {
                                Kind::Value
                            },
                            description: description.map(str::to_owned),
                            span: scope.span.clone(),
                            noquote: true,
                            nospace: false,
                            matches,
                            display: None,
                        },
                    ))
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|a, b| (&a.0, &a.1.value).cmp(&(&b.0, &b.1.value)));
            let answer = Answer {
                query,
                candidates: candidates
                    .into_iter()
                    .map(|(_, candidate)| candidate)
                    .collect(),
                state: State::Complete,
            };
            self.values = suggestions(&answer);
            self.status(&answer.query, Phase::Complete, None, self.values.len());
            return CompletionResult::fresh(self.values.clone())
                .with_partial(Some(self.partial(&answer.query)));
        }
        let changed = self.current.as_ref().is_none_or(|query| {
            !query.matches(line, cursor) || query.session != self.service.session()
        });
        if changed {
            let trigger = if self.explicit {
                Trigger::Explicit
            } else {
                self.automatic = false;
                Trigger::Refresh
            };
            self.current = Some(self.service.request(line, cursor, trigger));
        }
        self.explicit = false;
        let Some(query) = &self.current else {
            return CompletionResult::Pending;
        };
        let Some((serial, answer, ongoing)) = self.service.result(query) else {
            self.status(query, Phase::Pending, None, 0);
            return CompletionResult::Limited {
                suggestions: Vec::<Suggestion>::new().into(),
                message: tr!("正在查询补全", "Querying completions").into(),
            };
        };
        let query = &answer.query;
        if self.serial != serial {
            self.update_values(answer.clone());
            self.serial = serial;
        }
        match &answer.state {
            State::Complete => {
                self.status(query, Phase::Complete, None, self.values.len());
                CompletionResult::fresh(self.values.clone()).with_partial(Some(self.partial(query)))
            }
            State::Partial(message) => {
                let querying = ongoing && self.values.is_empty();
                let phase = if querying {
                    Phase::Pending
                } else {
                    Phase::Partial
                };
                self.status(query, phase, None, self.values.len());
                CompletionResult::Limited {
                    suggestions: self.values.clone(),
                    message: if querying {
                        display_text_limited(message, &[], 240, 4096).0
                    } else {
                        format!(
                            "{}: {}",
                            tr!("部分补全结果", "Partial completions"),
                            display_text_limited(message, &[], 220, 4096).0,
                        )
                    },
                }
            }

            State::Failed(message) | State::Unavailable(message) => {
                let message = format!(
                    "{}: {}",
                    tr!("补全暂不可用", "Completion unavailable"),
                    display_text_limited(message, &[], 240, 4096).0
                );
                self.status(query, Phase::Unavailable, Some(message.clone()), 0);
                CompletionResult::Unavailable { message }
            }
        }
    }

    fn poll_completion(&mut self) -> CompletionStatus {
        let Some(query) = &self.current else {
            return CompletionStatus::Idle;
        };
        match self.service.result(query) {
            Some((serial, _, _)) if serial != self.serial => CompletionStatus::Ready,
            None | Some((_, _, true)) => CompletionStatus::Pending,
            _ => CompletionStatus::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmbeddedShell, ShellOptions};

    fn display_bytes(suggestion: &Suggestion) -> usize {
        std::mem::size_of::<Suggestion>()
            + suggestion
                .display_override
                .as_ref()
                .map_or(0, String::capacity)
            + suggestion.description.as_ref().map_or(0, String::capacity)
            + suggestion
                .completion_id
                .as_ref()
                .map_or(0, String::capacity)
            + suggestion.match_indices.as_ref().map_or(0, |matches| {
                matches.capacity() * std::mem::size_of::<usize>()
            })
    }

    fn query(text: &str, cursor: usize) -> Query {
        Query {
            text: text.into(),
            cursor,
            session: 1,
            epoch: 1,
            trigger: Trigger::Explicit,
        }
    }

    fn candidate(value: &str, span: std::ops::Range<usize>) -> Candidate {
        Candidate {
            source: Source::Path,
            value: value.into(),
            kind: Kind::File,
            description: None,
            span,
            noquote: false,
            nospace: true,
            matches: Vec::new(),
            display: None,
        }
    }

    #[test]
    fn inline_commands_complete_locally_without_a_worker_or_escaping_prefixes() {
        for (prefix, line, value, completed) in [
            ("#", "#mo", "mode", "#mode"),
            ("##", "##th", "think", "##think"),
            ("问", "  问he", "help", "  问help"),
        ] {
            let state = Arc::new(Mutex::new(None));
            let mut completer = Completion::new(
                Config {
                    inline_prefix: Some(prefix.into()),
                    ..Default::default()
                },
                state.clone(),
                Arc::new(|| {}),
            );
            let CompletionResult::Fresh { suggestions, .. } = completer.complete(line, line.len())
            else {
                panic!("local command completion did not finish");
            };
            assert_eq!(suggestions.len(), 1);
            assert_eq!(suggestions[0].value, value);
            let mut text = line.to_owned();
            text.replace_range(suggestions[0].span.start..suggestions[0].span.end, value);
            assert_eq!(text, completed);
            assert!(matches!(
                completer.poll_completion(),
                CompletionStatus::Idle
            ));
            assert_eq!(
                state.lock().unwrap().as_ref().unwrap().phase,
                Phase::Complete
            );
            assert!(completer.current.is_none());
        }
    }

    #[test]
    fn inline_fixed_arguments_and_empty_results_are_authoritative() {
        let mut completer = Completion::new(
            Config {
                inline_prefix: Some("#".into()),
                ..Default::default()
            },
            Default::default(),
            Arc::new(|| {}),
        );
        for (line, expected) in [
            ("#", 9),
            ("# ", 9),
            ("#mode ", 3),
            ("#think ", 2),
            ("#auto ", 2),
            ("#mode au", 1),
            ("#out invalid", 0),
            ("#fix explain", 0),
            ("#unknown", 0),
        ] {
            let CompletionResult::Fresh { suggestions, .. } = completer.complete(line, line.len())
            else {
                panic!("non-authoritative inline completion: {line}");
            };
            assert_eq!(suggestions.len(), expected, "{line}");
            for suggestion in suggestions.iter() {
                assert!(
                    line.get(suggestion.span.start..suggestion.span.end)
                        .is_some()
                );
                assert!(!suggestion.value.contains('\\'));
            }
        }
        assert!(matches!(
            completer.complete("#mode ", 999),
            CompletionResult::Unavailable { .. }
        ));
    }

    #[test]
    fn quoted_completion_round_trips_controls_unicode_and_line_middle() {
        let mut shell = EmbeddedShell::new(ShellOptions::default()).unwrap();
        for value in [
            "space name",
            "tab\tname",
            "line\nname",
            "carriage\rreturn",
            "a'\"$`\\b",
            "组合e\u{301}👩\u{200d}💻",
        ] {
            for (text, span) in [
                ("x --name=na tail", 9..11),
                ("x 'na' tail", 2..6),
                ("x \"na\" tail", 2..6),
                ("x 'na' tail", 3..5),
                ("x \"na\" tail", 3..5),
            ] {
                let request = query(text, span.end);
                let suggestion = insert(&candidate(value, span.clone()), &request);
                let completed =
                    format!("{}{}{}", &text[..span.start], suggestion, &text[span.end..]);
                let arguments = completed.strip_prefix("x ").unwrap();
                assert_eq!(
                                shell
                                    .run_user_line(&format!(
                                        "capture_completion() {{ CAPTURED=$1; TRAILING=$2; }}; capture_completion {arguments}"
                                    ))
                                    .exit_code,
                                0,
                                "{value:?}: {completed:?}",
                            );
                let expected = if text.contains("--name=") {
                    format!("--name={value}")
                } else {
                    value.into()
                };
                assert_eq!(
                    shell.var("CAPTURED").as_deref(),
                    Some(expected.as_str()),
                    "{completed:?}"
                );
                assert_eq!(shell.var("TRAILING").as_deref(), Some("tail"));
            }
        }
    }

    #[test]
    fn controls_and_truncation_only_change_display_and_mapped_highlights() {
        let mut value = candidate("a\t中\nb\x1b👩\u{200d}💻", 5..6);
        value.matches = vec![2, 4, 6];
        value.description = Some("unsafe\n\t\x1b[31m detail".into());
        let request = query("echo a tail", 6);
        let result = suggestions(&Answer {
            query: request.clone(),
            candidates: vec![value.clone()],
            state: State::Complete,
        });
        let display = result[0].display_value();
        assert_eq!(display, "a\\t中\\nb\\e👩\u{200d}💻");
        assert_eq!(result[0].match_indices.as_deref(), Some(&[3, 6, 9][..]));
        assert!(!display.chars().any(char::is_control));
        assert!(
            !result[0]
                .description
                .as_ref()
                .unwrap()
                .chars()
                .any(char::is_control)
        );
        assert_eq!(result[0].value, insert(&value, &request));
        assert!(!result[0].append_whitespace);

        value.value = "👩\u{200d}💻".repeat(300);
        let result = suggestions(&Answer {
            query: request,
            candidates: vec![value.clone()],
            state: State::Complete,
        });
        assert_eq!(result[0].display_value().graphemes(true).count(), 256);
        assert_eq!(result[0].value, value.value);
    }

    #[test]
    fn insertion_options_and_logical_identity_survive_display_and_type_changes() {
        let request = query("echo fi", 7);
        let mut value = candidate("file name", 5..7);
        value.noquote = true;
        value.nospace = true;
        let answer = |candidate| Answer {
            query: request.clone(),
            candidates: vec![candidate],
            state: State::Complete,
        };
        let initial = suggestions(&answer(value.clone()));
        assert_eq!(initial[0].value, "file name");
        assert!(!initial[0].append_whitespace);
        value.kind = Kind::Directory;
        value.description = Some("new type".into());
        value.display = Some("different display".into());
        value.matches = vec![0, 2];
        let changed = suggestions(&answer(value));
        assert_eq!(initial[0].completion_id, changed[0].completion_id);
        assert!(changed[0].match_indices.as_ref().unwrap().is_empty());
        let mut directory = candidate("file name/", 5..7);
        directory.kind = Kind::Directory;
        let directory = suggestions(&answer(directory));
        assert_eq!(initial[0].completion_id, directory[0].completion_id);
    }

    #[test]
    fn common_prefix_extends_raw_prefix_but_never_overwrites_fuzzy_or_middle_input() {
        for (text, cursor, names, expected) in [
            ("echo buil", 9, ["build.rs", "build-all.sh"], "build"),
            ("echo bd", 7, ["build.rs", "build-all.sh"], ""),
            ("echo all", 8, ["build-all.rs", "build-all.sh"], ""),
            ("echo foo", 8, ["FooBar", "FooBaz"], "FooBa"),
            ("echo Foo", 8, ["foobar", "foobaz"], ""),
            ("echo \"bui", 9, ["build one", "build other"], "\"build o"),
            ("echo buil tail", 9, ["build.rs", "build-all.sh"], "build"),
        ] {
            let request = query(text, cursor);
            let values = suggestions(&Answer {
                query: request.clone(),
                candidates: names
                    .iter()
                    .map(|name| candidate(name, 5..cursor))
                    .collect(),
                state: State::Complete,
            });
            let partial = common_prefix(&request, &values);
            assert_eq!(partial.insert, expected, "{text:?}");
        }
        let request = query("echo build tail", 8);
        let values = suggestions(&Answer {
            query: request.clone(),
            candidates: ["build.rs", "build.sh"]
                .iter()
                .map(|name| candidate(name, 5..10))
                .collect(),
            state: State::Complete,
        });
        assert!(common_prefix(&request, &values).insert.is_empty());
    }

    #[test]
    fn unavailable_completion_status() {
        for enabled in [true, false] {
            let state = Arc::new(Mutex::new(None));
            let mut completion = Completion::new(
                Config {
                    enabled,
                    ..Default::default()
                },
                state.clone(),
                Arc::new(|| {}),
            );
            completion.completion_requested();
            assert!(matches!(
                completion.complete("echo x", 6),
                CompletionResult::Unavailable { .. },
            ));
            let status = state.lock().unwrap();
            assert_eq!(status.as_ref().unwrap().phase, Phase::Unavailable);
            assert!(status.as_ref().unwrap().error.is_some());
            drop(status);
            completion.completion_navigated();
            assert!(!completion.automatic_completion_allowed());
            completion.completion_cancelled();
            assert_eq!(completion.poll_completion(), CompletionStatus::Idle);
        }
    }

    #[test]
    fn abbreviation_selection_uses_exact_expansion_and_notifies_approved_identity() {
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let observed = notifications.clone();
        let mut completion = Completion::new(
            Config {
                selection_observer: Some(Arc::new(move |selection| {
                    observed.lock().unwrap().push(selection.clone());
                })),
                ..Default::default()
            },
            Default::default(),
            Arc::new(|| {}),
        );
        let request = query("go tail", 2);
        let mut rule = candidate("go build 'literal $text'", 0..2);
        rule.kind = Kind::Abbreviation;
        rule.source = Source::Abbreviation {
            name: "go".into(),
            revision: 7,
        };
        rule.display = Some("go".into());
        rule.description = Some("go build 'literal $text' · fixture".into());
        rule.noquote = true;
        rule.matches = vec![0, 1];
        completion.update_values(Arc::new(Answer {
            query: request.clone(),
            candidates: vec![rule.clone()],
            state: State::Complete,
        }));
        let suggestion = completion.values[0].clone();
        assert_eq!(suggestion.value, "go build 'literal $text'");
        assert_eq!(suggestion.display_value(), "go");
        assert_eq!(suggestion.match_indices.as_deref(), Some(&[0, 1][..]));
        assert!(!suggestion.append_whitespace);
        assert!(
            completion.partial(&request).insert.is_empty(),
            "a prefix is not a rule selection"
        );
        assert!(notifications.lock().unwrap().is_empty());
        assert_eq!(
            completion.completion_accepted(&suggestion),
            CompletionAcceptance::SuppressAbbreviationExpansion,
        );
        assert_eq!(
            *notifications.lock().unwrap(),
            [AbbreviationSelection {
                name: "go".into(),
                revision: 7
            }]
        );
        rule.source = Source::Abbreviation {
            name: "go".into(),
            revision: 8,
        };
        completion.update_values(Arc::new(Answer {
            query: request,
            candidates: vec![candidate("gone", 0..2), rule],
            state: State::Complete,
        }));
        assert_eq!(
            completion.completion_accepted(&suggestion),
            CompletionAcceptance::Continue,
            "an old revision cannot select the refreshed rule",
        );
        assert_eq!(
            completion.completion_accepted(&completion.values[0].clone()),
            CompletionAcceptance::Continue,
        );
        assert_eq!(notifications.lock().unwrap().len(), 1);
        assert_eq!(
            completion.completion_accepted(&completion.values[1].clone()),
            CompletionAcceptance::SuppressAbbreviationExpansion,
        );
        assert_eq!(
            notifications.lock().unwrap()[1],
            AbbreviationSelection {
                name: "go".into(),
                revision: 8,
            },
        );
    }

    #[test]
    fn display_generations_and_contextual_ids_stay_bounded_for_large_replies() {
        let request = query("x", 1);
        let value = "👩\u{200d}💻\t中".repeat(200);
        let mut entry = candidate(&value, 0..1);
        entry.source = Source::Script;
        entry.noquote = true;
        entry.matches = (0..256).collect();
        entry.description = Some("👩\u{200d}💻\nlarge description".repeat(300));
        let answer = Answer {
            query: request,
            candidates: (0..MAX_RESULTS)
                .map(|index| {
                    let mut entry = entry.clone();
                    entry.value.push_str(&index.to_string());
                    entry
                })
                .collect(),
            state: State::Complete,
        };
        let values = suggestions(&answer);
        let bytes = values.iter().map(display_bytes).sum::<usize>();
        assert!(
            bytes + MAX_RESULTS * 128 + 256 <= DISPLAY_GENERATION_BYTES,
            "{bytes}"
        );
        assert_eq!(values[0].value, answer.candidates[0].value);
        assert_eq!(
            values[MAX_RESULTS - 1].value,
            answer.candidates[MAX_RESULTS - 1].value
        );
        assert!(values.iter().all(|value| {
            value
                .match_indices
                .as_ref()
                .unwrap()
                .iter()
                .all(|index| *index < value.display_value().graphemes(true).count())
        }));
        assert_eq!(
            values[0].completion_id,
            suggestions(&answer)[0].completion_id
        );
        assert_ne!(values[0].completion_id, values[1].completion_id);
    }
}
