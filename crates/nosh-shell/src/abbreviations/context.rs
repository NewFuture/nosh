use std::ops::Range;

use brush_core::completion::simple_tokenize_by_delimiters;

/// Conservative command positions, using brush's linear quote-aware splitter.
pub(crate) fn command_ranges(
    input: &str,
    prefix: Option<&str>,
) -> Result<Vec<Range<usize>>, String> {
    if input.len() > super::MAX_BYTES {
        return Err("abbreviation input exceeds 256 KiB".into());
    }
    if prefix.is_some_and(|prefix| !prefix.is_empty() && input.trim_start().starts_with(prefix)) {
        return Ok(Vec::new());
    }
    let tokens = simple_tokenize_by_delimiters(
        input,
        &[' ', '\t', '\n', '\r', '|', '&', ';', '<', '>', '(', ')'],
    );
    if tokens.len() > 4096 {
        return Err("abbreviation token limit exceeded".into());
    }
    let mut ranges = Vec::new();
    let mut command = true;
    let mut redirect = false;
    let mut comment = false;
    let mut previous_end = 0;
    let mut unsafe_scope = false;
    for (index, token) in tokens.iter().enumerate() {
        let gap = &input[previous_end..token.start];
        if gap.contains('\n') {
            if input[..token.start].trim_end().ends_with('\\') {
                return Ok(ranges);
            }
            command = true;
            comment = false;
            redirect = false;
        }
        previous_end = token.end();
        if comment {
            continue;
        }
        let word = token.text;
        if word.starts_with('#') {
            comment = true;
            continue;
        }
        if word.contains("$(")
            || word.contains('`')
            || word.contains("${")
            || word.contains('\\')
            || matches!(word, "(" | ")" | "((" | "))" | "{" | "}" | "[[" | "]]")
            || (command
                && !redirect
                && matches!(word, "for" | "select" | "case" | "function" | "coproc"))
        {
            return Ok(ranges);
        }
        match word {
            ";" | "&&" | "||" | "|" | "|&" | "&" => {
                command = true;
                redirect = false;
                continue;
            }
            "<" | ">" | ">>" | "<>" | ">|" | "&>" | "&>>" | "<&" | ">&" => {
                redirect = true;
                continue;
            }
            "<<" | "<<-" | "<<<" => return Ok(ranges),
            "if" | "then" | "elif" | "else" | "while" | "until" | "do" | "!" if command => continue,
            "fi" | "done" | "esac" if command => {
                command = false;
                continue;
            }
            _ => {}
        }
        if redirect {
            redirect = false;
            continue;
        }
        if word.bytes().all(|b| b.is_ascii_digit())
            && tokens
                .get(index + 1)
                .is_some_and(|next| next.start == token.end() && next.text.starts_with(['<', '>']))
        {
            continue;
        }
        if !command {
            continue;
        }
        if let Some((name, _)) = word.split_once('=')
            && brush_core::env::valid_variable_name(name)
        {
            unsafe_scope |= matches!(name, "PATH" | "HOME" | "OLDPWD");
            continue;
        }
        command = false;
        if unsafe_scope {
            continue;
        }
        if word.contains(['\'', '"', '$', '*', '?', '[', ']', '/'])
            || tokens
                .get(index + 1)
                .is_some_and(|next| next.text.starts_with('('))
        {
            unsafe_scope = true;
            continue;
        }
        ranges.push(token.start..token.end());
        unsafe_scope |= matches!(
            word,
            "." | "source"
                | "eval"
                | "alias"
                | "unalias"
                | "enable"
                | "hash"
                | "set"
                | "shopt"
                | "export"
                | "unset"
                | "declare"
                | "typeset"
                | "read"
                | "cd"
                | "pushd"
                | "popd"
                | "command"
                | "builtin"
        );
    }
    Ok(ranges)
}
