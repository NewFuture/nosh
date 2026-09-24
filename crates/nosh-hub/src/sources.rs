//! Download sources (HF, hf-mirror, ModelScope) and local-only region inference.

use std::fmt;

use crate::registry::FileEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hub {
    HuggingFace,
    HfMirror,
    ModelScope,
}

impl Hub {
    pub const ALL: [Hub; 3] = [Hub::HuggingFace, Hub::HfMirror, Hub::ModelScope];

    pub fn name(self) -> &'static str {
        match self {
            Hub::HuggingFace => "huggingface.co",
            Hub::HfMirror => "hf-mirror.com",
            Hub::ModelScope => "modelscope.cn",
        }
    }

    /// Accepts config / CLI spellings such as `hf`, `hf-mirror`, `modelscope`.
    pub fn parse(s: &str) -> Option<Hub> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hf" | "huggingface" | "huggingface.co" => Some(Hub::HuggingFace),
            "hf-mirror" | "hfmirror" | "hf-mirror.com" | "mirror" => Some(Hub::HfMirror),
            "modelscope" | "ms" | "modelscope.cn" => Some(Hub::ModelScope),
            _ => None,
        }
    }
}

impl fmt::Display for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A concrete URL for one file on one hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub hub: Hub,
    pub url: String,
    pub revision: String,
}

/// Endpoint overrides (`NOSH_ENDPOINT`, compatible with `HF_ENDPOINT`).
#[derive(Debug, Clone, Default)]
pub struct Endpoints {
    pub hf: Option<String>,
}

impl Endpoints {
    pub fn from_env() -> Self {
        let hf = std::env::var("NOSH_ENDPOINT")
            .ok()
            .or_else(|| std::env::var("HF_ENDPOINT").ok())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().trim_end_matches('/').to_string());
        Self { hf }
    }
}

/// Builds the candidate URLs for a file, one per usable hub.
pub fn candidates(file: &FileEntry, endpoints: &Endpoints) -> Vec<Candidate> {
    let mut out = Vec::new();
    for src in &file.sources {
        match src.hub.as_str() {
            "hf" => {
                let base = endpoints
                    .hf
                    .clone()
                    .unwrap_or_else(|| "https://huggingface.co".to_string());
                out.push(Candidate {
                    hub: Hub::HuggingFace,
                    url: format!("{base}/{}/resolve/{}/{}", src.repo, src.revision, file.name),
                    revision: src.revision.clone(),
                });
                if endpoints.hf.is_none() {
                    out.push(Candidate {
                        hub: Hub::HfMirror,
                        url: format!(
                            "https://hf-mirror.com/{}/resolve/{}/{}",
                            src.repo, src.revision, file.name
                        ),
                        revision: src.revision.clone(),
                    });
                }
            }
            "modelscope" => out.push(Candidate {
                hub: Hub::ModelScope,
                url: format!(
                    "https://www.modelscope.cn/models/{}/resolve/{}/{}",
                    src.repo, src.revision, file.name
                ),
                revision: src.revision.clone(),
            }),
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    MainlandChina,
    Global,
}

impl Region {
    /// Preferred hub order before any speed test.
    pub fn hub_order(self) -> [Hub; 3] {
        match self {
            Region::MainlandChina => [Hub::ModelScope, Hub::HfMirror, Hub::HuggingFace],
            Region::Global => [Hub::HuggingFace, Hub::ModelScope, Hub::HfMirror],
        }
    }
}

const CN_TIMEZONES: &[&str] = &[
    "Asia/Shanghai",
    "Asia/Chongqing",
    "Asia/Chungking",
    "Asia/Harbin",
    "Asia/Urumqi",
    "Asia/Kashgar",
    "PRC",
];

/// Infers the region from locale and time zone only (no IP geolocation).
pub fn infer_region(locale: Option<&str>, timezone: Option<&str>) -> Region {
    let cn_locale = locale.is_some_and(|l| {
        let l = l.to_ascii_lowercase().replace('-', "_");
        l.starts_with("zh_cn") || l.starts_with("zh_hans_cn") || l == "zh_sg.utf-8"
    });
    let cn_tz = timezone.is_some_and(|tz| {
        let tz = tz.trim().trim_start_matches(':');
        CN_TIMEZONES
            .iter()
            .any(|c| tz == *c || tz.ends_with(&format!("/{c}")))
    });
    if cn_locale || cn_tz {
        Region::MainlandChina
    } else {
        Region::Global
    }
}

/// Region of the current machine (`NOSH_REGION=cn|global` overrides).
pub fn current_region() -> Region {
    if let Ok(r) = std::env::var("NOSH_REGION") {
        match r.to_ascii_lowercase().as_str() {
            "cn" | "china" => return Region::MainlandChina,
            "global" | "intl" => return Region::Global,
            _ => {}
        }
    }
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
    infer_region(locale.as_deref(), system_timezone().as_deref())
}

fn system_timezone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ")
        && !tz.is_empty()
    {
        return Some(tz);
    }
    if let Ok(s) = std::fs::read_to_string("/etc/timezone") {
        let s = s.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    std::fs::read_link("/etc/localtime")
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;

    #[test]
    fn region_inference() {
        assert_eq!(
            infer_region(Some("zh_CN.UTF-8"), None),
            Region::MainlandChina
        );
        assert_eq!(
            infer_region(Some("en_US.UTF-8"), Some("Asia/Shanghai")),
            Region::MainlandChina
        );
        assert_eq!(
            infer_region(None, Some("/usr/share/zoneinfo/Asia/Shanghai")),
            Region::MainlandChina
        );
        assert_eq!(
            infer_region(Some("en_US.UTF-8"), Some("America/Los_Angeles")),
            Region::Global
        );
        assert_eq!(
            infer_region(Some("zh_TW.UTF-8"), Some("Asia/Taipei")),
            Region::Global
        );
        assert_eq!(infer_region(None, None), Region::Global);
    }

    #[test]
    fn candidate_urls() {
        let reg = Registry::builtin();
        let w = reg.default_model().weights();
        let c = candidates(w, &Endpoints::default());
        assert_eq!(c.len(), 3);
        assert_eq!(
            c[0].url,
            "https://huggingface.co/openbmb/MiniCPM5-2B-GGUF/resolve/2079a22f3beaa4e306449978533478fe0522f4b3/MiniCPM5-2B-Q4_K_M.gguf"
        );
        assert_eq!(c[1].hub, Hub::HfMirror);
        assert_eq!(
            c[2].url,
            "https://www.modelscope.cn/models/OpenBMB/MiniCPM5-2B-GGUF/resolve/master/MiniCPM5-2B-Q4_K_M.gguf"
        );
        let custom = Endpoints {
            hf: Some("http://127.0.0.1:8080".into()),
        };
        let c = candidates(w, &custom);
        assert_eq!(c.len(), 2);
        assert!(c[0].url.starts_with("http://127.0.0.1:8080/openbmb/"));
    }

    #[test]
    fn hub_parse() {
        assert_eq!(Hub::parse("HF"), Some(Hub::HuggingFace));
        assert_eq!(Hub::parse("hf-mirror"), Some(Hub::HfMirror));
        assert_eq!(Hub::parse("modelscope"), Some(Hub::ModelScope));
        assert_eq!(Hub::parse("auto"), None);
    }
}
