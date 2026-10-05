use std::ops::Range;
use std::sync::Arc;

use brush_core::completion::CompletionToken;
use brush_parser::Token;
use serde::{Deserialize, Serialize};

use super::types::{NativeSnapshot, Query};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Context {
    pub words: Arc<[String]>,
    pub index: usize,
    pub word: String,
    pub span: Range<usize>,
    pub command_start: usize,
    pub command_end: usize,
    pub quote: Option<char>,
    pub redirect: bool,
    pub path: Option<String>,
    pub requires_execution: bool,
}

pub(super) struct Words {
    pub values: Vec<String>,
    pub starts: Vec<usize>,
    pub index: usize,
    pub word: String,
    pub span: Range<usize>,
}

impl Words {
    fn parse(query: &Query, start: usize, end: usize, delimiters: &[char]) -> Result<Self, String> {
        let raw = brush_core::completion::simple_tokenize_by_delimiters(
            &query.text[start..end],
            delimiters,
        );
        let cursor = query.cursor - start;
        let current = raw
            .iter()
            .position(|token| token.start <= cursor && cursor <= token.end());
        let index =
            current.unwrap_or_else(|| raw.iter().take_while(|token| token.end() < cursor).count());
        let span = current.map_or(query.cursor..query.cursor, |index| {
            start + raw[index].start..start + raw[index].end()
        });
        let fragment = query
            .text
            .get(span.start..query.cursor)
            .ok_or("invalid completion word")?;
        if fragment.len() > super::types::MAX_WORD {
            return Err("completion word limit".into());
        }
        let mut values: Vec<_> = raw
            .iter()
            .map(|token| brush_parser::unquote_str(token.text))
            .collect();
        let mut starts: Vec<_> = raw.iter().map(|token| token.start).collect();
        if current.is_none() {
            values.insert(index, String::new());
            starts.insert(index, cursor);
        }
        Ok(Self {
            values,
            starts,
            index,
            word: brush_parser::unquote_str(fragment),
            span,
        })
    }

    pub fn tokens(&self) -> Vec<CompletionToken<'_>> {
        self.values
            .iter()
            .zip(&self.starts)
            .map(|(text, start)| CompletionToken {
                text,
                start: *start,
            })
            .collect()
    }
}

fn assignment(word: &str) -> Option<(&str, &str)> {
    let (name, value) = word.split_once('=')?;
    brush_core::env::valid_variable_name(name).then_some((name, value))
}

impl Context {
    pub fn parse(query: &Query, snapshot: &NativeSnapshot) -> Result<Self, String> {
        if query.text.len() > super::types::MAX_INPUT || !query.text.is_char_boundary(query.cursor)
        {
            return Err("completion input or cursor limit".into());
        }
        let options = snapshot.context.options();
        let all =
            brush_core::completion::simple_tokenize_by_delimiters(&query.text, &[' ', '\t', '\n']);
        let word_start = all
            .iter()
            .find(|word| word.start <= query.cursor && query.cursor <= word.end())
            .map_or(query.cursor, |word| word.start);
        let tokens = brush_parser::uncached_tokenize_str(&query.text, &options.tokenizer_options())
            .or_else(|_| {
                brush_parser::uncached_tokenize_str(
                    &query.text[..word_start],
                    &options.tokenizer_options(),
                )
            })
            .map_err(|error| format!("completion context: {error}"))?;
        let mut command_start = 0;
        let mut command_end = query.text.len();
        let mut redirect = false;
        let offsets: Vec<_> = query
            .text
            .char_indices()
            .map(|(offset, _)| offset)
            .collect();
        let byte = |index| offsets.get(index).copied().unwrap_or(query.text.len());
        for token in &tokens {
            match token {
                Token::Operator(operator, location)
                    if matches!(
                        operator.as_str(),
                        ";" | "|" | "|&" | "||" | "&&" | "&" | "\n" | "("
                    ) =>
                {
                    let start = byte(location.start.index);
                    let end = byte(location.end.index);
                    if end <= query.cursor {
                        command_start = end;
                        redirect = false;
                    } else if start >= query.cursor {
                        command_end = start;
                        break;
                    }
                }
                Token::Operator(operator, location)
                    if byte(location.start.index) < query.cursor
                        && matches!(operator.as_str(), "<" | ">" | ">>" | "<>" | ">|" | "<<<") =>
                {
                    redirect = true
                }
                Token::Word(_, location) if byte(location.end.index) < word_start => {
                    redirect = false
                }
                _ => {}
            }
        }
        let line = query
            .text
            .get(command_start..command_end)
            .ok_or("invalid command boundary")?;
        let cursor = query.cursor - command_start;
        let raw = brush_core::completion::simple_tokenize_by_delimiters(line, &[' ', '\t', '\n']);
        let mut path = snapshot.context.path.clone();
        let mut skipped = raw
            .iter()
            .take_while(|token| {
                token.end() < cursor
                    && matches!(
                        token.text,
                        "if" | "then" | "elif" | "else" | "while" | "until" | "do" | "{" | "!"
                    )
            })
            .count();
        for token in raw.iter().skip(skipped) {
            if token.end() >= cursor {
                break;
            }
            let Some((name, value)) = assignment(token.text) else {
                break;
            };
            if name == "PATH" {
                if value.contains(['$', '`']) {
                    return Err("dynamic PATH assignment cannot be completed statically".into());
                }
                path = Some(brush_parser::unquote_str(value));
            }
            skipped += 1;
        }
        if skipped > 0 {
            command_start += raw.get(skipped).map_or(cursor, |token| token.start);
        }
        let words = Words::parse(query, command_start, command_end, &[' ', '\t', '\n'])?;
        let fragment = &query.text[words.span.start..query.cursor];
        let quote = fragment
            .chars()
            .next()
            .filter(|character| matches!(character, '\'' | '"'));
        let mut context = Self {
            words: words.values.into(),
            index: words.index,
            word: words.word,
            span: words.span,
            command_start,
            command_end,
            quote,
            redirect,
            path,
            requires_execution: false,
        };
        context.requires_execution = context.needs_script(snapshot)
            || (context.quote != Some('\'')
                && context.word.contains(['$', '`'])
                && context.word.contains('/'));
        Ok(context)
    }

    pub fn command(&self) -> Option<&str> {
        self.words
            .first()
            .map(String::as_str)
            .filter(|word| !word.is_empty())
    }

    pub fn needs_script(&self, snapshot: &NativeSnapshot) -> bool {
        if self.redirect || !snapshot.scripts {
            return false;
        }
        let registry = &snapshot.registry;
        if registry.overflow {
            return true;
        }
        if self.command().is_none() {
            registry.empty
        } else if self.index == 0 {
            registry.initial
        } else {
            registry.default
                || self.command().is_some_and(|command| {
                    registry.names.contains(command)
                        || std::path::Path::new(command)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| registry.names.contains(name))
                })
        }
    }

    pub fn valid_for(&self, query: &Query) -> bool {
        self.index < self.words.len()
            && self.command_start <= self.span.start
            && self.span.start <= query.cursor
            && query.cursor <= self.span.end
            && self.span.end <= self.command_end
            && query
                .text
                .get(self.command_start..self.command_end)
                .is_some()
            && query.text.get(self.span.clone()).is_some()
            && self.word.len() <= super::types::MAX_WORD
    }

    pub(super) fn script_words(
        &self,
        query: &Query,
        snapshot: &NativeSnapshot,
    ) -> Result<Words, String> {
        let delimiters: Vec<_> = snapshot.word_breaks.chars().collect();
        Words::parse(query, self.command_start, self.command_end, &delimiters)
    }
}
