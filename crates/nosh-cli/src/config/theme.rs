use std::borrow::Cow;
use std::fs::{self, OpenOptions};
#[cfg(all(test, unix))]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nosh_shell::theme::{Builtin, Theme};

use super::read_text;
use crate::starship::Glyphs;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub palette: Theme,
    source: ThemeSource,
    environment_enabled: bool,
    glyphs: Glyphs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ThemeSource {
    Builtin(Builtin),
    Starship { path: PathBuf, text: Arc<str> },
}

impl Default for Selection {
    fn default() -> Self {
        let name = Builtin::default();
        let glyphs = automatic_glyphs();
        Self {
            palette: Theme {
                powerline: glyphs.powerline(),
                ..name.palette()
            },
            source: ThemeSource::Builtin(name),
            environment_enabled: false,
            glyphs,
        }
    }
}

impl Selection {
    fn loaded(&self) -> nosh_shell::repl::LoadedTheme {
        nosh_shell::repl::LoadedTheme {
            palette: self.palette,
            builtin: self.builtin(),
        }
    }

    pub fn builtin(&self) -> Option<Builtin> {
        match self.source {
            ThemeSource::Builtin(name) => Some(name),
            ThemeSource::Starship { .. } => None,
        }
    }

    pub fn label(&self) -> String {
        match &self.source {
            ThemeSource::Builtin(name) => name.name().into(),
            ThemeSource::Starship { path, .. } => path.display().to_string(),
        }
    }

    pub fn starship(&self) -> Cow<'_, str> {
        match &self.source {
            ThemeSource::Builtin(name) => {
                crate::starship::export(*name, self.palette.powerline).into()
            }
            ThemeSource::Starship { text, .. } => Cow::Borrowed(text),
        }
    }
}

pub fn selection_key(value: &str, cwd: &Path) -> Result<String, String> {
    if let Ok(builtin) = value.parse::<Builtin>() {
        return Ok(builtin.name().into());
    }
    fs::canonicalize(cwd.join(super::expand_home(value)))
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|e| format!("theme {value:?}: {e}"))
}

pub fn load(path: &Path) -> Result<Selection, String> {
    let text = read_text(path)?;
    let table = text
        .parse::<toml::Table>()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    resolve_at(&table, None, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn choice<'a>(table: &'a toml::Table, requested: Option<&'a str>) -> Result<&'a str, String> {
    let shell = table
        .get("shell")
        .map(|value| value.as_table().ok_or("shell must be a table"))
        .transpose()?;
    if let Some(value) = requested {
        return Ok(value);
    }
    match shell.and_then(|shell| shell.get("theme")) {
        Some(value) => value
            .as_str()
            .ok_or_else(|| "shell.theme: expected a string".into()),
        None => Ok(Builtin::default().name()),
    }
}

pub fn resolve_at(
    table: &toml::Table,
    requested: Option<&str>,
    config_path: &Path,
) -> Result<Selection, String> {
    let name = choice(table, requested)?;
    resolve_prepared(table, theme_source(name, config_path)?, None)
}

fn bool_setting(table: &toml::Table, name: &str) -> Result<bool, String> {
    table
        .get("shell")
        .and_then(toml::Value::as_table)
        .and_then(|shell| shell.get(name))
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| format!("shell.{name}: expected true or false"))
        })
        .transpose()
        .map(|value| value.unwrap_or(false))
}

fn automatic_glyphs() -> Glyphs {
    if nosh_shell::theme::automatic_powerline(nosh_shell::style::stdout()) {
        Glyphs::Powerline
    } else {
        Glyphs::Plain
    }
}

fn glyph_setting(table: &toml::Table) -> Result<Glyphs, String> {
    match table
        .get("shell")
        .and_then(toml::Value::as_table)
        .and_then(|shell| shell.get("nerd_font"))
    {
        None => Ok(automatic_glyphs()),
        Some(toml::Value::String(value)) if value == "auto" => Ok(automatic_glyphs()),
        Some(toml::Value::Boolean(true)) => Ok(Glyphs::NerdFont),
        Some(toml::Value::Boolean(false)) => Ok(Glyphs::Plain),
        _ => Err("shell.nerd_font: expected \"auto\", true or false".into()),
    }
}

fn theme_source(name: &str, config_path: &Path) -> Result<ThemeSource, String> {
    if let Ok(name) = name.parse::<Builtin>() {
        return Ok(ThemeSource::Builtin(name));
    }
    use std::io::Read;
    let source_path = super::expand_home(name);
    let path = if source_path.is_absolute() {
        source_path
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(source_path)
    };
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options
        .open(&path)
        .map_err(|e| format!("theme {}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Starship theme must be a regular file".into());
    }
    let mut source = String::new();
    file.take(256 * 1024 + 1)
        .read_to_string(&mut source)
        .map_err(|e| e.to_string())?;
    if source.len() > 256 * 1024 {
        return Err("Starship theme exceeds 256 KiB".into());
    }
    Ok(ThemeSource::Starship {
        path,
        text: source.into(),
    })
}

