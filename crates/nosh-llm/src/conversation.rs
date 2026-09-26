//! Encoded conversation history, independent of model loading and KV storage.
//! Mutations that need tokenization are prepared before changing the log.

use crate::LlmError;
use crate::engine::{Message, SessionSpec};
use crate::template;
use crate::tokenizer::Tok;

enum Entry {
    Message(Vec<u32>),
    Tools {
        results: Vec<String>,
        tokens: Vec<u32>,
    },
}

impl Entry {
    fn tools(results: Vec<String>, tok: &mut Tok) -> Result<Self, LlmError> {
        let refs: Vec<_> = results.iter().map(String::as_str).collect();
        let tokens = tok.encode_segments(&template::render_tool_results(&refs))?;
        Ok(Self::Tools { results, tokens })
    }

    fn tokens(&self) -> &[u32] {
        match self {
            Self::Message(tokens) | Self::Tools { tokens, .. } => tokens,
        }
    }

    fn message_count(&self) -> usize {
        match self {
            Self::Message(_) => 1,
            Self::Tools { results, .. } => results.len(),
        }
    }
}

pub(crate) struct Conversation {
    pub(crate) spec: SessionSpec,
    prefix: Vec<u32>,
    entries: Vec<Entry>,
}

impl Conversation {
    pub(crate) fn new(spec: SessionSpec, tok: &mut Tok) -> Result<Self, LlmError> {
        let prefix =
            tok.encode_segments(&template::render_system(Some(&spec.system), &spec.tools))?;
        Ok(Self {
            spec,
            prefix,
            entries: Vec::new(),
        })
    }

    pub(crate) fn token_count(&self) -> usize {
        self.prefix.len() + self.entries.iter().map(|e| e.tokens().len()).sum::<usize>()
    }

    pub(crate) fn message_count(&self) -> usize {
        self.entries.iter().map(Entry::message_count).sum()
    }

    pub(crate) fn tokens(&self, suffix: &[u32]) -> Vec<u32> {
        let mut tokens = Vec::with_capacity(self.token_count() + suffix.len());
        tokens.extend_from_slice(&self.prefix);
        for entry in &self.entries {
            tokens.extend_from_slice(entry.tokens());
        }
        tokens.extend_from_slice(suffix);
        tokens
    }

    pub(crate) fn append(&mut self, messages: Vec<Message>, tok: &mut Tok) -> Result<(), LlmError> {
        let mut messages = messages.into_iter().peekable();
        let mut entries = Vec::new();
        while let Some(message) = messages.next() {
            let entry = match message {
                Message::System(_) => {
                    return Err(LlmError::Config("system messages are set at open()".into()));
                }
                Message::User(content) => {
                    Entry::Message(tok.encode_segments(&template::render_user(&content))?)
                }
                Message::Assistant {
                    content,
                    tool_calls,
                } => Entry::Message(
                    tok.encode_segments(&template::render_assistant(&content, &tool_calls))?,
                ),
                Message::Tool(content) => {
                    let mut results = vec![content];
                    while let Some(Message::Tool(content)) =
                        messages.next_if(|m| matches!(m, Message::Tool(_)))
                    {
                        results.push(content);
                    }
                    Entry::tools(results, tok)?
                }
            };
            entries.push(entry);
        }
        self.entries.extend(entries);
        Ok(())
    }

    /// Takes ownership of the generation prompt, raw assistant ids and turn ending.
    pub(crate) fn push_assistant(&mut self, tokens: Vec<u32>) {
        self.entries.push(Entry::Message(tokens));
    }

    pub(crate) fn rewind(&mut self, keep: usize, tok: &mut Tok) -> Result<(), LlmError> {
        let mut count = 0;
        for (i, entry) in self.entries.iter().enumerate() {
            let n = entry.message_count();
            if count + n > keep {
                let replacement = match entry {
                    Entry::Tools { results, .. } if count < keep => {
                        Some(Entry::tools(results[..keep - count].to_vec(), tok)?)
                    }
                    _ => None,
                };
                self.entries.truncate(i);
                self.entries.extend(replacement);
                return Ok(());
            }
            count += n;
        }
        Ok(())
    }

