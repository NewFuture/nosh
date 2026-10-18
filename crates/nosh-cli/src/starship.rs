//! The official CLI is the compatibility boundary; no Starship grammar or SDK is vendored.

#[cfg(unix)]
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
#[cfg(unix)]
use std::process::{Child, Stdio};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use nosh_shell::theme::{Builtin, ColorPair, Fragment, Theme};

const OUTPUT_LIMIT: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyphs {
    Plain,
    Powerline,
    NerdFont,
}

impl Glyphs {
    pub fn powerline(self) -> bool {
        self != Self::Plain
    }

    fn allows(self, character: char) -> bool {
        !matches!(character as u32, 0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd)
            || self == Self::NerdFont
            || (self == Self::Powerline && matches!(character as u32, 0xe0b0..=0xe0b7))
    }
}

fn filter_glyphs(value: &mut toml::Value, glyphs: Glyphs, display_text: bool) {
    match value {
        toml::Value::String(text) if display_text => {
            text.retain(|character| glyphs.allows(character));
        }
        toml::Value::Table(table) => {
            for (name, value) in table {
                filter_glyphs(
                    value,
                    glyphs,
                    display_text
                        || matches!(
                            name.as_str(),
                            "format" | "symbol" | "symbols" | "substitutions"
                        )
                        || name.starts_with("symbol_")
                        || name.ends_with("_symbol"),
                );
            }
        }
        toml::Value::Array(values) => {
            for value in values {
                filter_glyphs(value, glyphs, display_text);
            }
        }
        _ => {}
    }
}

#[cfg(unix)]
struct Process {
    child: Child,
    reaped: bool,
}

#[cfg(unix)]
impl Drop for Process {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // The leader is not reaped before this guard, so its process group cannot be reused.
        let killed = unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        if killed < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            eprintln!(
                "nosh: cannot stop Starship process group: {}",
                std::io::Error::last_os_error()
            );
            if let Err(error) = self.child.kill() {
                eprintln!("nosh: cannot stop Starship: {error}");
            }
        }
        if let Err(error) = self.child.wait() {
            eprintln!("nosh: cannot reap Starship: {error}");
        }
    }
}

#[cfg(unix)]
fn exited(child: &Child) -> Result<bool, String> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: valid storage; WNOWAIT retains ownership of the leader PID until cleanup.
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let info = unsafe { info.assume_init() };
    #[cfg(target_os = "linux")]
    let pid = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let pid = info.si_pid;
    Ok(pid != 0)
}

