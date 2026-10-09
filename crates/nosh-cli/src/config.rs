//! `~/.config/nosh/config.toml` (design §11): common keys only; unknown keys
//! and bad values produce warnings and fall back to defaults.

use std::path::PathBuf;

use nosh_hub::SourceSelection;
use nosh_permissions::{ApprovalMode, RuleSpec, UserRule};
use nosh_shell::{CaptureOutput, OnFailure};

#[derive(Debug, Clone)]
pub struct Config {
    pub ai_prefix: String,
    pub trigger_on_error: bool,
    pub on_failure: OnFailure,
    pub capture_output: CaptureOutput,
    pub input_assist: bool,
    pub completion: bool,
    pub completion_scripts: bool,
    pub status_bar: bool,
    pub command_assist: bool,
    pub editing: nosh_shell::editing::Config,
    pub nl_guard: bool,
    pub approval: ApprovalMode,
    pub max_steps: usize,
    pub command_timeout_sec: u64,
    pub restore_cwd: bool,
    pub conversation_idle_minutes: u64,
    pub model_id: Option<String>,
    pub model_path: Option<PathBuf>,
    pub context_length: usize,
    pub model_device: Result<nosh_llm::InferenceDevice, String>,
    pub thinking: bool,
    pub download_auto: bool,
    pub source_selection: SourceSelection,
    pub allow: Vec<UserRule>,
    pub deny: Vec<UserRule>,
    pub safety_error: Option<String>,
    pub protected_paths: Vec<PathBuf>,
    pub fallback_shell: String,
    pub warnings: Vec<String>,
    pub path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ai_prefix: "#".into(),
            trigger_on_error: true,
            on_failure: OnFailure::Hint,
            capture_output: CaptureOutput::Last,
            input_assist: true,
            completion: true,
            completion_scripts: true,
            status_bar: true,
            command_assist: true,
            editing: Default::default(),
            nl_guard: true,
            approval: ApprovalMode::default(),
            max_steps: 10,
            command_timeout_sec: 60,
            restore_cwd: false,
            conversation_idle_minutes: 30,
            model_id: None,
            model_path: None,
            context_length: 8192,
            model_device: Ok(nosh_llm::InferenceDevice::Auto),
            thinking: false,
            download_auto: true,
            source_selection: SourceSelection::Auto,
            allow: Vec::new(),
            deny: Vec::new(),
            safety_error: None,
            protected_paths: Vec::new(),
            fallback_shell: "/bin/bash".into(),
            warnings: Vec::new(),
            path: None,
        }
    }
}

const KNOWN: &[(&str, &[&str])] = &[
    (
        "shell",
        &[
            "ai_prefix",
            "trigger_on_error",
            "on_failure",
            "capture_output",
            "input_assist",
            "completion",
            "completion_scripts",
            "status_bar",
            "command_assist",
            "nl_guard",
            "edit_mode",
            "keybindings",
        ],
    ),
    (
        "agent",
        &[
            "approval",
            "max_steps",
            "command_timeout_sec",
            "restore_cwd",
            "conversation_idle_minutes",
        ],
    ),
    (
        "model",
        &["id", "path", "context_length", "device", "thinking"],
    ),
    ("download", &["auto", "source_selection"]),
    (
        "safety",
        &["allow", "deny", "protected_paths", "fallback_shell"],
    ),
];

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(h) = std::env::var_os("HOME")
    {
        return PathBuf::from(h).join(rest);
    }
    if s == "~"
        && let Some(h) = std::env::var_os("HOME")
    {
        return PathBuf::from(h);
    }
    PathBuf::from(s)
}

struct Reader<'a> {
    table: &'a toml::Table,
    warnings: Vec<String>,
}

