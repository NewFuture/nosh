use std::os::unix::process::ExitStatusExt;
use std::path::Path;

use serde_json::json;

use super::{Captured, HelpCommand};

const EXCERPT_CHARS: usize = 1800;

pub(super) fn render(
    command: &HelpCommand<'_>,
    program: &Path,
    executable: &Path,
    output: &Captured,
    query: Option<&str>,
) -> Result<String, String> {
    let mut blocks = blocks("stdout", &output.stdout);
    blocks.extend(self::blocks("stderr", &output.stderr));
    let exit_code = output.status.and_then(|status| status.code());
    let signal = output.status.and_then(|status| status.signal());
    if blocks.is_empty() {
        return Err(format!(
            "no help text returned by {} (exit={exit_code:?}, signal={signal:?}, capture_complete={})",
            json!(program),
            output.complete
        ));
    }
    let mut selected = Vec::new();
    let mut matching_section = false;
    let mut stream = "";
    for block in &blocks {
        if stream != block.stream {
            matching_section = false;
            stream = block.stream;
        }
        let found = query.and_then(|query| find_query(&block.text, query));
        if heading(first_line(&block.text)) && !option_line(&block.text) {
            matching_section =
                query.is_some_and(|query| find_query(first_line(&block.text), query).is_some());
        }
        if let Some(anchor) = found.or_else(|| (query.is_none() || matching_section).then_some(0)) {
            selected.push((block, anchor));
        }
    }
    selected.sort_by_key(|(block, anchor)| {
        if query.is_some_and(|query| query.starts_with('-')) {
            // Prefer a flag's declaration over a mention in examples or prose.
            usize::from(!option_line(&block.text) || *anchor >= first_line(&block.text).len())
        } else if query.is_none() {
            let line = first_line(&block.text).trim().to_lowercase();
            usize::from(!(line.starts_with("usage:") || line == "synopsis"))
        } else {
            0
        }
    });
    let matched_blocks = query.map(|_| selected.len());
    let mut body = String::new();
    let mut truncated = false;
    let mut previous_stream = "";
    for (block, anchor) in selected {
        let mut prefix = if body.is_empty() { "" } else { "\n\n" }.to_owned();
        if previous_stream != block.stream {
            prefix.push_str(&format!("[{}]\n", block.stream));
        }
        let remaining = EXCERPT_CHARS.saturating_sub(body.chars().count() + prefix.len());
        if remaining < 64 {
            truncated = true;
            break;
        }
        let (text, cut) = excerpt(&block.text, anchor, remaining);
        body.push_str(&prefix);
        body.push_str(&text);
        previous_stream = block.stream;
        truncated |= cut;
    }
    if body.is_empty() {
        body = "[no matching help text in captured output; this does not establish that an option is unsupported]".into();
    }
    let metadata = json!({
        "name": command.name,
        "program": program,
        "executable": executable,
        "argument": command.flag,
        "subcommands": command.subcommands,
        "exit_code": exit_code,
        "signal": signal,
        "capture_complete": output.complete,
        "stdout_bytes": output.stdout.len(),
        "stderr_bytes": output.stderr.len(),
        "query": query,
        "matched_blocks": matched_blocks,
        "excerpt_truncated": truncated,
    });
    Ok(format!("[command_help]\n{metadata}\n{body}"))
}

struct Block {
    stream: &'static str,
    text: String,
}

fn blocks(stream: &'static str, bytes: &[u8]) -> Vec<Block> {
    let text = nosh_shell::style::strip_ansi(&String::from_utf8_lossy(bytes));
    let lines: Vec<_> = text
        .lines()
        .map(nosh_shell::style::safe_output_line)
        .collect();
    let mut result = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        if lines[start].trim().is_empty() {
            start += 1;
            continue;
        }
        let option = option_line(&lines[start]);
        let mut end = start + 1;
        while end < lines.len() {
            let line = &lines[end];
            if line.trim().is_empty() {
                let next = (end + 1..lines.len()).find(|&i| !lines[i].trim().is_empty());
                if option
                    && let Some(next) = next
                    && indent(&lines[next]) > indent(&lines[start])
                    && !option_line(&lines[next])
                    && !heading(&lines[next])
                {
                    end = next;
                    continue;
                }
                break;
            }
            if option_line(line)
                || heading(line)
                || (option && indent(line) <= indent(&lines[start]))
            {
                break;
            }
            end += 1;
        }
        result.push(Block {
            stream,
            text: lines[start..end].join("\n"),
        });
        start = end;
    }
    result
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn option_line(text: &str) -> bool {
    first_line(text)
        .trim_start()
        .strip_prefix('-')
        .and_then(|text| text.chars().next())
        .is_some_and(|c| c == '-' || c.is_alphanumeric() || c == '?')
}

fn heading(line: &str) -> bool {
    let line = line.trim();
    line.ends_with(':') || matches!(line, "SYNOPSIS" | "OPTIONS" | "DESCRIPTION" | "COMMANDS")
}

fn indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 8 } else { 1 })
        .sum()
}

fn find_query(text: &str, query: &str) -> Option<usize> {
    let option = query.starts_with('-');
    let haystack = text.to_lowercase();
    let needle = query.to_lowercase();
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '+');
    let find = |needle: &str| {
        haystack
            .match_indices(needle)
            .find_map(|(offset, matched)| {
                (!option
                    || (!haystack[..offset].chars().next_back().is_some_and(word)
                        && !haystack[offset + matched.len()..]
                            .chars()
                            .next()
                            .is_some_and(word)))
                .then_some(offset)
            })
    };
    let offset = find(&needle).or_else(|| {
        // Git usage abbreviates both forms of boolean flags as --[no-]option.
        let long = needle.strip_prefix("--")?;
        if long.is_empty() || !long.chars().all(|c| c.is_alphanumeric() || c == '-') {
            return None;
        }
        find(&format!(
            "--[no-]{}",
            long.strip_prefix("no-").unwrap_or(long)
        ))
    })?;
    // Unicode lowercase can change byte length; return an offset into the original.
    let mut lowered = 0;
    text.char_indices().find_map(|(original, c)| {
        let start = lowered;
        lowered += c.to_lowercase().map(char::len_utf8).sum::<usize>();
        (start <= offset && offset < lowered).then_some(original)
    })
}

fn excerpt(text: &str, anchor: usize, limit: usize) -> (String, bool) {
    if text.chars().count() <= limit {
        return (text.into(), false);
    }
    let declaration = first_line(text);
    let prefix = if option_line(text) && text[..anchor].chars().count() > limit / 2 {
        let declaration: String = declaration.chars().take(limit / 3).collect();
        format!("{declaration}\n[...]\n")
    } else {
        String::new()
    };
    let budget = limit - prefix.chars().count();
    let anchor = text[..anchor].chars().count();
    let start = anchor.saturating_sub(budget / 3);
    let lead = if start > 0 { "[...]" } else { "" };
    let content: String = text
        .chars()
        .skip(start)
        .take(budget - lead.len() - 5)
        .collect();
    (format!("{prefix}{lead}{content}[...]"), true)
}