#[cfg(unix)]
fn nonblocking(fd: i32) -> Result<(), String> {
    // SAFETY: caller retains the open pipe throughout the operation.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(unix)]
fn read_available(reader: &mut impl Read, bytes: &mut Vec<u8>) -> Result<bool, String> {
    let mut chunk = [0; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(true),
            Ok(size) => {
                if bytes.len() + size > OUTPUT_LIMIT {
                    return Err("Starship output exceeded 64 KiB".into());
                }
                bytes.extend_from_slice(&chunk[..size]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn run(
    config: &toml::Table,
    cwd: &Path,
    environment: Option<&[(String, String)]>,
    previous: Option<&nosh_shell::UserCommand>,
    cancelled: &dyn Fn() -> bool,
) -> Result<String, String> {
    let mut file = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    file.write_all(
        toml::to_string(config)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    let mut command = Command::new("starship");
    if let Some(environment) = environment {
        command.env_clear().envs(environment.iter().cloned());
    }
    command
        .args([
            "prompt",
            "--status",
            &previous.map_or(0, |command| command.exit).to_string(),
            "--cmd-duration",
            &previous
                .map_or(0, |command| command.duration.as_millis())
                .to_string(),
            "--terminal-width",
            "512",
        ])
        .arg("--path")
        .arg(cwd)
        .arg("--logical-path")
        .arg(cwd)
        .current_dir(cwd)
        .env("STARSHIP_CONFIG", file.path())
        .env("STARSHIP_SHELL", "nu")
        .env("STARSHIP_LOG", "warn")
        .env("STARSHIP_NUM_THREADS", "2")
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("__NOSH_STYLE_PROBE_UNSET");
    collect(&mut command, cancelled, TIMEOUT)
}

#[cfg(unix)]
fn collect(
    command: &mut Command,
    cancelled: &dyn Fn() -> bool,
    timeout: Duration,
) -> Result<String, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe. The adapter never grants a controlling terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = Process {
        child: command.spawn().map_err(|e| {
            format!("cannot start Starship ({e}); install Starship or select an internal theme")
        })?,
        reaped: false,
    };
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or("missing Starship stdout")?;
    let mut stderr = process
        .child
        .stderr
        .take()
        .ok_or("missing Starship stderr")?;
    nonblocking(stdout.as_raw_fd())?;
    nonblocking(stderr.as_raw_fd())?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let deadline = Instant::now() + timeout;
    let (mut out_done, mut err_done) = (false, false);
    loop {
        if cancelled() {
            return Err("Starship cancelled".into());
        }
        if Instant::now() >= deadline {
            return Err("Starship exceeded its two-second prompt budget".into());
        }
        if !out_done {
            out_done = read_available(&mut stdout, &mut out)?;
        }
        if !err_done {
            err_done = read_available(&mut stderr, &mut err)?;
        }
        if out_done && err_done && exited(&process.child)? {
            break;
        }
        let mut fds = [
            libc::pollfd {
                fd: if out_done { -1 } else { stdout.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if err_done { -1 } else { stderr.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 10) } < 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    // Stop any descendants while the unreaped leader still reserves the group ID.
    if unsafe { libc::kill(-(process.child.id() as i32), libc::SIGKILL) } < 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err(format!(
            "cannot stop Starship descendants: {}",
            std::io::Error::last_os_error()
        ));
    }
    let status = process.child.wait().map_err(|e| e.to_string())?;
    process.reaped = true;
    let err = String::from_utf8(err).map_err(|_| "Starship diagnostics are not UTF-8")?;
    if !status.success() || !err.trim().is_empty() {
        return Err(format!(
            "Starship {status}: {}",
            nosh_shell::style::visible_text(err.trim())
        ));
    }
    String::from_utf8(out).map_err(|_| "Starship output is not UTF-8".into())
}

#[cfg(windows)]
fn collect(
    command: &mut Command,
    cancelled: &dyn Fn() -> bool,
    timeout: Duration,
) -> Result<String, String> {
    let output = nosh_platform::process::capture(command, timeout, OUTPUT_LIMIT, cancelled)
        .map_err(|error| format!("Starship: {error}"))?;
    if !output.complete {
        return Err("Starship output exceeded 64 KiB".into());
    }
    let error =
        String::from_utf8(output.stderr).map_err(|_| "Starship diagnostics are not UTF-8")?;
    if !output.status.is_some_and(|status| status.success()) || !error.trim().is_empty() {
        return Err(format!(
            "Starship {:?}: {}",
            output.status,
            nosh_shell::style::visible_text(error.trim())
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| "Starship output is not UTF-8".into())
}

fn custom_style<'a>(table: &'a toml::Table, role: &str) -> Result<Option<&'a str>, String> {
    let key = format!("nosh_{role}");
    let Some(module) = table
        .get("custom")
        .and_then(toml::Value::as_table)
        .and_then(|m| m.get(&key))
    else {
        return Ok(None);
    };
    let module = module
        .as_table()
        .ok_or_else(|| format!("custom.{key} must be a table"))?;
    module
        .get("style")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| format!("custom.{key}.style must be a string"))
        })
        .transpose()
}

fn style_string(pair: ColorPair) -> String {
    let [r, g, b] = pair.foreground_rgb();
    let [br, bg, bb] = pair.background_rgb();
    format!("fg:#{r:02x}{g:02x}{b:02x} bg:#{br:02x}{bg:02x}{bb:02x}")
}

pub fn compile(source: &str) -> Result<Theme, String> {
    let table: toml::Table = source
        .parse()
        .map_err(|e| format!("invalid Starship TOML: {e}"))?;
    validate_custom(&table)?;
    let palette_name = table.get("palette").and_then(toml::Value::as_str);
    let palette =
        palette_name.and_then(|name| table.get("palettes")?.as_table()?.get(name)?.as_table());
    let reference = palette.and_then(|palette| {
        ["nosh_text_bg", "base", "background"]
            .into_iter()
            .find_map(|key| {
                let value = palette.get(key)?.as_str()?.strip_prefix('#')?;
                if value.len() != 6 {
                    return None;
                }
                let rgb = u32::from_str_radix(value, 16).ok()?.to_be_bytes();
                Some([rgb[1], rgb[2], rgb[3]])
            })
    });
    let light = reference.map_or_else(
        || palette_name.is_some_and(|name| name.contains("latte") || name.contains("light")),
        |rgb| nosh_shell::theme::contrast(rgb, [0; 3]) > nosh_shell::theme::contrast(rgb, [255; 3]),
    );
    let mut theme = if light { Builtin::Light } else { Builtin::Dark }.palette();
    let mut probe = toml::Table::new();
    for key in ["palette", "palettes"] {
        if let Some(value) = table.get(key) {
            probe.insert(key.into(), value.clone());
        }
    }
    probe.insert("add_newline".into(), false.into());
    let mut format = String::new();
    let mut variables = toml::Table::new();
    let pairs: Vec<_> = theme.pairs().collect();
    let mut requested_styles = Vec::with_capacity(pairs.len());
    for (index, (role, pair)) in pairs.iter().enumerate() {
        let fallback = style_string(*pair);
        let module_name = match *role {
            "environment" => Some("directory"),
            "git_branch" => Some("git_branch"),
            _ => None,
        };
        let module_style = if let Some(module_name) = module_name {
            table
                .get(module_name)
                .and_then(toml::Value::as_table)
                .and_then(|v| v.get("style"))
                .map(|v| {
                    v.as_str()
                        .ok_or_else(|| format!("{module_name}.style must be a string"))
                })
                .transpose()?
        } else {
            None
        };
        let style = custom_style(&table, role)?
            .or(module_style)
            .unwrap_or(&fallback);
        requested_styles.push(style.to_owned());
        let name = format!("nosh_style_{index}");
        let mut value = toml::Table::new();
        value.insert("variable".into(), "__NOSH_STYLE_PROBE_UNSET".into());
        value.insert("default".into(), "X".into());
        value.insert("format".into(), "[$env_value]($style)".into());
        value.insert("style".into(), style.into());
        variables.insert(name.clone(), value.into());
        format.push_str(&format!("${{env_var.{name}}}"));
    }
    probe.insert("env_var".into(), variables.into());
    probe.insert("format".into(), format.into());
    let output = run(
        &probe,
        &std::env::current_dir().map_err(|e| e.to_string())?,
        None,
        None,
        &|| false,
    )?;
    let fragment = Fragment::parse(&output);
    let styles: Vec<_> = fragment
        .runs()
        .flat_map(|(text, style)| text.chars().map(move |c| (c, style)))
        .collect();
    if styles.len() != pairs.len() || styles.iter().any(|(c, _)| *c != 'X') {
        return Err("Starship did not return a complete static style map".into());
    }
    for (((role, fallback), (_, style)), requested) in
        pairs.into_iter().zip(styles).zip(requested_styles)
    {
        if style == Default::default()
            && !requested
                .split_whitespace()
                .all(|token| matches!(token, "none" | "fg:none" | "bg:none"))
        {
            return Err(format!(
                "{role}: Starship returned no usable style for {requested:?}"
            ));
        }
        let pair = ColorPair::from_style(style, fallback).map_err(|e| format!("{role}: {e}"))?;
        theme.override_color(role, pair)?;
    }
    theme.validate()?;
    Ok(theme)
}

fn validate_custom(table: &toml::Table) -> Result<(), String> {
    if let Some(custom) = table.get("custom") {
        for (name, value) in custom.as_table().ok_or("custom must be a table")? {
            if let Some(role) = name.strip_prefix("nosh_")
                && !Theme::default().pairs().any(|(name, _)| name == role)
            {
                return Err(format!("custom.{name}: unknown reserved nosh style role"));
            }
            let module = value
                .as_table()
                .ok_or_else(|| format!("custom.{name} must be a table"))?;
            if module.get("disabled").and_then(toml::Value::as_bool) == Some(true) {
                continue;
            }

            if module.contains_key("command") || module.get("when").is_some_and(toml::Value::is_str)
            {
                return Err(format!(
                    "custom.{name}: theme imports cannot execute custom command/when scripts; disable this module explicitly"
                ));
            }
        }
    }
    Ok(())
}

pub fn environment(
    source: &str,
    shell: &nosh_shell::EmbeddedShell,
    glyphs: Glyphs,
) -> Result<Fragment, String> {
    let mut table: toml::Table = source
        .parse()
        .map_err(|e| format!("invalid Starship TOML: {e}"))?;
    validate_custom(&table)?;
    table.insert("add_newline".into(), false.into());
    table.remove("custom");
    for name in ["character", "line_break"] {
        let module = table
            .entry(name)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        module
            .as_table_mut()
            .ok_or_else(|| format!("{name} must be a table"))?
            .insert("disabled".into(), true.into());
    }
    let mut value = toml::Value::Table(table);
    if glyphs != Glyphs::NerdFont {
        filter_glyphs(&mut value, glyphs, false);
    }
    let table = value.as_table().expect("table stays a table");
    let variables: Vec<_> = shell
        .scalar_vars()
        .into_iter()
        .filter(|(_, _, exported)| *exported)
        .map(|(name, value, _)| (name, value))
        .collect();
    let count = shell.interrupts().count();
    let output = run(
        table,
        &shell.cwd(),
        Some(&variables),
        shell.recent_commands().last(),
        &|| shell.interrupts().count() != count,
    )?;
    Ok(Fragment::parse(&output))
}

pub fn export(builtin: Builtin, nerd_font: bool) -> String {
    let theme = builtin.palette();
    let mut table = toml::Table::new();
    table.insert(
        "$schema".into(),
        "https://starship.rs/config-schema.json".into(),
    );
    table.insert("palette".into(), builtin.name().replace('-', "_").into());
    table.insert("add_newline".into(), false.into());
    let mut format =
        "$directory$git_branch$git_status$rust$nodejs$golang$python$conda$status$cmd_duration"
            .to_owned();
    if nerd_font {
        format.push_str("[\u{e0b4}](fg:prev_bg)");
    }
    table.insert("format".into(), format.into());
    let mut colors = toml::Table::new();
    let mut custom = toml::Table::new();
    for (role, pair) in theme.pairs() {
        for (suffix, rgb) in [("fg", pair.foreground_rgb()), ("bg", pair.background_rgb())] {
            colors.insert(
                format!("nosh_{role}_{suffix}"),
                format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]).into(),
            );
        }
        let mut module = toml::Table::new();
        let mut style = format!("fg:nosh_{role}_fg bg:nosh_{role}_bg");
        let attributes = pair.attributes();
        for (enabled, attribute) in [
            (attributes.is_bold, "bold"),
            (attributes.is_italic, "italic"),
            (attributes.is_underline, "underline"),
            (attributes.is_strikethrough, "strikethrough"),
        ] {
            if enabled {
                style.push(' ');
                style.push_str(attribute);
            }
        }
        module.insert("style".into(), style.into());
        module.insert("disabled".into(), true.into());
        module.insert(
            "description".into(),
            format!("nosh live {role} style; no command is executed").into(),
        );
        custom.insert(format!("nosh_{role}"), module.into());
    }
    table.insert(
        "palettes".into(),
        toml::Table::from_iter([(builtin.name().replace('-', "_"), colors.into())]).into(),
    );
    table.insert("custom".into(), custom.into());
    for (name, format, foreground, background) in [
        (
            "directory",
            "[ $path ]($style)",
            "nosh_environment_fg",
            "nosh_environment_bg",
        ),
        (
            "git_branch",
            "[ ($branch) ]($style)",
            "nosh_git_branch_fg",
            "nosh_git_branch_bg",
        ),
        (
            "rust",
            "[ rs $version ]($style)",
            "nosh_positive_bg",
            "nosh_positive_fg",
        ),
        (
            "nodejs",
            "[ node $version ]($style)",
            "nosh_positive_bg",
            "nosh_positive_fg",
        ),
        (
            "python",
            "[ py $version( $virtualenv) ]($style)",
            "nosh_positive_bg",
            "nosh_positive_fg",
        ),
        (
            "golang",
            "[ go $version ]($style)",
            "nosh_positive_bg",
            "nosh_positive_fg",
        ),
        (
            "git_status",
            "[ $all_status$ahead_behind ]($style)",
            "nosh_git_branch_fg",
            "nosh_git_branch_bg",
        ),
        (
            "conda",
            "[ conda $environment ]($style)",
            "nosh_accent_bg",
            "nosh_accent_fg",
        ),
        (
            "status",
            "[ exit $status ]($style)",
            "nosh_failure_fg",
            "nosh_failure_bg",
        ),
        (
            "cmd_duration",
            "[ $duration ]($style)",
            "nosh_pending_fg",
            "nosh_pending_bg",
        ),
    ] {
        let mut module = toml::Table::new();
        module.insert(
            "style".into(),
            format!("fg:{foreground} bg:{background}").into(),
        );
        let format = if nerd_font && name == "directory" {
            format!("([\u{e0b6}](fg:{background}){format})")
        } else if nerd_font {
            format!("([\u{e0b0}](fg:prev_bg bg:{background}){format})")
        } else {
            format!("({format})")
        };
        module.insert("format".into(), format.into());
        if name == "status" {
            module.insert("disabled".into(), false.into());
        }
        if name == "cmd_duration" {
            module.insert("min_time".into(), 2000.into());
        }
        table.insert(name.into(), module.into());
    }
    toml::to_string_pretty(&table).expect("native theme is serializable")
}

#[cfg(test)]
mod tests;
