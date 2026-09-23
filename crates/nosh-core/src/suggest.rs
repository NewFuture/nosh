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
    if let Some(c) = out
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
    {
        return Ok(Some(c));
    }
    Ok(extract_command(&out.text).map(|command| Suggestion {
        command,
        explanation: None,
    }))
}

/// Falls back to a command in a code block or a `$ ` line of prose output.
pub fn extract_command(text: &str) -> Option<String> {
    let mut in_block = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            if in_block {
                in_block = false;
                continue;
            }
            in_block = true;
            continue;
        }
        if in_block && !t.is_empty() {
            return Some(t.trim_start_matches("$ ").to_string());
        }
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
}