    pub(crate) fn compact_tool_results(
        &mut self,
        keep_recent: usize,
        tok: &mut Tok,
    ) -> Result<usize, LlmError> {
        let cutoff = self.message_count().saturating_sub(keep_recent);
        let mut seen = 0;
        let mut changes = Vec::new();
        let mut changed = 0;
        for (i, entry) in self.entries.iter().enumerate() {
            seen += entry.message_count();
            // Keep a whole template turn if it overlaps the recent messages.
            if seen <= cutoff
                && let Entry::Tools { results, .. } = entry
            {
                let shortened: Vec<_> = results.iter().map(|s| shorten_tool_result(s)).collect();
                let n = results
                    .iter()
                    .zip(&shortened)
                    .filter(|(a, b)| a != b)
                    .count();
                if n > 0 {
                    changes.push((i, Entry::tools(shortened, tok)?));
                    changed += n;
                }
            }
        }
        for (i, entry) in changes {
            self.entries[i] = entry;
        }
        Ok(changed)
    }
}

/// One-line stand-in for an old tool result (keeps the status header).
pub fn shorten_tool_result(content: &str) -> String {
    const KEEP: usize = 200;
    let count = content.chars().count();
    if count <= KEEP + 40 {
        return content.to_string();
    }
    let end = content.char_indices().nth(KEEP).expect("long result").0;
    format!(
        "{}\n[\u{2026} older output omitted to save context ({count} chars)]",
        &content[..end]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SamplingParams;
    use tokenizers::models::wordlevel::WordLevel;
    use tokenizers::pre_tokenizers::whitespace::WhitespaceSplit;
    use tokenizers::{AddedToken, Tokenizer};

    fn tokenizer() -> Tok {
        let vocab = [
            "[UNK]",
            "system",
            "user",
            "assistant",
            "alpha",
            "beta",
            "gamma",
        ]
        .into_iter()
        .enumerate()
        .map(|(i, word)| (word.to_string(), i as u32))
        .collect();
        let model = WordLevel::builder()
            .vocab(vocab)
            .unk_token("[UNK]".into())
            .build()
            .unwrap();
        let mut inner = Tokenizer::new(model);
        inner.with_pre_tokenizer(Some(WhitespaceSplit));
        inner
            .add_special_tokens(
                [
                    "<s>",
                    "<|im_start|>",
                    "<|im_end|>",
                    "<think>",
                    "</think>",
                    "<tool_response>",
                    "</tool_response>",
                ]
                .map(|s| AddedToken::from(s, true)),
            )
            .unwrap();
        Tok::from_tokenizer(inner)
    }

    fn conversation(tok: &mut Tok) -> Conversation {
        Conversation::new(
            SessionSpec {
                system: "system".into(),
                tools: vec![],
                thinking: false,
                sampling: SamplingParams::default(),
                max_new_tokens: 64,
            },
            tok,
        )
        .unwrap()
    }

    fn messages() -> Vec<Message> {
        vec![
            Message::User("alpha".into()),
            Message::Assistant {
                content: "beta".into(),
                tool_calls: vec![],
            },
            Message::Tool("alpha".into()),
            Message::Tool("beta".into()),
            Message::Tool("gamma".into()),
            Message::User("gamma".into()),
        ]
    }

    #[test]
    fn append_matches_template_tokens_and_counts_logical_messages() {
        let mut tok = tokenizer();
        let mut session = conversation(&mut tok);
        let prefix = session.tokens(&[]);
        let messages = messages();
        let expected = tok
            .encode_segments(&template::render_messages(&messages))
            .unwrap();
        session.append(messages, &mut tok).unwrap();
        assert_eq!(session.message_count(), 6);
        assert_eq!(session.entries.len(), 4);
        assert_eq!(session.tokens(&[]), [prefix, expected].concat());
        let suffix = [1001, 1002];
        let tokens = session.tokens(&suffix);
        assert_eq!(tokens.len(), session.token_count() + suffix.len());
        assert!(tokens.ends_with(&suffix));
    }

    #[test]
    fn generated_tokens_are_owned_once_without_reencoding() {
        let mut tok = tokenizer();
        let mut session = conversation(&mut tok);
        let prefix = session.tokens(&[]);
        let raw = vec![130_072, 8, 99, 9, 130_073, 42];
        let pointer = raw.as_ptr();
        session.push_assistant(raw);
        assert_eq!(session.entries[0].tokens().as_ptr(), pointer);
        assert_eq!(
            session.tokens(&[]),
            [prefix, vec![130_072, 8, 99, 9, 130_073, 42]].concat()
        );
        assert_eq!(session.message_count(), 1);
    }

    #[test]
    fn rewind_splits_tool_groups_at_every_message_boundary() {
        for keep in 0..=7 {
            let mut tok = tokenizer();
            let mut session = conversation(&mut tok);
            session.append(messages(), &mut tok).unwrap();
            session.rewind(keep, &mut tok).unwrap();
            let mut expected = conversation(&mut tok);
            expected
                .append(messages().into_iter().take(keep).collect(), &mut tok)
                .unwrap();
            assert_eq!(session.tokens(&[]), expected.tokens(&[]), "keep={keep}");
            assert_eq!(session.message_count(), keep.min(6));
            session.rewind(usize::MAX, &mut tok).unwrap();
            assert_eq!(session.tokens(&[]), expected.tokens(&[]));
        }
    }

    #[test]
    fn separate_appends_do_not_merge_tool_turns() {
        let mut tok = tokenizer();
        let mut session = conversation(&mut tok);
        session
            .append(vec![Message::Tool("alpha".into())], &mut tok)
            .unwrap();
        session
            .append(vec![Message::Tool("beta".into())], &mut tok)
            .unwrap();
        assert_eq!(session.entries.len(), 2);
        session.rewind(1, &mut tok).unwrap();
        assert_eq!(session.message_count(), 1);
    }

    #[test]
    fn compaction_counts_changed_results_and_keeps_overlapping_groups() {
        let mut tok = tokenizer();
        let mut session = conversation(&mut tok);
        let long = "alpha ".repeat(100);
        session
            .append(
                vec![
                    Message::Tool(long.clone()),
                    Message::Tool(long.clone()),
                    Message::Tool("beta".into()),
                ],
                &mut tok,
            )
            .unwrap();
        let original = session.tokens(&[]);
        assert_eq!(session.compact_tool_results(1, &mut tok).unwrap(), 0);
        assert_eq!(session.tokens(&[]), original);
        assert_eq!(
            session.compact_tool_results(usize::MAX, &mut tok).unwrap(),
            0
        );
        session.push_assistant(vec![1000, 1001]);
        assert_eq!(session.compact_tool_results(1, &mut tok).unwrap(), 2);
        let mut expected = conversation(&mut tok);
        expected
            .append(
                vec![
                    Message::Tool(shorten_tool_result(&long)),
                    Message::Tool(shorten_tool_result(&long)),
                    Message::Tool("beta".into()),
                ],
                &mut tok,
            )
            .unwrap();
        expected.push_assistant(vec![1000, 1001]);
        assert_eq!(session.tokens(&[]), expected.tokens(&[]));
        assert_eq!(session.message_count(), 4);
        assert!(session.token_count() < original.len());
    }

    #[test]
    fn failed_mutations_leave_the_conversation_unchanged() {
        let mut tok = tokenizer();
        let mut session = conversation(&mut tok);
        session
            .append(
                vec![
                    Message::User("alpha".into()),
                    Message::Tool("beta ".repeat(100)),
                    Message::Tool("gamma ".repeat(100)),
                ],
                &mut tok,
            )
            .unwrap();
        let before = session.tokens(&[]);
        assert!(
            session
                .append(
                    vec![
                        Message::User("beta".into()),
                        Message::System("not allowed".into()),
                    ],
                    &mut tok
                )
                .is_err()
        );
        assert_eq!(session.tokens(&[]), before);

        let mut broken = Tok::from_tokenizer(Tokenizer::new(WordLevel::default()));
        assert!(
            session
                .append(vec![Message::User("gamma".into())], &mut broken)
                .is_err()
        );
        assert_eq!(session.tokens(&[]), before);
        assert!(session.rewind(2, &mut broken).is_err());
        assert_eq!(session.tokens(&[]), before);
        assert!(session.compact_tool_results(0, &mut broken).is_err());
        assert_eq!(session.tokens(&[]), before);
        assert_eq!(session.message_count(), 3);
    }

    #[test]
    fn shortening_preserves_unicode_and_the_existing_marker() {
        let short = "\u{4e2d}".repeat(240);
        assert_eq!(shorten_tool_result(&short), short);
        let long = "\u{4e2d}".repeat(241);
        assert_eq!(
            shorten_tool_result(&long),
            format!(
                "{}\n[\u{2026} older output omitted to save context (241 chars)]",
                "\u{4e2d}".repeat(200)
            )
        );
    }
}
