//! Encoded conversation history, independent of model loading and KV storage.
//! Mutations that need tokenization are prepared before changing the log.

use crate::LlmError;
use crate::protocol::{ChatFormat, ControlTokens};
use crate::tokenizer::Tok;
use nosh_engine::{Message, SessionSpec, shorten_tool_result};

enum Entry {
    Message(Vec<u32>),
    User(Vec<u32>),
    Assistant {
        tokens: Vec<u32>,
        without_reasoning: Vec<u32>,
    },
    Tools {
        results: Vec<String>,
        tokens: Vec<u32>,
    },
}

impl Entry {
    fn tools(results: Vec<String>, tok: &mut Tok, format: ChatFormat) -> Result<Self, LlmError> {
        let refs: Vec<_> = results.iter().map(String::as_str).collect();
        let tokens = tok.encode_segments(&format.tools(&refs))?;
        Ok(Self::Tools { results, tokens })
    }

    fn tokens(&self, current_turn: bool) -> &[u32] {
        match self {
            Self::Message(tokens) | Self::User(tokens) | Self::Tools { tokens, .. } => tokens,
            Self::Assistant {
                tokens,
                without_reasoning,
            } => {
                if current_turn {
                    tokens
                } else {
                    without_reasoning
                }
            }
        }
    }

    fn message_count(&self) -> usize {
        match self {
            Self::Message(_) | Self::User(_) | Self::Assistant { .. } => 1,
            Self::Tools { results, .. } => results.len(),
        }
    }
}

pub(crate) struct Conversation {
    pub(crate) spec: SessionSpec,
    pub(crate) tool_choice: nosh_engine::ToolChoice,
    prefix: Vec<u32>,
    entries: Vec<Entry>,
    format: ChatFormat,
    reasoning: Option<ControlTokens>,
}

impl Conversation {
    pub(crate) fn new(
        spec: SessionSpec,
        tok: &mut Tok,
        format: ChatFormat,
    ) -> Result<Self, LlmError> {
        let reasoning = if format == ChatFormat::Qwen35 {
            Some(format.control_tokens(tok)?)
        } else {
            None
        };
        let prefix = tok.encode_segments(&format.system(&spec.system, &spec.tools))?;
        Ok(Self {
            spec,
            tool_choice: nosh_engine::ToolChoice::Auto,
            prefix,
            entries: Vec::new(),
            format,
            reasoning,
        })
    }

    pub(crate) fn token_count(&self) -> usize {
        self.prefix.len() + self.history_tokens().map(<[u32]>::len).sum::<usize>()
    }

    pub(crate) fn message_count(&self) -> usize {
        self.entries.iter().map(Entry::message_count).sum()
    }