fn resolve_prepared(
    table: &toml::Table,
    source: ThemeSource,
    previous: Option<&Selection>,
) -> Result<Selection, String> {
    let environment_enabled = bool_setting(table, "starship")?;
    let glyphs = glyph_setting(table)?;
    let mut palette = if let Some(previous) = previous.filter(|old| old.source == source) {
        previous.palette
    } else {
        match &source {
            ThemeSource::Builtin(name) => name.palette(),
            ThemeSource::Starship { text, .. } => crate::starship::compile(text)?,
        }
    };
    palette.powerline = glyphs.powerline();
    Ok(Selection {
        palette,
        source,
        environment_enabled,
        glyphs,
    })
}

/// Configuration and parsed styles are refreshed only at the next prompt, and only
/// a changed theme invokes the upstream style parser again.
pub struct Loader {
    path: PathBuf,
    cache: Mutex<Selection>,
}

impl Loader {
    pub fn new(path: PathBuf, initial: Selection) -> Self {
        Self {
            path,
            cache: Mutex::new(initial),
        }
    }

    pub fn refresh(&self) -> Result<nosh_shell::repl::LoadedTheme, String> {
        let text = read_text(&self.path)?;
        let table: toml::Table = text
            .parse()
            .map_err(|e| format!("invalid configuration: {e}"))?;
        let source = theme_source(choice(&table, None)?, &self.path)?;
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| "theme loader lock poisoned")?;
        let selection = resolve_prepared(&table, source, Some(&cache))?;
        let loaded = selection.loaded();
        *cache = selection;
        Ok(loaded)
    }

    pub fn environment(
        &self,
        shell: &nosh_shell::EmbeddedShell,
    ) -> Result<Option<nosh_shell::theme::Fragment>, String> {
        let selection = {
            let cache = self
                .cache
                .lock()
                .map_err(|_| "theme loader lock poisoned")?;
            if !cache.environment_enabled {
                return Ok(None);
            }
            cache.clone()
        };
        let glyphs = if selection
            .palette
            .styles(nosh_shell::style::stdout())
            .powerline()
        {
            selection.glyphs
        } else {
            Glyphs::Plain
        };
        crate::starship::environment(&selection.starship(), shell, glyphs).map(Some)
    }
}

pub fn save(path: &Path, requested: &str) -> Result<(), String> {
    save_checked(path, requested, || {})
}

