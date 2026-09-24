//! `~/.config/nosh/config.toml` (design §11): common keys only; unknown keys
//! and bad values produce warnings and fall back to defaults.

use std::path::PathBuf;

use nosh_hub::SourceSelection;
use nosh_permissions::ApprovalMode;
use nosh_shell::OnFailure;

#[derive(Debug, Clone)]
pub struct Config {
    pub ai_prefix: String,
    pub trigger_on_error: bool,
    pub on_failure: OnFailure,
    pub nl_guard: bool,
    pub builtin_name: String,
    pub approval: ApprovalMode,
    pub max_steps: usize,
    pub command_timeout_sec: u64,
    pub restore_cwd: bool,
    pub conversation_idle_minutes: u64,
    pub model_id: Option<String>,
    pub model_path: Option<PathBuf>,
    pub context_length: usize,
    pub thinking: bool,
    pub download_auto: bool,
    pub source_selection: SourceSelection,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
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
            nl_guard: true,
            builtin_name: "ai".into(),
            approval: ApprovalMode::Confirm,
            max_steps: 10,
            command_timeout_sec: 60,
            restore_cwd: false,
            conversation_idle_minutes: 30,
            model_id: None,
            model_path: None,
            context_length: 8192,
            thinking: false,
            download_auto: true,
            source_selection: SourceSelection::Auto,
            allow: Vec::new(),
            deny: Vec::new(),
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
            "nl_guard",
            "builtin_name",
            "suggest_key",
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
    ("engine", &["shared", "idle_exit_minutes", "kv_budget"]),
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
        Self::load_from(nosh_hub::paths::config_file())
    }

    /// A missing file means defaults; a file that cannot be read also gets
    /// defaults, but with a warning, since its rules would be lost silently.
    fn load_from(path: PathBuf) -> Self {
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => Self {
                warnings: vec![format!("cannot be read, using defaults: {e}")],
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
            c.ai_prefix = v;
        }
        if let Some(v) = r.bool("shell", "trigger_on_error") {
            c.trigger_on_error = v;
        }
        if let Some(v) = r.str("shell", "on_failure") {
            match OnFailure::parse(&v) {
                Some(o) => c.on_failure = o,
                None => r
                    .warnings
                    .push("shell.on_failure: hint | auto | off".into()),
            }
        }
        if let Some(v) = r.str("shell", "nl_guard") {
            match v.as_str() {
                "destructive" => c.nl_guard = true,
                "off" => c.nl_guard = false,
                _ => r.warnings.push("shell.nl_guard: destructive | off".into()),
            }
        }
        if let Some(v) = r.str("shell", "builtin_name") {
            c.builtin_name = v;
        }
        if let Some(v) = r.str("shell", "suggest_key")
            && v != "ctrl-g"
        {
            r.warnings
                .push("shell.suggest_key: only ctrl-g is supported in this version".into());
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
        if let Some(v) = r.str("model", "device")
            && v != "auto"
            && v != "cpu"
        {
            r.warnings.push(format!(
                "model.device = {v}: only cpu is available in this build"
            ));
        }
        if let Some(v) = r.str("model", "thinking") {
            match v.as_str() {
                "on" => c.thinking = true,
                "off" => c.thinking = false,
                "auto" => {
                    r.warnings
                        .push("model.thinking = auto is not implemented; using off".into());
                }
                _ => r.warnings.push("model.thinking: off | on | auto".into()),
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
        if let Some(v) = r.list("safety", "allow") {
            c.allow = v;
        }
        if let Some(v) = r.list("safety", "deny") {
            c.deny = v;
        }
        if let Some(v) = r.list("safety", "protected_paths") {
            c.protected_paths = v.iter().map(|p| expand_home(p)).collect();
        }
        if let Some(v) = r.str("safety", "fallback_shell") {
            c.fallback_shell = v;
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
deny = ["docker system prune*"]
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
        assert_eq!(c.deny, vec!["docker system prune*".to_string()]);
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
    }

    #[test]
    fn unreadable_config_warns_but_missing_does_not() {
        let dir = std::env::temp_dir().join(format!("nosh-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = Config::load_from(dir.join("missing.toml"));
        assert!(missing.warnings.is_empty(), "{:?}", missing.warnings);
        // A directory cannot be read as a file (like a permission or I/O error).
        let unreadable = Config::load_from(dir.clone());
        assert_eq!(unreadable.approval, Config::default().approval);
        assert_eq!(unreadable.warnings.len(), 1, "{:?}", unreadable.warnings);
        assert!(unreadable.warnings[0].contains("cannot be read"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