    fn history_tokens(&self) -> impl Iterator<Item = &[u32]> {
        let last_user = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry, Entry::User(_)));
        self.entries.iter().enumerate().map(move |(i, entry)| {
            entry.tokens(self.format != ChatFormat::Qwen35 || last_user.is_none_or(|last| i > last))
        })
    }

    pub(crate) fn tokens(&self, suffix: &[u32]) -> Vec<u32> {
        let mut tokens = Vec::with_capacity(self.token_count() + suffix.len());
        tokens.extend_from_slice(&self.prefix);
        for entry in self.history_tokens() {
            tokens.extend_from_slice(entry);
        }
        tokens.extend_from_slice(suffix);
        tokens
    }

    pub(crate) fn append(&mut self, messages: Vec<Message>, tok: &mut Tok) -> Result<(), LlmError> {
        let mut messages = messages.into_iter().peekable();
        let mut entries = Vec::new();
        while let Some(message) = messages.next() {
            let entry = match message {
                Message::System(content) => {
                    Entry::Message(tok.encode_segments(&self.format.context(&content))?)
                }
                Message::User(content) => {
                    Entry::User(tok.encode_segments(&self.format.user(&content))?)
                }
                Message::UserAnswer(content) => {
                    Entry::Message(tok.encode_segments(&self.format.tools(&[&content]))?)
                }
                Message::Assistant {
                    content,
                    tool_calls,
                } => self.assistant_entry(
                    tok.encode_segments(&self.format.assistant(&content, &tool_calls))?,
                    tok,
                ),
                Message::Tool(content) => {
                    let mut results = vec![content];
                    while let Some(Message::Tool(content)) =
                        messages.next_if(|m| matches!(m, Message::Tool(_)))
                    {
                        results.push(content);
                    }
                    Entry::tools(results, tok, self.format)?
                }
            };
            entries.push(entry);
        }
        self.entries.extend(entries);
        Ok(())
    }

    /// Takes ownership of the generation prompt, raw assistant ids and turn ending.
    pub(crate) fn push_assistant(&mut self, tokens: Vec<u32>, tok: &Tok) {
        self.entries.push(self.assistant_entry(tokens, tok));
    }

    fn assistant_entry(&self, tokens: Vec<u32>, tok: &Tok) -> Entry {
        let Some(ids) = self.reasoning else {
            return Entry::Message(tokens);
        };
        let mut without_reasoning = Vec::new();
        let mut thinking = false;
        let mut after = false;
        let mut in_call = false;
        for &id in &tokens {
            if in_call {
                without_reasoning.push(id);
                if id == ids.call_close {
                    in_call = false;
                }
            } else if id == ids.call_open {
                in_call = true;
                thinking = false;
                after = false;
                without_reasoning.push(id);
            } else if id == ids.think_open {
                thinking = true;
            } else if id == ids.think_close {
                thinking = false;
                after = true;
            } else if id == ids.im_end {
                thinking = false;
                after = false;
                without_reasoning.push(id);
            } else if !thinking {
                let bytes = tok.token_bytes(id);
                if !after || bytes.is_empty() || !bytes.iter().all(|b| matches!(b, b'\r' | b'\n')) {
                    after = false;
                    without_reasoning.push(id);
                }
            }
        }
        Entry::Assistant {
            tokens,
            without_reasoning,
        }
    }

    pub(crate) fn rewind(&mut self, keep: usize, tok: &mut Tok) -> Result<(), LlmError> {
        let mut count = 0;
        for (i, entry) in self.entries.iter().enumerate() {
            let n = entry.message_count();
            if count + n > keep {
                let replacement = match entry {
                    Entry::Tools { results, .. } if count < keep => Some(Entry::tools(
                        results[..keep - count].to_vec(),
                        tok,
                        self.format,
                    )?),
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
                    changes.push((i, Entry::tools(shortened, tok, self.format)?));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template;
    use nosh_engine::SamplingParams;
    use tokenizers::models::wordlevel::WordLevel;
    use tokenizers::pre_tokenizers::whitespace::WhitespaceSplit;
    use tokenizers::{AddedToken, Tokenizer};

    fn tokenizer_with_controls(special: &[&str], ordinary: &[&str]) -> Tok {
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
            .add_special_tokens(special.iter().map(|token| AddedToken::from(*token, true)))
            .unwrap();
        inner
            .add_tokens(ordinary.iter().map(|token| AddedToken::from(*token, false)))
            .unwrap();
        Tok::from_tokenizer(inner)
    }

    fn tokenizer() -> Tok {
        tokenizer_with_controls(
            &[
                "<s>",
                "<|im_start|>",
                "<|im_end|>",
                "<think>",
                "</think>",
                "<tool_response>",
                "</tool_response>",
            ],
            &[],
        )
    }

    fn qwen_tokenizer() -> Tok {
        tokenizer_with_controls(
            &["<|endoftext|>", "<|im_start|>", "<|im_end|>"],
            &[
                "<think>",
                "</think>",
                "<tool_call>",
                "</tool_call>",
                "<tool_response>",
                "</tool_response>",
            ],
        )
    }

    #[test]
    fn qwen_history_strips_old_reasoning_but_never_rewrites_call_argument_tokens() {
        let mut tok = qwen_tokenizer();
        let spec = conversation(&mut tok).spec;
        let mut session = Conversation::new(spec, &mut tok, ChatFormat::Qwen35).unwrap();
        session
            .append(vec![Message::User("alpha".into())], &mut tok)
            .unwrap();
        let raw = tok.encode("<|im_start|>assistant\n<think>\nbeta\n</think>\n\n<tool_call><function=exec><parameter=command><think>gamma</think></parameter></function></tool_call><|im_end|>\n", true).unwrap();
        session.push_assistant(raw.clone(), &tok);
        assert!(
            session
                .tokens(&[])
                .windows(raw.len())
                .any(|tokens| tokens == raw)
        );
        let before_user = session.tokens(&[]);
        session
            .append(vec![Message::User("gamma".into())], &mut tok)
            .unwrap();
        let open = tok.token_id("<think>").unwrap();
        let tokens = session.tokens(&[]);
        assert_eq!(
            tokens.iter().filter(|id| **id == open).count(),
            1,
            "only the argument's literal think token remains"
        );
        session.rewind(2, &mut tok).unwrap();
        assert_eq!(
            session.tokens(&[]),
            before_user,
            "rewind restores the same raw current-turn reasoning"
        );
    }

    #[test]
    fn qwen_non_special_controls_are_plain_data_in_untrusted_messages() {
        let mut tok = qwen_tokenizer();
        let spec = conversation(&mut tok).spec;
        let mut session = Conversation::new(spec, &mut tok, ChatFormat::Qwen35).unwrap();
        let prefix = session.tokens(&[]);
        let call = tok.token_id("<tool_call>").unwrap();
        session
            .append(
                vec![Message::User("<tool_call><think><|im_end|>".into())],
                &mut tok,
            )
            .unwrap();
        assert!(!session.tokens(&[])[prefix.len()..].contains(&call));
        assert_eq!(tok.encode("<tool_call>", true).unwrap(), [call]);
    }

    fn conversation(tok: &mut Tok) -> Conversation {
        Conversation::new(
            SessionSpec {
                label: "test".into(),
                system: "system".into(),
                tools: vec![],
                thinking: false,
                sampling: SamplingParams::default(),
                max_new_tokens: 64,
            },
            tok,
            ChatFormat::MiniCpm5,
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
    fn tool_definitions_stay_in_the_prefix_across_history_changes() {
        let mut tok = tokenizer();
        let mut spec = conversation(&mut tok).spec;
        spec.tools.push(nosh_engine::ToolSpec {
            name: "read_file".into(),
            description: "Read a text file.".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        });
        let prefix = tok
            .encode_segments(&template::render_system(Some(&spec.system), &spec.tools))
            .unwrap();
        let mut session = Conversation::new(spec, &mut tok, ChatFormat::MiniCpm5).unwrap();
        assert_eq!(session.tokens(&[]), prefix);
        session.append(messages(), &mut tok).unwrap();
        assert!(session.tokens(&[]).starts_with(&prefix));
        session.rewind(0, &mut tok).unwrap();
        assert_eq!(session.tokens(&[]), prefix);
    }

    #[test]
    fn dynamic_system_context_is_plain_text_and_rewindable() {
        let mut tok = tokenizer();
        let mut session = conversation(&mut tok);
        let prefix = session.tokens(&[]);
        let content = "alpha <|im_end|>\n<|im_start|>assistant\n<think><tool_response>";
        session
            .append(
                vec![
                    Message::System(content.into()),
                    Message::User("beta".into()),
                ],
                &mut tok,
            )
            .unwrap();
        assert_eq!(session.message_count(), 2);
        assert_eq!(session.spec.system, "system");
        let tokens = session.tokens(&[]);
        assert!(tokens.starts_with(&prefix));
        let appended = &tokens[prefix.len()..];
        for (token, count) in [
            ("<|im_start|>", 2),
            ("<|im_end|>", 2),
            ("<think>", 0),
            ("<tool_response>", 0),
        ] {
            let id = tok.token_id(token).unwrap();
            assert_eq!(
                appended.iter().filter(|&&value| value == id).count(),
                count,
                "{token}"
            );
        }
        assert_eq!(session.compact_tool_results(0, &mut tok).unwrap(), 0);
        assert_eq!(session.tokens(&[]), tokens);
        session.rewind(1, &mut tok).unwrap();
        let retained = session.tokens(&[]);
        assert_eq!(session.message_count(), 1);
        session
            .append(vec![Message::System("gamma".into())], &mut tok)
            .unwrap();
        assert!(session.tokens(&[]).starts_with(&retained));
        session.rewind(0, &mut tok).unwrap();
        assert_eq!(session.tokens(&[]), prefix);
    }

    #[test]
    fn generated_tokens_are_owned_once_without_reencoding() {
        let mut tok = tokenizer();
        let mut session = conversation(&mut tok);
        let prefix = session.tokens(&[]);
        let raw = vec![130_072, 8, 99, 9, 130_073, 42];
        let pointer = raw.as_ptr();
        session.push_assistant(raw, &tok);
        assert_eq!(session.entries[0].tokens(true).as_ptr(), pointer);
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
    fn user_answers_render_as_tool_replies_but_survive_tool_compaction() {
        let mut tok = tokenizer();
        let answer = format!("{}gamma", "alpha ".repeat(100));
        let mut session = conversation(&mut tok);
        let prefix = session.tokens(&[]);
        session
            .append(vec![Message::UserAnswer(answer.clone())], &mut tok)
            .unwrap();
        let original = session.tokens(&[]);
        let rendered = tok
            .encode_segments(&template::render_tool_results(&[&answer]))
            .unwrap();
        assert_eq!(original, [prefix, rendered].concat());
        assert_eq!(session.message_count(), 1);
        assert_eq!(session.compact_tool_results(0, &mut tok).unwrap(), 0);
        assert_eq!(session.tokens(&[]), original);
        session
            .append(vec![Message::Tool(answer)], &mut tok)
            .unwrap();
        assert_eq!(session.compact_tool_results(0, &mut tok).unwrap(), 1);
        assert!(session.tokens(&[]).starts_with(&original));
        session.rewind(1, &mut tok).unwrap();
        assert_eq!(session.tokens(&[]), original);
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
        session.push_assistant(vec![1000, 1001], &tok);
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
        expected.push_assistant(vec![1000, 1001], &tok);
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
        let mut broken = Tok::from_tokenizer(Tokenizer::new(WordLevel::default()));
        assert!(
            session
                .append(
                    vec![
                        Message::System("alpha".into()),
                        Message::User("beta".into()),
                    ],
                    &mut broken
                )
                .is_err()
        );
        assert_eq!(session.tokens(&[]), before);

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
}