fn save_checked(path: &Path, requested: &str, before_commit: impl FnOnce()) -> Result<(), String> {
    super::document::update_checked(
        path,
        |document| {
            let table = document
                .to_string()
                .parse::<toml::Table>()
                .map_err(|e| format!("invalid configuration: {e}"))?;
            resolve_at(&table, Some(requested), path)?;
            if !document.contains_key("shell") {
                document["shell"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            let shell = document["shell"]
                .as_table_like_mut()
                .ok_or("shell must be a table")?;
            let mut value = toml_edit::Value::from(requested);
            if let Some(old) = shell.get("theme").and_then(toml_edit::Item::as_value) {
                *value.decor_mut() = old.decor().clone();
            }
            shell.insert("theme", toml_edit::Item::Value(value));
            Ok(())
        },
        before_commit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_and_capability_options_are_validated() {
        let path = Path::new("config.toml");
        assert_eq!(
            resolve_at(&toml::Table::new(), None, path).unwrap(),
            Selection::default()
        );
        assert_eq!(
            resolve_at(&"[shell]\nnerd_font = 'auto'".parse().unwrap(), None, path).unwrap(),
            Selection::default()
        );
        let table = "[shell]\ntheme = 'soft-light'\nnerd_font = true\nstarship = true"
            .parse()
            .unwrap();
        let light = resolve_at(&table, None, path).unwrap();
        assert_eq!(light.builtin(), Some(Builtin::Light));
        assert!(light.palette.powerline);
        assert_eq!(light.glyphs, Glyphs::NerdFont);
        let disabled =
            resolve_at(&"[shell]\nnerd_font = false".parse().unwrap(), None, path).unwrap();
        assert!(!disabled.palette.powerline);
        assert_eq!(disabled.glyphs, Glyphs::Plain);
        for bad in [
            "shell = 42",
            "[shell]\ntheme = 42",
            "[shell]\nstarship = 'yes'",
            "[shell]\nnerd_font = 1",
            "[shell]\nnerd_font = 'on'",
            "[shell]\nnerd_font = 'AUTO'",
        ] {
            assert!(
                resolve_at(&bad.parse().unwrap(), None, path).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn loaded_theme_marks_only_actual_builtin_selections() {
        let mut selection = Selection::default();
        assert_eq!(selection.loaded().builtin, Some(Builtin::Dark));
        selection.source = ThemeSource::Starship {
            path: PathBuf::from("custom.toml"),
            text: "".into(),
        };
        assert_eq!(selection.loaded().builtin, None);
        assert_eq!(selection.loaded().palette, selection.palette);
    }

    #[test]
    fn cached_theme_applies_display_options_but_retains_valid_state_on_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "[shell]\ntheme = 'soft-light'\nnerd_font = false\n").unwrap();
        let loader = Loader::new(path.clone(), load(&path).unwrap());
        assert!(!loader.refresh().unwrap().palette.powerline);
        fs::write(
            &path,
            "[shell]\ntheme = 'soft-light'\nnerd_font = true\nstarship = true\n",
        )
        .unwrap();
        let loaded = loader.refresh().unwrap();
        assert!(loaded.palette.powerline);
        assert_eq!(loaded.builtin, Some(Builtin::Light));
        let previous = loader.cache.lock().unwrap().clone();
        assert!(previous.environment_enabled);
        fs::write(&path, "[shell]\nnerd_font = 'yes'\n").unwrap();
        assert!(loader.refresh().is_err());
        assert_eq!(*loader.cache.lock().unwrap(), previous);
    }

    #[test]
    fn initial_theme_survives_a_failed_first_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "[shell]\ntheme = 'soft-light'\n").unwrap();
        let initial = load(&path).unwrap();
        let loader = Loader::new(path.clone(), initial.clone());
        fs::write(&path, "broken [").unwrap();
        assert!(loader.refresh().is_err());
        assert_eq!(*loader.cache.lock().unwrap(), initial);
    }

    #[test]
    fn selection_paths_use_the_callers_working_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("my theme.toml");
        fs::write(&path, "").unwrap();
        assert_eq!(
            selection_key("./my theme.toml", directory.path()).unwrap(),
            path.canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(
            selection_key("soft-light", Path::new("missing")).unwrap(),
            "soft-light"
        );
    }

    #[test]
    fn save_preserves_comments_permissions_and_unrelated_configuration() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let text = "# keep me\n[shell]\ntheme = 'soft-dark' # chosen\nstatus_bar = false\n\n[safety]\ndeny = [{ command_prefix = 'rm' }] # important\n";
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let original_group = fs::metadata(&path).unwrap().gid();
        save(&path, Builtin::Light.name()).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("# keep me") && updated.contains("# chosen"));
        assert!(updated.contains("deny = [{ command_prefix = 'rm' }] # important"));
        assert_eq!(load(&path).unwrap().builtin(), Some(Builtin::Light));
        assert_eq!(fs::metadata(&path).unwrap().gid(), original_group);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let before: toml::Table = text.parse().unwrap();
        let after: toml::Table = updated.parse().unwrap();
        assert_eq!(before["safety"], after["safety"]);
        assert_eq!(before["shell"]["status_bar"], after["shell"]["status_bar"]);
    }

    #[test]
    fn invalid_readonly_symlink_and_concurrent_saves_do_not_overwrite() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "broken [").unwrap();
        assert!(save(&path, Builtin::Light.name()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "broken [");
        fs::write(&path, "[shell]\ntheme = 'soft-dark'\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(save(&path, Builtin::Light.name()).is_err());
        assert_eq!(load(&path).unwrap().builtin(), Some(Builtin::Dark));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let changed = "[shell]\ntheme = 'soft-dark'\n# concurrent\n";
        assert!(
            save_checked(&path, Builtin::Light.name(), || fs::write(&path, changed)
                .unwrap())
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), changed);
        let link = directory.path().join("linked.toml");
        symlink(&path, &link).unwrap();
        assert!(save(&link, Builtin::Light.name()).is_err());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn missing_and_inline_shell_tables_can_be_saved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("config.toml");
        save(&path, Builtin::Light.name()).unwrap();
        assert_eq!(load(&path).unwrap().builtin(), Some(Builtin::Light));
        fs::write(
            &path,
            "shell = { theme = 'soft-dark', status_bar = false }\n",
        )
        .unwrap();
        save(&path, Builtin::Light.name()).unwrap();
        assert_eq!(load(&path).unwrap().builtin(), Some(Builtin::Light));
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("status_bar = false")
        );
    }
}
