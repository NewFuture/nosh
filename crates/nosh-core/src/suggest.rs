//! Suggestion mode (Ctrl+G, `nosh -s`): a short separate conversation with
//! only `propose_command`; nothing is executed.

use nosh_llm::{ChatEngine, LlmError, Message, SamplingParams, SessionSpec};
use nosh_shell::{EmbeddedShell, Trigger};

use crate::prompt::{self, Environment, TaskInput};
use crate::tools::{self, ToolSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub command: String,
    pub explanation: Option<String>,
}

/// Asks for one command for `text`.
pub fn suggest(
    engine: &mut dyn ChatEngine,
    env: &Environment,
    shell: &EmbeddedShell,
    text: &str,
    trigger: Trigger,
    sampling: SamplingParams,
) -> Result<Option<Suggestion>, LlmError> {
    let spec = SessionSpec {
        system: prompt::suggest_system_prompt(env),
        tools: tools::specs(ToolSet::Suggest),
        thinking: false,
        sampling: SamplingParams {
            temperature: 0.7,
            ..sampling
        },
        max_new_tokens: 256,
    };
    let sid = engine.open(spec)?;
    let msg = prompt::task_message(shell, &TaskInput::new(trigger, text), None);
    engine.cancel_handle().reset();
    let res = engine.step(sid, vec![Message::User(msg)], &mut |_| {});
    engine.close(sid);
    let out = res?;
    let found = out
        .tool_calls
        .iter()
        .find(|c| c.name == "propose_command")
        .and_then(|c| {
            let cmd = c.str_arg("command")?.trim();
            (!cmd.is_empty()).then(|| Suggestion {
                command: cmd.to_string(),
                explanation: c.str_arg("explanation").map(str::to_string),
            })
        })
        .or_else(|| {
            extract_command(&out.text).map(|command| Suggestion {
                command,
                explanation: None,
            })
        });
    // Never hand the user a command whose text is not what it looks like.
    Ok(found.filter(|s| !s.command.chars().any(nosh_shell::style::is_hidden)))
}

/// Falls back to the first code block (all of it: a command may span lines)
/// or a `$ ` line of prose output.
pub fn extract_command(text: &str) -> Option<String> {
    let mut block: Option<Vec<&str>> = None;
    for line in text.lines() {
        if line.trim().starts_with("```") {
            match block.take() {
                Some(lines) => {
                    let cmd = lines.join("\n").trim().to_string();
                    if !cmd.is_empty() {
                        return Some(cmd);
                    }
                }
                None => block = Some(Vec::new()),
            }
            continue;
        }
        if let Some(lines) = &mut block {
            lines.push(line.trim_start().strip_prefix("$ ").unwrap_or(line));
        }
    }
    // A block cut off before its closing fence.
    if let Some(cmd) = block
        .map(|lines| lines.join("\n").trim().to_string())
        .filter(|c| !c.is_empty())
    {
        return Some(cmd);
    }
    text.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("$ "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction() {
        assert_eq!(
            extract_command("Use this:\n```bash\n$ du -sh * | sort -h\n```").as_deref(),
            Some("du -sh * | sort -h")
        );
        assert_eq!(
            extract_command("Run\n$ ls -la\nto list").as_deref(),
            Some("ls -la")
        );
        assert_eq!(extract_command("no idea"), None);
    }

    #[test]
    fn multi_line_blocks_are_kept_whole() {
        let text = "Rename them:\n```bash\nfor f in *.txt; do\n  mv \"$f\" \"${f%.txt}.md\"\ndone\n```\nThen check.";
        assert_eq!(
            extract_command(text).as_deref(),
            Some("for f in *.txt; do\n  mv \"$f\" \"${f%.txt}.md\"\ndone")
        );
        assert_eq!(
            extract_command("```\n$ cd src\n$ make\n```").as_deref(),
            Some("cd src\nmake")
        );
    }
}
