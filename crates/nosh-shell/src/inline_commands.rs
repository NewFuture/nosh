//! The shared input contract, command catalog and local completion scope.

use std::ops::Range;

use nosh_platform::tr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    Confirm,
    Auto,
    Yolo,
}

const MODES: &[(&str, ApprovalMode)] = &[
    ("confirm", ApprovalMode::Confirm),
    ("auto", ApprovalMode::Auto),
    ("yolo", ApprovalMode::Yolo),
];
const SWITCHES: &[(&str, bool)] = &[("on", true), ("off", false)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagementCommand {
    Mode(Option<ApprovalMode>),
    Think(Option<bool>),
    Out(Option<usize>),
    Clear,
    Ctx,
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Auto(Option<bool>),
    Fix(String),
    Manage(ManagementCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input<'a> {
    Shell,
    Task(&'a str),
    Command(Command),
    Error(Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub span: Range<usize>,
    kind: ErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ErrorKind {
    Unknown(String),
    Arguments(&'static str),
}

impl Error {
    pub fn message(&self, prefix: &str) -> String {
        let prefix = crate::style::visible_text(prefix);
        match &self.kind {
            ErrorKind::Unknown(name) => {
                let name = crate::style::visible_text(name);
                tr!(
                    format!(
                        "未知内置命令 {prefix}{name}；使用 {prefix}help 查看帮助，任务正文前须加空白"
                    ),
                    format!(
                        "unknown command {prefix}{name}; use {prefix}help, or add whitespace before a task"
                    )
                )
            }
            ErrorKind::Arguments(syntax) => tr!(
                format!("参数无效；用法：{prefix}{syntax}"),
                format!("invalid arguments; usage: {prefix}{syntax}")
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Help,
    Mode,
    Think,
    Auto,
    Fix,
    Out,
    Clear,
    Ctx,
    Status,
}

struct Definition {
    name: &'static str,
    syntax: &'static str,
    zh: &'static str,
    en: &'static str,
    kind: Kind,
}

const COMMANDS: &[Definition] = &[
    Definition {
        name: "help",
        syntax: "help",
        zh: "显示帮助",
        en: "show help",
        kind: Kind::Help,
    },
    Definition {
        name: "mode",
        syntax: "mode [confirm|auto|yolo]",
        zh: "查看或切换审批模式",
        en: "show or switch approval mode",
        kind: Kind::Mode,
    },
    Definition {
        name: "think",
        syntax: "think [on|off]",
        zh: "查看或开关思考模式",
        en: "show or toggle thinking",
        kind: Kind::Think,
    },
    Definition {
        name: "auto",
        syntax: "auto [on|off]",
        zh: "查看或暂停自动路由与自动建议",
        en: "show or pause automatic routing and suggestions",
        kind: Kind::Auto,
    },
    Definition {
        name: "fix",
        syntax: "fix [question]",
        zh: "生成修复命令；附问题时交给 Agent 诊断",
        en: "suggest a fix; add a question for Agent diagnosis",
        kind: Kind::Fix,
    },
    Definition {
        name: "out",
        syntax: "out [id]",
        zh: "查看 agent 命令输出，默认最近一条",
        en: "show agent command output, most recent by default",
        kind: Kind::Out,
    },
    Definition {
        name: "clear",
        syntax: "clear",
        zh: "新建对话，不重置 shell 环境",
        en: "start a new conversation without resetting the shell",
        kind: Kind::Clear,
    },
    Definition {
        name: "ctx",
        syntax: "ctx",
        zh: "查看上下文占用",
        en: "show context usage",
        kind: Kind::Ctx,
    },
    Definition {
        name: "status",
        syntax: "status",
        zh: "查看运行状态",
        en: "show status",
        kind: Kind::Status,
    },
];

impl Definition {
    fn description(&self) -> &'static str {
        tr!(self.zh, self.en)
    }

    fn values(&self) -> Vec<(&'static str, Option<&'static str>)> {
        match self.kind {
            Kind::Mode => MODES.iter().map(|(name, _)| (*name, None)).collect(),
            Kind::Think | Kind::Auto => SWITCHES.iter().map(|(name, _)| (*name, None)).collect(),
            _ => Vec::new(),
        }
    }

    fn parse(&self, args: &str) -> Option<Command> {
        if matches!(self.kind, Kind::Fix) {
            return Some(Command::Fix(args.to_owned()));
        }
        let mut words = args.split_whitespace();
        let arg = words.next();
        if words.next().is_some() {
            return None;
        }
        let switch = || match arg {
            Some(arg) => SWITCHES
                .iter()
                .find(|(name, _)| *name == arg)
                .map(|(_, on)| Some(*on)),
            None => Some(None),
        };
        match self.kind {
            Kind::Mode => {
                let mode = match arg {
                    Some(arg) => Some(MODES.iter().find(|(name, _)| *name == arg)?.1),
                    None => None,
                };
                Some(Command::Manage(ManagementCommand::Mode(mode)))
            }
            Kind::Think => Some(Command::Manage(ManagementCommand::Think(switch()?))),
            Kind::Auto => Some(Command::Auto(switch()?)),
            Kind::Out => {
                let id = match arg {
                    Some(arg) if arg.bytes().all(|byte| byte.is_ascii_digit()) => {
                        Some(arg.parse().ok()?)
                    }
                    Some(_) => return None,
                    None => None,
                };
                Some(Command::Manage(ManagementCommand::Out(id)))
            }
            _ if arg.is_some() => None,
            Kind::Help => Some(Command::Help),
            Kind::Clear => Some(Command::Manage(ManagementCommand::Clear)),
            Kind::Ctx => Some(Command::Manage(ManagementCommand::Ctx)),
            Kind::Status => Some(Command::Manage(ManagementCommand::Status)),
            Kind::Fix => unreachable!(),
        }
    }
}

fn body<'a>(line: &'a str, prefix: &str, enabled: bool) -> Option<(&'a str, usize)> {
    if !enabled || prefix.is_empty() {
        return None;
    }
    let trimmed = line.trim_start();
    let body = trimmed.strip_prefix(prefix)?;
    Some((body, line.len() - trimmed.len() + prefix.len()))
}

fn task(body: &str) -> bool {
    !body.trim().is_empty() && body.starts_with(char::is_whitespace)
}

pub fn is_command(line: &str, prefix: &str, enabled: bool) -> bool {
    body(line, prefix, enabled).is_some_and(|(body, _)| !task(body))
}

pub fn parse<'a>(line: &'a str, prefix: &str, enabled: bool) -> Input<'a> {
    let Some((body, start)) = body(line, prefix, enabled) else {
        return Input::Shell;
    };
    if body.trim().is_empty() {
        return Input::Command(Command::Help);
    }
    if task(body) {
        return Input::Task(body.trim());
    }
    let end = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = &body[..end];
    let Some(definition) = COMMANDS.iter().find(|definition| definition.name == name) else {
        return Input::Error(Error {
            span: start..start + end,
            kind: ErrorKind::Unknown(name.into()),
        });
    };
    let rest = &body[end..];
    let args = rest.trim();
    match definition.parse(args) {
        Some(command) => Input::Command(command),
        None => {
            let start = start + end + rest.len() - rest.trim_start().len();
            Input::Error(Error {
                span: start..start + args.len(),
                kind: ErrorKind::Arguments(definition.syntax),
            })
        }
    }
}

pub fn help(prefix: &str) -> String {
    let prefix = crate::style::visible_text(prefix);
    let mut lines = COMMANDS
        .iter()
        .map(|definition| {
            format!(
                "  {prefix}{:<26} {}",
                definition.syntax,
                definition.description()
            )
        })
        .collect::<Vec<_>>();
    lines.push(format!(
        "  {prefix} {:<25} {}",
        "<task>",
        tr!(
            "执行任务（前缀后加空白）",
            "run a task (whitespace after the prefix)"
        )
    ));
    lines.join("\n")
}

pub(crate) struct CompletionScope<'a> {
    pub word: &'a str,
    pub span: Range<usize>,
    pub values: Vec<(&'static str, Option<&'static str>)>,
    pub command_name: bool,
}

pub(crate) fn completion_scope<'a>(
    line: &'a str,
    cursor: usize,
    prefix: &str,
) -> Option<CompletionScope<'a>> {
    let (body, start) = body(line, prefix, true)?;
    if task(body) {
        return None;
    }
    let mut scope = CompletionScope {
        word: "",
        span: cursor..cursor,
        values: Vec::new(),
        command_name: false,
    };
    if cursor < start {
        return Some(scope);
    }
    let end = body.find(char::is_whitespace).unwrap_or(body.len());
    if body.trim().is_empty() || cursor <= start + end {
        scope.word = if body.trim().is_empty() {
            ""
        } else {
            line.get(start..cursor)?
        };
        scope.span = start..if body.trim().is_empty() {
            line.len()
        } else {
            start + end
        };
        scope.values = COMMANDS
            .iter()
            .map(|definition| (definition.name, Some(definition.description())))
            .collect();
        scope.command_name = true;
        return Some(scope);
    }
    let rest = &body[end..];
    let args = rest.trim_start();
    let arg_start = start + end + rest.len() - args.len();
    let arg_end = arg_start + args.find(char::is_whitespace).unwrap_or(args.len());
    if cursor > arg_end {
        return Some(scope);
    }
    let Some(definition) = COMMANDS
        .iter()
        .find(|definition| definition.name == &body[..end])
    else {
        return Some(scope);
    };
    let start = arg_start.min(cursor);
    scope.word = line.get(start..cursor)?;
    scope.span = start..arg_end;
    scope.values = definition.values();
    Some(scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_contract_and_task_text() {
        for line in ["#", " # \t", "#help"] {
            assert_eq!(parse(line, "#", true), Input::Command(Command::Help));
        }
        assert_eq!(
            parse("# 找文件\n不要修改", "#", true),
            Input::Task("找文件\n不要修改")
        );
        assert_eq!(
            parse("  ## mode auto ", "##", true),
            Input::Task("mode auto")
        );
        assert_eq!(
            parse("  问mode auto", "问", true),
            Input::Command(Command::Manage(ManagementCommand::Mode(Some(
                ApprovalMode::Auto
            ))))
        );
        for (line, prefix, enabled) in [
            ("#help", "#", false),
            ("#help", "", true),
            ("echo hello #mode auto", "#", true),
            ("ai help", "#", true),
        ] {
            assert_eq!(parse(line, prefix, enabled), Input::Shell);
        }
        assert!(matches!(parse("#找文件", "#", true), Input::Error(_)));
    }

    #[test]
    fn catalog_and_fixed_parameters_share_the_contract() {
        assert_eq!(COMMANDS.len(), 9);
        for definition in COMMANDS {
            assert!(matches!(
                parse(&format!("#{}", definition.name), "#", true),
                Input::Command(_)
            ));
            for (value, _) in definition.values() {
                assert!(matches!(
                    parse(&format!("#{} {value}", definition.name), "#", true),
                    Input::Command(_)
                ));
            }
        }
        assert_eq!(
            parse("#auto off", "#", true),
            Input::Command(Command::Auto(Some(false)))
        );
        assert_eq!(
            parse("#out 12", "#", true),
            Input::Command(Command::Manage(ManagementCommand::Out(Some(12))))
        );
        assert_eq!(
            parse("#fix 说明原因\n不要修改文件", "#", true),
            Input::Command(Command::Fix("说明原因\n不要修改文件".into()))
        );
    }

    #[test]
    fn invalid_commands_and_arguments_never_become_tasks() {
        for line in [
            "#modeXYZ",
            "#unknown",
            "#next",
            "#history",
            "#private",
            "#undo",
            "#model",
            "#mode bad",
            "#think maybe",
            "#auto on extra",
            "#help extra",
            "#clear extra",
            "#ctx extra",
            "#status extra",
            "#out nope",
            "#out -1",
            "#out #1",
            "#out 999999999999999999999999999999999",
        ] {
            let Input::Error(error) = parse(line, "#", true) else {
                panic!("accepted invalid command: {line}");
            };
            assert!(line.get(error.span.clone()).is_some());
            assert!(!error.message("#").is_empty());
        }
    }

    #[test]
    fn completion_scopes_preserve_prefixes_and_arguments() {
        for line in ["#he", "  问he", "# \t"] {
            let prefix = if line.contains('问') { "问" } else { "#" };
            let scope = completion_scope(line, line.len(), prefix).unwrap();
            let mut result = line.to_owned();
            result.replace_range(scope.span, "help");
            assert_eq!(result, if prefix == "#" { "#help" } else { "  问help" });
            assert_eq!(scope.values.len(), 9);
        }
        let line = "#mode au";
        let scope = completion_scope(line, line.len(), "#").unwrap();
        assert_eq!(scope.word, "au");
        assert_eq!(&line[scope.span], "au");
        assert_eq!(
            scope
                .values
                .iter()
                .map(|(value, _)| *value)
                .collect::<Vec<_>>(),
            ["confirm", "auto", "yolo"]
        );
        assert!(completion_scope("# 任务", "# 任务".len(), "#").is_none());
        assert!(
            completion_scope("#fix 原因", "#fix 原因".len(), "#")
                .unwrap()
                .values
                .is_empty()
        );
        assert!(
            completion_scope("#unknown ", 9, "#")
                .unwrap()
                .values
                .is_empty()
        );
        assert!(!is_command("# 任务", "#", true));
        assert!(is_command("#unknown", "#", true));
        assert!(!is_command("#help", "#", false));
    }
}
