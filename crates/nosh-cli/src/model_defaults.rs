//! Implicit model policy; explicit model IDs and paths always win.

use std::io;

use nosh_llm::InferenceDevice;
use nosh_llm::device::CudaMemorySnapshot;
use nosh_platform::{memory, tr};

const Q8: &str = "minicpm5-2b:q8_0";
const Q4: &str = "minicpm5-2b:q4_k_m";
const Q8_AVAILABLE_BYTES: u64 = 7 * 1024 * 1024 * 1024;
const Q4_AVAILABLE_BYTES: u64 = Q8_AVAILABLE_BYTES / 2;

/// Resolve once before loading; callers keep the chosen ID in their setup.
pub fn select(
    requested: Option<String>,
    has_path: bool,
    device: InferenceDevice,
) -> Result<Option<String>, String> {
    let (id, warning) = select_with(requested, has_path, memory::available_bytes, || {
        device.available_cuda_memory()
    });
    if let Some(warning) = warning {
        eprintln!("nosh: {warning}");
    }
    id
}

fn select_with(
    requested: Option<String>,
    has_path: bool,
    available: impl FnOnce() -> io::Result<u64>,
    cuda_memory: impl FnOnce() -> CudaMemorySnapshot,
) -> (Result<Option<String>, String>, Option<String>) {
    if requested.is_some() || has_path {
        return (Ok(requested), None);
    }
    let ram = available();
    if ram.as_ref().is_ok_and(|bytes| *bytes >= Q8_AVAILABLE_BYTES) {
        return (Ok(Some(Q8.into())), None);
    }
    let cuda = cuda_memory();
    let mut warnings = cuda.warnings;
    let ram = match ram {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            warnings.push(tr!(
                format!("无法读取可用内存：{error}"),
                format!("cannot read available RAM: {error}")
            ));
            None
        }
    };
    let id = match ram.into_iter().chain(cuda.free_bytes).max() {
        Some(bytes) if bytes >= Q8_AVAILABLE_BYTES => Q8,
        Some(bytes) if bytes >= Q4_AVAILABLE_BYTES => Q4,
        _ if !warnings.is_empty() => Q4,
        _ => return (Err(tr!(
            "可用内存和可用单卡显存均低于 3.5 GiB；不自动选择、安装或加载模型。推理可用 --model ID 或 --model-path PATH；模型管理命令请提供位置 ID，例如 nosh model pull ID。",
            "available RAM and usable single-GPU VRAM are both below 3.5 GiB; automatic model selection, installation and loading are disabled. For inference use --model ID or --model-path PATH; model commands require a positional ID, e.g. nosh model pull ID."
        ).into()), None),
    };
    let warning = (!warnings.is_empty()).then(|| tr!(
        format!("资源检测不完整（{}）；选择 {id}，可显式指定模型覆盖。", warnings.join("; ")),
        format!("resource observations are incomplete ({}); selecting {id}; specify a model explicitly to override", warnings.join("; "))
    ));
    (Ok(Some(id.into())), warning)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_use_available_ram_or_single_gpu_memory_without_adding_them() {
        for (ram, vram, expected) in [
            (0, 0, None),
            (Q4_AVAILABLE_BYTES - 1, Q4_AVAILABLE_BYTES - 1, None),
            (Q4_AVAILABLE_BYTES, 0, Some(Q4)),
            (0, Q4_AVAILABLE_BYTES, Some(Q4)),
            (Q4_AVAILABLE_BYTES + 1, 0, Some(Q4)),
            (2 << 30, 2 << 30, None),
            (Q8_AVAILABLE_BYTES - 1, 0, Some(Q4)),
            (0, Q8_AVAILABLE_BYTES - 1, Some(Q4)),
            (Q8_AVAILABLE_BYTES, 0, Some(Q8)),
            (0, Q8_AVAILABLE_BYTES, Some(Q8)),
            (Q8_AVAILABLE_BYTES + 1, 0, Some(Q8)),
            (0, Q8_AVAILABLE_BYTES + 1, Some(Q8)),
            (4 << 30, 4 << 30, Some(Q4)),
            (Q8_AVAILABLE_BYTES - 1, Q8_AVAILABLE_BYTES - 1, Some(Q4)),
        ] {
            let (id, warning) = select_with(
                None,
                false,
                || Ok(ram),
                || CudaMemorySnapshot {
                    free_bytes: Some(vram),
                    warnings: vec![],
                },
            );
            match expected {
                Some(expected) => assert_eq!(id.unwrap().as_deref(), Some(expected)),
                None => {
                    let reason = id.unwrap_err();
                    assert!(reason.contains("3.5 GiB"), "{reason}");
                    for hint in ["--model ID", "--model-path PATH", "nosh model pull ID"] {
                        assert!(reason.contains(hint), "{reason}");
                    }
                }
            }
            assert!(warning.is_none());
        }
    }

    #[test]
    fn explicit_ids_and_paths_never_probe_or_switch() {
        for requested in [Q8, Q4, "minicpm5-1b:q4_k_m", "no-such-model", ""] {
            for has_path in [false, true] {
                let (id, warning) = select_with(
                    Some(requested.into()),
                    has_path,
                    || panic!("must not probe RAM"),
                    || panic!("must not probe CUDA"),
                );
                assert_eq!(id.unwrap().as_deref(), Some(requested));
                assert!(warning.is_none());
            }
        }
        assert_eq!(
            select_with(
                None,
                true,
                || panic!("explicit path must not probe RAM"),
                || panic!("explicit path must not probe CUDA")
            ),
            (Ok(None), None)
        );
    }

    #[test]
    fn incomplete_observations_warn_and_use_known_resources_or_the_q4_fallback() {
        for (ram, vram, gpu_error, expected) in [
            (Err("fixture RAM failure"), None, None, Q4),
            (
                Err("fixture RAM failure"),
                Some(Q4_AVAILABLE_BYTES - 1),
                None,
                Q4,
            ),
            (
                Err("fixture RAM failure"),
                Some(Q8_AVAILABLE_BYTES),
                None,
                Q8,
            ),
            (Ok(0), None, Some("fixture GPU failure"), Q4),
            (
                Ok(0),
                Some(Q8_AVAILABLE_BYTES),
                Some("fixture GPU failure"),
                Q8,
            ),
        ] {
            let (id, warning) = select_with(
                None,
                false,
                || ram.map_err(io::Error::other),
                || CudaMemorySnapshot {
                    free_bytes: vram,
                    warnings: gpu_error.into_iter().map(str::to_string).collect(),
                },
            );
            assert_eq!(id.unwrap().as_deref(), Some(expected));
            let warning = warning.expect("probe failures must remain visible");
            assert!(warning.contains("fixture"), "{warning}");
            assert!(warning.contains(expected), "{warning}");
        }
    }

    #[test]
    fn enough_ram_never_initializes_cuda_for_model_choice() {
        assert_eq!(
            select_with(
                None,
                false,
                || Ok(Q8_AVAILABLE_BYTES),
                || panic!("RAM already qualifies")
            ),
            (Ok(Some(Q8.into())), None)
        );
    }

    #[test]
    fn policy_matches_the_builtin_registry() {
        let registry = nosh_hub::Registry::builtin();
        assert_eq!(registry.default_model().id, Q8);
        assert!(registry.get(Q4).is_some());
    }
}