impl Reader<'_> {
    fn get(&self, sec: &str, key: &str) -> Option<&toml::Value> {
        self.table.get(sec)?.as_table()?.get(key)
    }

    fn str(&mut self, sec: &str, key: &str) -> Option<String> {
        let v = self.get(sec, key)?;
        match v.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                self.warnings
                    .push(format!("{sec}.{key}: expected a string"));
                None
            }
        }
    }

    fn bool(&mut self, sec: &str, key: &str) -> Option<bool> {
        let v = self.get(sec, key)?;
        match v.as_bool() {
            Some(b) => Some(b),
            None => {
                self.warnings
                    .push(format!("{sec}.{key}: expected true or false"));
                None
            }
        }
    }

    fn int(&mut self, sec: &str, key: &str, min: i64, max: i64) -> Option<i64> {
        let v = self.get(sec, key)?;
        match v.as_integer() {
            Some(i) if (min..=max).contains(&i) => Some(i),
            _ => {
                self.warnings
                    .push(format!("{sec}.{key}: expected an integer in {min}..={max}"));
                None
            }
        }
    }

    fn list(&mut self, sec: &str, key: &str) -> Option<Vec<String>> {
        let v = self.get(sec, key)?;
        match v.as_array() {
            Some(a) if a.iter().all(|x| x.is_str()) => Some(
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
            ),
            _ => {
                self.warnings
                    .push(format!("{sec}.{key}: expected a list of strings"));
                None
            }
        }
    }
}

impl Config {
    /// Loads the user config (missing file = defaults).
    pub fn load() -> Self {
        Self::load_from(nosh_platform::paths::config_file())
    }

