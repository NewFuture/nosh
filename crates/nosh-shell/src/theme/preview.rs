//! Deterministic display fixtures using the production status and syntax renderers.
//! These are sample facts, not a second input analyzer or a model request.

use super::{Role, Theme};
use crate::{Assistance, editing, input_assist, status, style};
use nosh_platform::tr;
use reedline::{Menu, PromptContext, PromptEditMode, PromptInteraction};

#[derive(Debug)]
pub struct Sample {
    pub name: &'static str,
    pub status: String,
    pub input: String,
    pub detail: String,
}

struct MenuSamples;

impl reedline::Completer for MenuSamples {
    fn complete(&mut self, _: &str, _: usize) -> reedline::CompletionResult {
        reedline::CompletionResult::fresh(
            [
                "status",
                "show",
                "switch",
                "stash",
                "stage",
                "shortlog",
                "submodule",
                "sparse-checkout",
                "send-email",
            ]
            .into_iter()
            .map(|value| reedline::Suggestion {
                value: value.into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
        )
    }
}

pub fn samples(
    theme: Theme,
    terminal: style::Terminal,
    columns: u16,
    approval: &str,
    ai: bool,
) -> Vec<Sample> {
    let (keys, _) = editing::Config {
        mode: editing::Mode::Emacs,
        ..Default::default()
    }
    .compile(editing::Capabilities { enhanced: false }, ai);
    let styles = theme.styles(terminal);
    let advice = Assistance::Command {
        command_id: 1,
        intent: tr!("建议", "Next").into(),
        program: "git status --short".into(),
    };
    let reversed = |role| {
        let pair = theme.text_color(role);
        super::ColorPair::from_rgb(pair.background_rgb(), pair.foreground_rgb())
            .expect("reversing a validated pair preserves contrast")
    };
    let fields = super::Fragment::parse(&status::paint_segments(
        &[
            (tr!("~/项目/nosh", "~/work/nosh").into(), theme.environment),
            ("main +1".into(), theme.git_branch),
            ("rs v1.98".into(), reversed(Role::Positive)),
            ("venv dev".into(), reversed(Role::Accent)),
            ("2.4s".into(), theme.status.pending),
        ],
        styles,
    ));
    let mut result = Vec::new();
    for (name, buffer, state, explanation, interaction) in [
        ("idle", "", None, "", PromptInteraction::Editing),
        (
            "input",
            "git status --short",
            None,
            "",
            PromptInteraction::Editing,
        ),
        ("fields", "cargo test", None, "", PromptInteraction::Editing),
        (
            "pending",
            "git status --short",
            Some(input_assist::State::Pending),
            tr!("查询中", "Querying"),
            PromptInteraction::Editing,
        ),
        ("advice", "", None, "", PromptInteraction::Editing),
        (
            "error",
            "missing_command",
            Some(input_assist::State::Error),
            tr!("未找到命令", "Command not found"),
            PromptInteraction::Editing,
        ),
        (
            "unavailable",
            "git status --short",
            Some(input_assist::State::Unavailable),
            tr!("分析暂不可用", "Analysis unavailable"),
            PromptInteraction::Editing,
        ),
        (
            "completion",
            "git s",
            None,
            "",
            PromptInteraction::Menu {
                name: "completion_menu",
                count: 9,
                provisional: false,
            },
        ),
        (
            "history",
            "git",
            None,
            "",
            PromptInteraction::HistorySearch {
                term: "git",
                has_match: true,
                status: Some(reedline::HistorySearchStatus {
                    title: tr!("历史 [全部]", "History [all]"),
                    description: Some(tr!("输入目录: 项目根目录", "Input directory: project root")),
                    ..Default::default()
                }),
            },
        ),
    ] {
        if name == "advice" && !ai {
            continue;
        }
        let context = PromptContext {
            buffer,
            cursor: buffer.len(),
            completion_cursor: buffer.len(),
            selection: None,
            columns,
            rows: 24,
            edit_mode: PromptEditMode::Emacs,
            interaction,
        };
        let feedback = state.map(|state| input_assist::Feedback {
            text: explanation.into(),
            compact: match state {
                input_assist::State::Error => tr!("无此命令", "No cmd"),
                input_assist::State::Pending => tr!("查询中", "Pending"),
                input_assist::State::Unavailable => tr!("不可用", "Unavail."),
                _ => explanation,
            }
            .into(),
            state,
            correction: None,
        });
        let row = status::compose(status::Context {
            editor: &context,
            environment: Some((tr!("~/项目/nosh", "~/work/nosh"), Some("main"))),
            external_environment: (name == "fields").then_some(&fields),
            feedback: feedback.as_ref(),
            completion: None,
            assistance: (name == "advice").then_some(&advice),
            latest_command: Some(1),
            failed_exit: None,
            last_result: None,
            context_usage: (name == "fields" && ai).then_some(crate::repl::ContextUsage {
                used: 1536,
                max: 8192,
            }),
            approval: if ai { approval } else { "" },
            note: None,
            bindings: keys.hints(&PromptEditMode::Emacs, interaction),
            color: terminal.color,
            color_depth: crate::status::color_depth(),
            unicode: terminal.unicode,
            theme,
        });
        let indicator = styles.paint(
            Role::Positive,
            if matches!(
                interaction,
                PromptInteraction::HistorySearch { status: None, .. }
            ) {
                crate::repl::INLINE_HISTORY_INDICATOR
            } else {
                terminal.glyph("❯ ", "> ")
            },
        );
        let mut input = indicator;
        for (index, word) in buffer.split_inclusive(' ').enumerate() {
            let role = if name == "history" {
                input_assist::Role::Text
            } else if name == "error" {
                input_assist::Role::Error
            } else if index == 0 {
                input_assist::Role::External
            } else {
                input_assist::Role::Text
            };
            input.push_str(
                &input_assist::role_style(role, styles)
                    .paint(word)
                    .to_string(),
            );
        }
        if name == "history" {
            input.push_str(
                &super::editor::hint_style(styles)
                    .paint(" status")
                    .to_string(),
            );
        }
        let detail = if name == "completion" {
            let mut menu =
                super::completion_menu("completion_menu", super::FrameTheme::new(styles));
            menu.update_values(&mut reedline::Editor::default(), &mut MenuSamples);
            menu.layout_for_preview(columns.saturating_sub(1), 3);
            menu.menu_string(3, styles.style(Role::Text).foreground.is_some())
                .replace("\r\n", "\n")
        } else {
            String::new()
        };
        result.push(Sample {
            name,
            status: row.text,
            input,
            detail,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_use_available_space_for_complete_state_labels() {
        let terminal = style::Terminal {
            tty: true,
            ansi: true,
            color: false,
            unicode: true,
        };
        for sample in samples(Theme::default(), terminal, 80, "Auto", true) {
            if sample.name == "history" {
                assert!(sample.input.starts_with(terminal.glyph("❯ ", "> ")));
                assert!(sample.input.ends_with("git status"));
                assert!(sample.status.contains(tr!("历史 [全部]", "History [all]")));
            }
            let expected = match sample.name {
                "advice" => tr!("建议: git status --short", "Next: git status --short"),
                "error" => tr!("未找到命令", "Command not found"),
                "unavailable" => tr!("分析暂不可用", "Analysis unavailable"),
                _ => continue,
            };
            assert!(sample.status.contains(expected), "{:?}", sample.status);
        }
    }

    #[test]
    fn samples_share_safe_geometry_across_themes_modes_and_widths() {
        for columns in [32, 48, 64, 80, 120, 160] {
            for ai in [true, false] {
                for approval in ["Auto", "Confirm", "YOLO"] {
                    let terminal = style::Terminal {
                        tty: true,
                        ansi: true,
                        color: false,
                        unicode: true,
                    };
                    let mut baseline = None;
                    for builtin in super::super::Builtin::ALL {
                        let samples = samples(builtin.palette(), terminal, columns, approval, ai);
                        for sample in &samples {
                            assert_eq!(style::width(&sample.status), usize::from(columns - 1));
                            assert!(!sample.status.contains('\x1b'));
                        }
                        let text: Vec<_> = samples
                            .iter()
                            .map(|sample| (sample.status.clone(), sample.input.clone()))
                            .collect();
                        if let Some(baseline) = &baseline {
                            assert_eq!(&text, baseline);
                        }
                        baseline = Some(text);
                        assert_eq!(samples.len(), if ai { 9 } else { 8 });
                    }
                }
            }
        }
    }
}
