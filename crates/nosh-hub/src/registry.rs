//! Built-in model registry (`assets/registry.toml`).

use serde::Deserialize;

use crate::HubError;

const BUILTIN: &str = include_str!("../../../assets/registry.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct Registry {
    pub schema: u32,
    #[serde(rename = "model")]
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub default: bool,
    pub display: String,
    pub arch: String,
    pub chat_format: String,
    pub context_max: usize,
    #[serde(default)]
    pub min_memory_mb: u64,
    pub eog_ids: Vec<u32>,
    pub license: String,
    pub sampling: Sampling,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub struct Sampling {
    pub temperature: f32,
    pub top_p: f32,
    pub min_p: f32,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileRole {
    Weights,
    Tokenizer,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    pub role: FileRole,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub sources: Vec<SourceRef>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SourceRef {
    pub hub: String,
    pub repo: String,
    #[serde(default = "default_revision")]
    pub revision: String,
}

fn default_revision() -> String {
    "main".to_string()
}

impl Registry {
    pub fn builtin() -> Self {
        Self::parse(BUILTIN).expect("built-in registry.toml is valid")
    }

    pub fn parse(src: &str) -> Result<Self, HubError> {
        let reg: Registry = toml::from_str(src)
            .map_err(|e| HubError::Registry(format!("invalid registry: {e}")))?;
        reg.validate()?;
        Ok(reg)
    }

    fn validate(&self) -> Result<(), HubError> {
        if self.models.iter().filter(|m| m.default).count() != 1 {
            return Err(HubError::Registry(
                "exactly one default model is required".into(),
            ));
        }
        for m in &self.models {
            for role in [FileRole::Weights, FileRole::Tokenizer] {
                if m.files.iter().filter(|f| f.role == role).count() != 1 {
                    return Err(HubError::Registry(format!(
                        "model {} needs exactly one {role:?} file",
                        m.id
                    )));
                }
            }
            for f in &m.files {
                if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(HubError::Registry(format!("bad sha256 for {}", f.name)));
                }
                if f.sources.is_empty() {
                    return Err(HubError::Registry(format!("no sources for {}", f.name)));
                }
            }
        }
        Ok(())
    }

    pub fn default_model(&self) -> &ModelEntry {
        self.models
            .iter()
            .find(|m| m.default)
            .expect("validated: one default")
    }

    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Resolves `id` or the default model; accepts the id without the quantization suffix.
    pub fn lookup(&self, id: Option<&str>) -> Result<&ModelEntry, HubError> {
        match id {
            None => Ok(self.default_model()),
            Some(id) => self
                .get(id)
                .or_else(|| {
                    self.models
                        .iter()
                        .find(|m| m.id.split(':').next() == Some(id))
                })
                .ok_or_else(|| HubError::UnknownModel(id.to_string())),
        }
    }

    pub fn find_by_sha256(&self, sha: &str) -> Option<(&ModelEntry, &FileEntry)> {
        self.models.iter().find_map(|m| {
            m.files
                .iter()
                .find(|f| f.sha256.eq_ignore_ascii_case(sha))
                .map(|f| (m, f))
        })
    }
}

impl ModelEntry {
    pub fn file(&self, role: FileRole) -> &FileEntry {
        self.files
            .iter()
            .find(|f| f.role == role)
            .expect("validated: one file per role")
    }

    pub fn weights(&self) -> &FileEntry {
        self.file(FileRole::Weights)
    }

    pub fn tokenizer(&self) -> &FileEntry {
        self.file(FileRole::Tokenizer)
    }

    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Directory name for this model inside a model store (`:` is not portable).
    pub fn dir_name(&self) -> String {
        self.id.replace(':', "-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_is_valid() {
        let reg = Registry::builtin();
        assert_eq!(reg.schema, 1);
        let def = reg.default_model();
        assert_eq!(def.id, "minicpm5-2b:q4_k_m");
        assert_eq!(def.eog_ids, vec![1, 130073]);
        assert_eq!(def.weights().size, 1_561_318_368);
        assert_eq!(
            def.weights().sha256,
            "ec2d5801640099e97d8d7e8003ad4d81f336e757811f03a26173dddf386602fd"
        );
        assert_eq!(def.tokenizer().size, 9_894_271);
        assert_eq!(reg.models.len(), 3);
        assert_eq!(def.dir_name(), "minicpm5-2b-q4_k_m");
    }

    #[test]
    fn lookup_accepts_short_ids() {
        let reg = Registry::builtin();
        assert_eq!(
            reg.lookup(Some("minicpm5-1b")).unwrap().id,
            "minicpm5-1b:q4_k_m"
        );
        assert_eq!(
            reg.lookup(Some("minicpm5-2b:q8_0")).unwrap().id,
            "minicpm5-2b:q8_0"
        );
        assert!(reg.lookup(Some("nope")).is_err());
        assert_eq!(reg.lookup(None).unwrap().id, "minicpm5-2b:q4_k_m");
    }

    #[test]
    fn find_by_hash() {
        let reg = Registry::builtin();
        let (m, f) = reg
            .find_by_sha256("81B64D05A23B17B34C475F42B3E72FBDE62D4B92CC34541F7A8031D0752DEAFA")
            .unwrap();
        assert_eq!(m.id, "minicpm5-1b:q4_k_m");
        assert_eq!(f.role, FileRole::Weights);
    }

    #[test]
    fn rejects_invalid_registry() {
        let bad = r#"
schema = 1
[[model]]
id = "x"
display = "x"
arch = "llama"
chat_format = "minicpm5"
context_max = 1
eog_ids = [1]
license = "MIT"
sampling = { temperature = 1.0, top_p = 1.0, min_p = 0.0 }
files = []
"#;
        assert!(Registry::parse(bad).is_err());
    }
}
