//! Suggestion mode (Ctrl+G, `nosh -s`): a short separate conversation with
//! no tools; one shell program is returned and nothing is executed.

use nosh_llm::{ChatEngine, LlmError, Message, SamplingParams, SessionSpec, StopReason};
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
        sampling,
        max_new_tokens: 256,
    };
    let sid = engine.open(spec)?;
    let msg = prompt::task_message(shell, &TaskInput::new(trigger, text), None);
    engine.cancel_handle().reset();
    let res = engine.step(sid, vec![Message::User(msg)], &mut |_| {});
    engine.close(sid);
    let out = res?;
    if out.stop != StopReason::EndOfTurn || !out.tool_calls.is_empty() || !out.errors.is_empty() {
        return Ok(None);
    }
    Ok(extract_command(&out.text, shell).map(|command| Suggestion {
        command,
        explanation: None,
    }))
}

/// Accept only a complete program, optionally inside one whole-response fence.
pub fn extract_command(text: &str, shell: &EmbeddedShell) -> Option<String> {
    let text = text.trim();
    let body = if text.starts_with("```") {
        let (tag, rest) = text.split_once('\n')?;
        if !matches!(tag.trim(), "```" | "```sh" | "```bash") {
            return None;
        }
        rest.strip_suffix("```")?.trim()
    } else {
        text
    };
    if body.contains("```") || body.chars().any(nosh_shell::style::is_hidden) {
        return None;
    }
    let body = body.strip_prefix("$ ").unwrap_or(body);
    nosh_shell::trigger::is_suggestion_program(body, shell).then(|| body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction() {
        let shell = EmbeddedShell::new(Default::default()).unwrap();
        for text in ["echo ok", "```sh\necho ok\n```", "```bash\n$ echo ok\n```"] {
            assert_eq!(extract_command(text, &shell).as_deref(), Some("echo ok"));
        }
        for text in [
            "",
            "no idea",
            "Use this:\n```bash\necho ok\n```",
            "```bash\necho ok\n```\nThen check.",
            "echo ok\nThis prints ok.",
            "```sh\necho a\n```\n```sh\necho b\n```",
            "echo a\n\necho b",
            "echo '",
            "for f in *; do echo \"$f\"",
            "```sh\necho ok",
            "```python\nprint('ok')\n```",
            "echo \u{202e}bad",
        ] {
            assert_eq!(extract_command(text, &shell), None, "{text}");
        }
    }

    #[test]
    fn multi_line_blocks_are_kept_whole() {
        let shell = EmbeddedShell::new(Default::default()).unwrap();
        for program in [
            "for f in *.txt; do\n  echo \"$f\"\ndone",
            "if test -d src; then\n  echo yes\nelse\n  echo no\nfi",
            "cd src && echo ok",
        ] {
            assert_eq!(extract_command(program, &shell).as_deref(), Some(program));
            assert_eq!(
                extract_command(&format!("```bash\n{program}\n```"), &shell).as_deref(),
                Some(program)
            );
        }
    }
}