    /// A missing file means defaults; a file that cannot be read also gets
    /// defaults, but with a warning, since its rules would be lost silently.
    fn load_from(path: PathBuf) -> Self {
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => Self {
                warnings: vec![format!("cannot be read, using defaults: {e}")],
                safety_error: Some(format!("configuration cannot be read: {e}")),
                ..Self::default()
            },
        };
        cfg.path = Some(path);
        cfg
    }

    pub fn parse(text: &str) -> Self {
        let mut c = Self::default();
        let table: toml::Table = match text.parse() {
            Ok(t) => t,
            Err(e) => {
                c.safety_error = Some(format!("invalid configuration: {e}"));
                c.warnings
                    .push(format!("invalid TOML, using defaults: {e}"));
                return c;
            }
        };
        let mut r = Reader {
            table: &table,
            warnings: Vec::new(),
        };
        for (sec, v) in &table {
            let Some(known) = KNOWN.iter().find(|(s, _)| s == sec).map(|(_, k)| *k) else {
                r.warnings.push(format!("unknown section [{sec}]"));
                continue;
            };
            match v.as_table() {
                Some(t) => {
                    for k in t.keys() {
                        if !known.contains(&k.as_str()) {
                            r.warnings.push(format!("unknown key {sec}.{k}"));
                        }
                    }
                }
                None => r.warnings.push(format!("{sec} must be a table")),
            }
        }
        if let Some(v) = r.str("shell", "ai_prefix") {
            if v.starts_with(char::is_whitespace) {
                r.warnings.push(
                    "shell.ai_prefix: must not start with whitespace; using the default prefix"
                        .into(),
                );
            } else {
                c.ai_prefix = v;
            }
        }
        if let Some(v) = r.bool("shell", "trigger_on_error") {
            c.trigger_on_error = v;
        }
        if let Some(v) = r.bool("shell", "input_assist") {
            c.input_assist = v;
        }
        if let Some(value) = r.bool("shell", "completion") {
            c.completion = value;
        }
        if let Some(value) = r.bool("shell", "completion_scripts") {
            c.completion_scripts = value;
        }
        if let Some(v) = r.bool("shell", "status_bar") {
            c.status_bar = v;
        }
        if let Some(v) = r.bool("shell", "command_assist") {
            c.command_assist = v;
        }
        if let Some(v) = r.str("shell", "on_failure") {
            match OnFailure::parse(&v) {
                Some(o) => c.on_failure = o,
                None => r
                    .warnings
                    .push("shell.on_failure: hint | auto | off".into()),
            }
        }
        if let Some(v) = r.str("shell", "capture_output") {
            match CaptureOutput::parse(&v) {
                Some(mode) => c.capture_output = mode,
                None => r.warnings.push("shell.capture_output: off | last".into()),
            }
        }
        if let Some(v) = r.str("shell", "nl_guard") {
            match v.as_str() {
                "destructive" => c.nl_guard = true,
                "off" => c.nl_guard = false,
                _ => r.warnings.push("shell.nl_guard: destructive | off".into()),
            }
        }
        let mut editing_errors = Vec::new();
        if let Some(value) = r.get("shell", "edit_mode") {
            match value
                .as_str()
                .ok_or_else(|| "shell.edit_mode: expected a string".to_owned())
                .and_then(str::parse)
            {
                Ok(mode) => c.editing.mode = mode,
                Err(error) => editing_errors.push(error),
            }
        }
        if let Some(value) = r.get("shell", "keybindings") {
            match value.clone().try_into::<nosh_shell::editing::Bindings>() {
                Ok(bindings) => c.editing.keybindings = bindings,
                Err(error) => editing_errors.push(format!("shell.keybindings: {error}")),
            }
        }
        if let Err(errors) = c.editing.validate() {
            editing_errors.extend(errors);
        }
        if !editing_errors.is_empty() {
            r.warnings.extend(editing_errors);
            r.warnings
                .push("shell editing configuration rejected; using defaults".into());
            c.editing = Default::default();
        }
        if let Some(v) = r.str("agent", "approval") {
            match ApprovalMode::parse(&v) {
                Some(m) => c.approval = m,
                None => r
                    .warnings
                    .push("agent.approval: confirm | auto | yolo".into()),
            }
        }
        if let Some(v) = r.int("agent", "max_steps", 1, 50) {
            c.max_steps = v as usize;
        }
        if let Some(v) = r.int("agent", "command_timeout_sec", 1, 600) {
            c.command_timeout_sec = v as u64;
        }
        if let Some(v) = r.bool("agent", "restore_cwd") {
            c.restore_cwd = v;
        }
        if let Some(v) = r.int("agent", "conversation_idle_minutes", 1, 24 * 60) {
            c.conversation_idle_minutes = v as u64;
        }
        c.model_id = r.str("model", "id");
        c.model_path = r.str("model", "path").map(|p| expand_home(&p));
        if let Some(v) = r.int("model", "context_length", 1024, 32768) {
            c.context_length = v as usize;
        }
        if let Some(v) = r.get("model", "device") {
            c.model_device = v
                .as_str()
                .ok_or_else(|| "model.device: expected a string".to_string())
                .and_then(|s| s.parse().map_err(|e| format!("model.device: {e}")));
            if let Err(error) = &c.model_device {
                r.warnings.push(error.clone());
            }
        }
        if let Some(v) = r.str("model", "thinking") {
            match v.as_str() {
                "on" => c.thinking = true,
                "off" => c.thinking = false,
                _ => r.warnings.push("model.thinking: off | on".into()),
            }
        }
        if let Some(v) = r.str("download", "auto") {
            match v.as_str() {
                "yes" => c.download_auto = true,
                "never" => c.download_auto = false,
                _ => r.warnings.push("download.auto: yes | never".into()),
            }
        }
        if let Some(v) = r.str("download", "source_selection") {
            match SourceSelection::parse(&v) {
                Some(s) => c.source_selection = s,
                None => r
                    .warnings
                    .push("download.source_selection: auto | hf | hf-mirror | modelscope".into()),
            }
        }
        for (key, target) in [("allow", &mut c.allow), ("deny", &mut c.deny)] {
            if let Some(value) = r.get("safety", key) {
                let compiled = value
                    .clone()
                    .try_into::<Vec<RuleSpec>>()
                    .map_err(|e| format!("safety.{key}: expected rule tables: {e}"))
                    .and_then(|rules| {
                        rules
                            .into_iter()
                            .enumerate()
                            .map(|(i, rule)| UserRule::compile(rule, format!("safety.{key}[{i}]")))
                            .collect::<Result<Vec<_>, _>>()
                    });
                match compiled {
                    Ok(rules) => *target = rules,
                    Err(error) => {
                        c.safety_error = Some(error.clone());
                        r.warnings.push(error);
                    }
                }
            }
        }
        if let Some(v) = r.list("safety", "protected_paths") {
            c.protected_paths = v.iter().map(|p| expand_home(p)).collect();
        }
        if let Some(v) = r.str("safety", "fallback_shell") {
            c.fallback_shell = v;
        }
        if c.safety_error.is_none() {
            c.safety_error = r
                .warnings
                .iter()
                .find(|w| {
                    w.contains("safety.")
                        || w.contains("agent.approval")
                        || w.starts_with("safety must")
                })
                .cloned();
        }
        c.warnings = r.warnings;
        c
    }

    pub fn print_warnings(&self) {
        let file = self
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "config".into());
        for w in &self.warnings {
            eprintln!("nosh: {file}: {w}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_configuration_uses_supported_settings() {
        let document = include_str!("../../../docs/DESIGN.md").replace("\r\n", "\n");
        let example = document
            .split_once("```toml\n")
            .expect("configuration example")
            .1
            .split_once("\n```")
            .expect("closed configuration example")
            .0;
        let config = Config::parse(example);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert!(config.safety_error.is_none(), "{:?}", config.safety_error);
    }

    #[test]
    fn unsupported_configuration_is_reported_instead_of_reserved() {
        let config = Config::parse(
            "[engine]\nshared = true\nidle_exit_minutes = 15\nkv_budget = 25\n\
             [model]\nthinking = 'auto'\n",
        );
        assert!(!config.thinking);
        assert_eq!(
            config.warnings,
            ["unknown section [engine]", "model.thinking: off | on"]
        );
        for (value, expected) in [("on", true), ("off", false)] {
            let config = Config::parse(&format!("[model]\nthinking = '{value}'"));
            assert_eq!(config.thinking, expected);
            assert!(config.warnings.is_empty());
        }
    }

    #[test]
    fn editing_settings_are_typed_and_invalid_groups_fall_back_without_losing_other_settings() {
        let config = Config::parse(
            r#"
[shell]
edit_mode = "vi"
input_assist = false
[shell.keybindings]
ai_suggest = ["F2", "F3"]
undo = ["Ctrl+Z", "Ctrl+_"]
[shell.keybindings.modes.vi_normal]
redo = ["F4"]
[shell.keybindings.contexts.history_search]
cancel = ["Ctrl+G", "F5"]
"#,
        );
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert_eq!(config.editing.mode, nosh_shell::editing::Mode::Vi);
        assert_eq!(
            config.editing.keybindings.actions["ai_suggest"],
            ["F2", "F3"]
        );
        assert!(!config.input_assist);
        for invalid in [
            "edit_mode = 'wrong'",
            "edit_mode = 42",
            "edit_mode = 'vi'\nkeybindings = { unknown = ['F2'] }",
            "edit_mode = 'vi'\nkeybindings = { undo = ['F2'], redo = ['F2'] }",
            "edit_mode = 'vi'\nkeybindings = { ai_suggest = ['Ctrl+C'] }",
            "edit_mode = 'vi'\nkeybindings = { undo = 'F2' }",
        ] {
            let config = Config::parse(&format!("[shell]\ninput_assist = false\n{invalid}"));
            assert_eq!(
                config.editing.mode,
                nosh_shell::editing::Mode::Auto,
                "{invalid}"
            );
            assert!(config.editing.keybindings.actions.is_empty());
            assert!(!config.input_assist);
            assert!(
                config
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("using defaults")),
                "{invalid}"
            );
        }
    }

    #[test]
    fn device_requests_are_preserved_or_rejected_never_silently_cpu() {
        assert_eq!(
            Config::default().model_device,
            Ok(nosh_llm::InferenceDevice::Auto)
        );
        for (text, expected) in [
            ("cpu", nosh_llm::InferenceDevice::Cpu),
            ("auto", nosh_llm::InferenceDevice::Auto),
            ("cuda", nosh_llm::InferenceDevice::Cuda(0)),
            ("cuda:1", nosh_llm::InferenceDevice::Cuda(1)),
        ] {
            let config = Config::parse(&format!("[model]\ndevice = \"{text}\""));
            assert_eq!(config.model_device, Ok(expected));
            assert!(config.warnings.is_empty());
        }
        for text in ["\"metal\"", "\"cdua\"", "42"] {
            let config = Config::parse(&format!("[model]\ndevice = {text}"));
            assert!(config.model_device.is_err());
            assert!(!config.warnings.is_empty());
        }
    }

    #[test]
    fn parses_known_keys_and_warns_on_unknown() {
        let c = Config::parse(
            r#"
[shell]
ai_prefix = "?"
on_failure = "auto"
nl_guard = "off"
colour = "blue"

[agent]
approval = "yolo"
max_steps = 5
command_timeout_sec = 9999

[model]
id = "minicpm5-1b:q4_k_m"
thinking = "on"

[safety]
deny = [{ command_prefix = "docker system prune" }]
protected_paths = ["~/.secrets"]

[telemetry]
on = true
"#,
        );
        assert_eq!(c.ai_prefix, "?");
        assert_eq!(c.on_failure, OnFailure::Auto);
        assert!(!c.nl_guard);
        assert_eq!(c.approval, ApprovalMode::Yolo);
        assert_eq!(c.max_steps, 5);
        assert_eq!(c.command_timeout_sec, 60, "out of range keeps the default");
        assert_eq!(c.model_id.as_deref(), Some("minicpm5-1b:q4_k_m"));
        assert!(c.thinking);
        assert_eq!(c.deny.len(), 1);
        assert_eq!(
            c.deny[0].spec.command_prefix.as_deref(),
            Some("docker system prune")
        );
        assert!(c.safety_error.is_none());
        assert!(c.protected_paths[0].ends_with(".secrets"));
        let w = c.warnings.join("\n");
        assert!(w.contains("unknown key shell.colour"), "{w}");
        assert!(w.contains("unknown section [telemetry]"), "{w}");
        assert!(w.contains("agent.command_timeout_sec"), "{w}");
    }

    #[test]
    fn invalid_toml_falls_back() {
        let c = Config::parse("[shell\nx=");
        assert_eq!(c.ai_prefix, "#");
        assert_eq!(c.warnings.len(), 1);
        assert!(c.safety_error.is_some());
    }

    #[test]
    fn inline_prefix_is_the_only_supported_command_entry_setting() {
        let config = Config::parse("[shell]\nai_prefix = '?'\nbuiltin_name = 'ask'");
        assert_eq!(config.ai_prefix, "?");
        assert_eq!(config.warnings, ["unknown key shell.builtin_name"]);
        let empty = Config::parse("[shell]\nai_prefix = ''");
        assert!(empty.ai_prefix.is_empty() && empty.warnings.is_empty());
    }

    #[test]
    fn inline_prefix_rejects_leading_whitespace_without_disabling_commands() {
        for prefix in [" ", "\t", "\n#", " ?", "\u{3000}?", "\u{00a0}#"] {
            let config = Config::parse(&format!(
                "[shell]\nai_prefix = {}",
                serde_json::to_string(prefix).unwrap()
            ));
            assert_eq!(config.ai_prefix, "#", "{prefix:?}");
            assert_eq!(config.warnings.len(), 1, "{prefix:?}");
            assert!(config.warnings[0].contains("shell.ai_prefix"), "{prefix:?}");
        }
    }

    #[test]
    fn inline_prefix_accepts_empty_and_recognizable_custom_values() {
        for prefix in ["", "#", "?", "##", "问", "# "] {
            let config = Config::parse(&format!(
                "[shell]\nai_prefix = {}",
                serde_json::to_string(prefix).unwrap()
            ));
            assert_eq!(config.ai_prefix, prefix);
            assert!(config.warnings.is_empty(), "{prefix:?}");
            if !prefix.is_empty() {
                assert!(matches!(
                    nosh_shell::inline_commands::parse(
                        &format!("{prefix}help"),
                        &config.ai_prefix,
                        true,
                    ),
                    nosh_shell::inline_commands::Input::Command(
                        nosh_shell::inline_commands::Command::Help,
                    ),
                ));
            }
        }
    }

    #[test]
    fn output_capture_defaults_to_last_and_validates_overrides() {
        assert_eq!(Config::default().capture_output, CaptureOutput::Last);
        for (value, expected) in [("off", CaptureOutput::Off), ("last", CaptureOutput::Last)] {
            let config = Config::parse(&format!("[shell]\ncapture_output = \"{value}\"\n"));
            assert_eq!(config.capture_output, expected);
            assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        }
        for value in ["\"all\"", "true", "42"] {
            let config = Config::parse(&format!("[shell]\ncapture_output = {value}\n"));
            assert_eq!(config.capture_output, CaptureOutput::Last);
            assert!(
                config
                    .warnings
                    .iter()
                    .any(|w| w.contains("shell.capture_output"))
            );
        }
    }

    #[test]
    fn input_assist_is_enabled_by_default_and_has_a_real_boolean_switch() {
        assert!(Config::default().input_assist);
        let disabled = Config::parse("[shell]\ninput_assist = false");
        assert!(!disabled.input_assist);
        assert!(disabled.warnings.is_empty());
        let invalid = Config::parse("[shell]\ninput_assist = \"off\"");
        assert!(invalid.input_assist);
        assert!(
            invalid
                .warnings
                .iter()
                .any(|w| w.contains("shell.input_assist: expected true or false"))
        );
    }

    #[test]
    fn completion_switches_are_real_booleans_independent_of_input_assist() {
        let defaults = Config::default();
        assert!(defaults.completion && defaults.completion_scripts);
        let disabled = Config::parse(
            "[shell]\ncompletion = false\ncompletion_scripts = false\ninput_assist = true",
        );
        assert!(!disabled.completion && !disabled.completion_scripts);
        assert!(disabled.input_assist);
        assert!(disabled.warnings.is_empty());
        let invalid = Config::parse("[shell]\ncompletion = \"off\"\ncompletion_scripts = 1");
        assert!(invalid.completion && invalid.completion_scripts);
        assert!(
            invalid
                .warnings
                .iter()
                .any(|warning| warning.contains("shell.completion"))
        );
        assert!(
            invalid
                .warnings
                .iter()
                .any(|warning| warning.contains("shell.completion_scripts"))
        );
    }

    #[test]
    fn status_bar_is_a_boolean_switch() {
        assert!(Config::default().status_bar);
        let disabled = Config::parse("[shell]\nstatus_bar = false");
        assert!(!disabled.status_bar);
        assert!(disabled.warnings.is_empty());
        let invalid = Config::parse("[shell]\nstatus_bar = \"off\"");
        assert!(invalid.status_bar);
        assert!(
            invalid
                .warnings
                .iter()
                .any(|warning| warning.contains("shell.status_bar"))
        );
    }

    #[test]
    fn unreadable_config_warns_but_missing_does_not() {
        let dir = std::env::temp_dir().join(format!("nosh-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = Config::load_from(dir.join("missing.toml"));
        assert!(missing.warnings.is_empty(), "{:?}", missing.warnings);
        assert_eq!(missing.approval, ApprovalMode::Auto);
        assert!(missing.safety_error.is_none());
        // A directory cannot be read as a file (like a permission or I/O error).
        let unreadable = Config::load_from(dir.clone());
        assert_eq!(unreadable.approval, Config::default().approval);
        assert_eq!(unreadable.warnings.len(), 1, "{:?}", unreadable.warnings);
        assert!(unreadable.warnings[0].contains("cannot be read"));
        assert!(unreadable.safety_error.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rule_tables_have_one_shape_and_invalid_rules_block_execution() {
        for text in [
            "[[safety.allow]]\ncommand_prefix = 'git fetch'\ncwd = '.'",
            "[safety]\nallow = [{ command_prefix = 'git fetch', cwd = '.' }]",
        ] {
            let cfg = Config::parse(text);
            assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
            assert!(cfg.safety_error.is_none());
            assert_eq!(cfg.allow.len(), 1);
            assert_eq!(
                cfg.allow[0].spec.command_prefix.as_deref(),
                Some("git fetch")
            );
        }
        for text in [
            "[safety]\nallow = ['git *']",
            "[safety]\ndeny = [{ command_prefix = 'git **' }]",
            "[safety]\ndeny = [{ command_prefix = 'git', command_exact = 'git status' }]",
            "[safety]\ndeny = [{ tool = 'unknown' }]",
            "[safety]\nalow = []",
            "[safety]\nprotected_paths = 1",
            "[agent]\napproval = 'typo'",
        ] {
            let cfg = Config::parse(text);
            assert!(cfg.safety_error.is_some(), "{text}");
            assert!(!cfg.warnings.is_empty(), "{text}");
        }
    }
}
