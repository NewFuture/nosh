use std::ops::Range;

use brush_core::completion::CompletionToken;
use brush_parser::Token;

use super::types::{NativeSnapshot, Query};

#[derive(Debug, Clone)]
pub(crate) struct Context {
    pub words: Vec<String>,
    pub index: usize,
    pub word: String,
    pub span: Range<usize>,
    pub command: Option<String>,
    pub command_start: usize,
    pub command_end: usize,
    pub quote: Option<char>,
    pub redirect: bool,
    pub path: Option<String>,
}

fn byte(text: &str, character: usize) -> usize {
    text.char_indices()
        .map(|(offset, _)| offset)
        .nth(character)
        .unwrap_or(text.len())
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
        for token in &tokens {
            match token {
                Token::Operator(operator, location)
                    if matches!(
                        operator.as_str(),
                        ";" | "|" | "||" | "&&" | "&" | "\n" | "("
                    ) =>
                {
                    let start = byte(&query.text, location.start.index);
                    let end = byte(&query.text, location.end.index);
                    if end <= query.cursor {
                        command_start = end;
                        redirect = false;
                    } else if start >= query.cursor {
                        command_end = start;
                        break;
                    }
                }
                Token::Operator(operator, location)
                    if byte(&query.text, location.start.index) < query.cursor
                        && matches!(operator.as_str(), "<" | ">" | ">>" | "<>" | ">|" | "<<<") =>
                {
                    redirect = true
                }
                Token::Word(_, location) if byte(&query.text, location.end.index) < word_start => {
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
        let mut raw =
            brush_core::completion::simple_tokenize_by_delimiters(line, &[' ', '\t', '\n']);
        let mut path = snapshot.context.path.clone();
        let mut skipped = 0;
        for token in &raw {
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
            raw.drain(..skipped);
            let offset = command_start - (query.cursor - cursor);
            for token in &mut raw {
                token.start -= offset;
            }
        }
        let cursor = query.cursor - command_start;
        let index = raw
            .iter()
            .position(|word| word.start <= cursor && cursor <= word.end())
            .unwrap_or_else(|| raw.iter().take_while(|word| word.end() < cursor).count());
        let token = raw
            .get(index)
            .filter(|token| token.start <= cursor && cursor <= token.end());
        let start = token.map_or(cursor, |token| token.start);
        let end = token.map_or(cursor, |token| token.end());
        let fragment = query
            .text
            .get(command_start + start..query.cursor)
            .ok_or("invalid completion word")?;
        if fragment.len() > super::types::MAX_WORD {
            return Err("completion word limit".into());
        }
        let quote = fragment
            .chars()
            .next()
            .filter(|character| matches!(character, '\'' | '"'));
        let mut words: Vec<_> = raw
            .iter()
            .map(|token| brush_parser::unquote_str(token.text))
            .collect();
        if token.is_none() {
            words.insert(index, String::new());
        }
        let command = words.first().filter(|word| !word.is_empty()).cloned();
        Ok(Self {
            words,
            index,
            word: brush_parser::unquote_str(fragment),
            span: command_start + start..command_start + end,
            command,
            command_start,
            command_end,
            quote,
            redirect,
            path,
        })
    }

    pub fn needs_script(&self, snapshot: &NativeSnapshot) -> bool {
        if self.redirect {
            return false;
        }
        let registry = &snapshot.registry;
        if registry.overflow {
            return true;
        }
        if self.command.is_none() {
            registry.empty
        } else if self.index == 0 {
            registry.initial
        } else {
            registry.default
                || self.command.as_deref().is_some_and(|command| {
                    registry.names.contains(command)
                        || std::path::Path::new(command)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| registry.names.contains(name))
                })
        }
    }

    pub fn script_tokens<'a>(
        &self,
        query: &'a Query,
        snapshot: &NativeSnapshot,
    ) -> Vec<CompletionToken<'a>> {
        let delimiters: Vec<_> = snapshot.word_breaks.chars().collect();
        brush_core::completion::simple_tokenize_by_delimiters(
            &query.text[self.command_start..self.command_end],
            &delimiters,
        )
    }
}
