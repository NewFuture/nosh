use super::*;
use reedline::{EditCommand, ReedlineEvent};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Correction {
    pub version: Version,
    pub range: Range<usize>,
    pub edit_range: Range<usize>,
    pub from: String,
    pub to: String,
}

fn command_name(word: &str) -> bool {
    (2..=32).contains(&word.len())
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-+.".contains(&b))
}

impl Correction {
    pub(super) fn propose(
        input: &Input,
        queries: &[Query],
        observations: &[Observation],
        index: &Index,
    ) -> Option<Self> {
        if !index.complete || index.cwd != input.context.cwd || index.path != input.context.path {
            return None;
        }
        let query = queries
            .iter()
            .zip(observations)
            .find_map(|(query, observation)| {
                (query.definite
                    && matches!(&query.kind, QueryKind::Command { path, .. } if *path == input.context.path)
                    && command_name(&query.word)
                    && observation.finding.as_ref().is_some_and(|finding| {
                        finding.state == State::Error
                            && matches!(finding.reason, Reason::MissingCommand)
                            && finding.range == query.range
                    }))
                .then_some(query)
            })?;
        let mut argv: Vec<String> = input
            .text
            .get(query.range.start..)?
            .split_whitespace()
            .take(3)
            .map(str::to_owned)
            .collect();
        *argv.first_mut()? = query.word.clone();
        if crate::trigger::looks_like_question(&argv) {
            return None;
        }
        let raw = input.text.get(query.range.clone())?;
        let edit_range = if raw == query.word {
            query.range.clone()
        } else if raw.len() == query.word.len() + 2
            && raw.starts_with(['\'', '"'])
            && raw.as_bytes()[0] == *raw.as_bytes().last()?
            && raw.get(1..raw.len() - 1) == Some(query.word.as_str())
        {
            query.range.start + 1..query.range.end - 1
        } else {
            return None;
        };
        let mut names: Vec<String> = index
            .names
            .iter()
            .chain(input.context.builtins.iter())
            .chain(input.context.aliases.iter())
            .chain(input.context.functions.iter())
            .filter(|name| command_name(name))
            .cloned()
            .collect();
        names.sort_unstable();
        names.dedup();
        let to = crate::spell::confident_match(&query.word, &names)?.to_owned();
        Some(Self {
            version: input.version,
            range: query.range.clone(),
            edit_range,
            from: query.word.clone(),
            to,
        })
    }

    pub(super) fn matches(&self, input: &Input) -> bool {
        self.version == input.version
            && command_name(&self.from)
            && command_name(&self.to)
            && self.edit_range.start >= self.range.start
            && self.edit_range.end <= self.range.end
            && input.text.get(self.range.clone()).is_some()
            && input.text.get(self.edit_range.clone()) == Some(self.from.as_str())
    }

    pub(super) fn query(&self, context: &Context) -> Query {
        Query {
            range: self.range.clone(),
            word: self.to.clone(),
            kind: QueryKind::Command {
                path: context.path.clone(),
                ai_on_missing: false,
            },
            definite: true,
        }
    }

    pub(super) fn edit(&self) -> ReedlineEvent {
        ReedlineEvent::Edit(vec![
            EditCommand::MoveToPosition {
                position: self.edit_range.start,
                select: false,
            },
            EditCommand::MoveToPosition {
                position: self.edit_range.end,
                select: true,
            },
            EditCommand::InsertString(self.to.clone()),
            EditCommand::MoveToEnd { select: false },
        ])
    }
}

pub(crate) fn right_navigation(event: &ReedlineEvent) -> bool {
    matches!(event, ReedlineEvent::UntilFound(events) if matches!(events.as_slice(),
        [ReedlineEvent::HistoryHintComplete, ReedlineEvent::MenuRight, ReedlineEvent::Right]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_assist::tests::Fixture;
    use std::os::unix::fs::PermissionsExt;

    fn propose(fixture: &Fixture, text: &str, names: &[&str]) -> (Input, Option<Correction>) {
        let mut input = fixture.input(text);
        Arc::make_mut(&mut input.context).ai_enabled = false;
        let analysis = super::super::analysis::analyze(&input);
        let observations = super::super::lookup::Lookup::default().run(&input, &analysis.queries);
        let index = Index {
            cwd: input.context.cwd.clone(),
            path: input.context.path.clone(),
            names: Arc::new(names.iter().map(|name| name.to_string()).collect()),
            complete: true,
            reason: None,
        };
        let proposal = Correction::propose(&input, &analysis.queries, &observations, &index);
        (input, proposal)
    }

    #[test]
    fn native_edits_preserve_quotes_unicode_parameters_and_compound_tail_and_undo() {
        let fixture = Fixture::new();
        for text in [
            "gti status -- '中文 e\u{301} 👩\u{200d}💻'",
            "  'gti' -- \"quoted args\" && echo 'tail; unchanged'",
            "\"gti\" status\n",
        ] {
            let (input, proposal) = propose(&fixture, text, &["git", "gtk"]);
            let proposal = proposal.unwrap();
            assert!(super::super::lookup::Lookup::default().confirm_correction(&input, &proposal));
            let mut editor = reedline::Reedline::create();
            editor.run_edit_commands(&[EditCommand::InsertString(text.into())]);
            let ReedlineEvent::Edit(commands) = proposal.edit() else {
                panic!("not an edit")
            };
            editor.run_edit_commands(&commands);
            assert_eq!(
                editor.current_buffer_contents(),
                text.replacen("gti", "git", 1)
            );
            assert_eq!(
                editor.current_insertion_point(),
                editor.current_buffer_contents().len()
            );
            editor.run_edit_commands(&[EditCommand::Undo]);
            assert_eq!(editor.current_buffer_contents(), text);
        }
    }

    #[test]
    fn dynamic_commands_overridden_path_prior_effects_and_questions_are_not_guessed() {
        let fixture = Fixture::new();
        for text in [
            "$(echo gti) status",
            "PATH=/other gti status",
            "touch changed; gti status",
            "can you help",
        ] {
            assert!(
                propose(&fixture, text, &["git", "cat"]).1.is_none(),
                "{text}"
            );
        }
        let (mut input, proposal) = propose(&fixture, "gti status", &["git"]);
        let proposal = proposal.unwrap();
        input.version.input += 1;
        assert!(!proposal.matches(&input));
    }

    #[test]
    fn index_names_alone_do_not_prove_executability_and_builtin_confirmation_is_local() {
        let fixture = Fixture::new();
        let (input, proposal) = propose(&fixture, "gti status", &["git"]);
        let proposal = proposal.unwrap();
        let target = std::path::Path::new(input.context.path.as_deref().unwrap()).join("git");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!super::super::lookup::Lookup::default().confirm_correction(&input, &proposal));
        let (input, proposal) = propose(&fixture, "ehco 'no execution'", &[]);
        let proposal = proposal.unwrap();
        let mut lookup = super::super::lookup::Lookup::default();
        assert!(lookup.confirm_correction(&input, &proposal));
        assert_eq!(lookup.stats().metadata_calls, 0);
    }
}
