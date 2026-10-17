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
    let observed = ram
        .as_ref()
        .ok()
        .copied()
        .unwrap_or(0)
        .max(cuda.free_bytes.unwrap_or(0));
    let uncertain = ram.is_err() || !cuda.warnings.is_empty();
    if observed < Q4_AVAILABLE_BYTES && !uncertain {
        return (
            Err(tr!(
                "可用内存和可用单卡显存均低于 3.5 GiB；不自动选择、安装或加载模型。可用 --model ID 或 --model-path PATH 显式覆盖。",
                "available RAM and usable single-GPU VRAM are both below 3.5 GiB; automatic model selection, installation and loading are disabled. Use --model ID or --model-path PATH to override."
            ).into()),
            None,
        );
    }
    let id = if observed >= Q8_AVAILABLE_BYTES {
        Q8
    } else {
        Q4
    };
    let mut warnings = cuda.warnings;
    if let Err(error) = ram {
        warnings.push(tr!(
            format!("无法读取可用内存：{error}"),
            format!("cannot read available RAM: {error}")
        ));
    }
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
    fn available_ram_threshold_is_exactly_seven_gib() {
        for (bytes, expected) in [
            (Q8_AVAILABLE_BYTES - 1, Q4),
            (Q8_AVAILABLE_BYTES, Q8),
            (Q8_AVAILABLE_BYTES + 1, Q8),
            (32 * 1024 * 1024 * 1024, Q8),
        ] {
            let (id, warning) = select_with(None, false, || Ok(bytes), CudaMemorySnapshot::default);
            assert_eq!(id.unwrap().as_deref(), Some(expected), "{bytes}");
            assert!(warning.is_none());
        }
    }

    #[test]
    fn both_resources_below_three_and_a_half_gib_disable_automatic_models() {
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
                    assert!(reason.contains("--model"), "{reason}");
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
    fn observation_errors_choose_q4_with_an_explicit_warning() {
        let (id, warning) = select_with(
            None,
            false,
            || Err(io::Error::other("fixture RAM failure")),
            CudaMemorySnapshot::default,
        );
        assert_eq!(id.unwrap().as_deref(), Some(Q4));
        let warning = warning.expect("fallback must be visible");
        assert!(warning.contains("fixture RAM failure"), "{warning}");
        assert!(warning.contains(Q4), "{warning}");
    }

    #[test]
    fn sufficient_vram_alone_selects_q8_without_adding_ram() {
        for (ram, vram, expected) in [
            (0, Q8_AVAILABLE_BYTES, Q8),
            (Q8_AVAILABLE_BYTES - 1, Q8_AVAILABLE_BYTES - 1, Q4),
            (4 * 1024 * 1024 * 1024, 4 * 1024 * 1024 * 1024, Q4),
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
            assert_eq!(id.unwrap().as_deref(), Some(expected));
            assert!(warning.is_none());
        }
        let (id, warning) = select_with(
            None,
            false,
            || Err(io::Error::other("RAM unavailable")),
            || CudaMemorySnapshot {
                free_bytes: Some(Q8_AVAILABLE_BYTES),
                warnings: vec![],
            },
        );
        assert_eq!(id.unwrap().as_deref(), Some(Q8));
        assert!(warning.unwrap().contains("RAM unavailable"));
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
    fn cuda_probe_errors_remain_visible() {
        let (id, warning) = select_with(
            None,
            false,
            || Ok(0),
            || CudaMemorySnapshot {
                free_bytes: None,
                warnings: vec!["fixture GPU failure".into()],
            },
        );
        assert_eq!(id.unwrap().as_deref(), Some(Q4));
        assert!(warning.unwrap().contains("fixture GPU failure"));
    }

    #[test]
    fn an_unknown_resource_does_not_falsely_prove_both_are_below_the_low_threshold() {
        let (id, warning) = select_with(
            None,
            false,
            || Err(io::Error::other("RAM unavailable")),
            || CudaMemorySnapshot {
                free_bytes: Some(Q4_AVAILABLE_BYTES - 1),
                warnings: vec![],
            },
        );
        assert_eq!(id.unwrap().as_deref(), Some(Q4));
        assert!(warning.unwrap().contains("RAM unavailable"));

        let (id, warning) = select_with(
            None,
            false,
            || Ok(0),
            || CudaMemorySnapshot {
                free_bytes: Some(Q8_AVAILABLE_BYTES),
                warnings: vec!["another GPU could not be read".into()],
            },
        );
        assert_eq!(id.unwrap().as_deref(), Some(Q8));
        assert!(warning.unwrap().contains("another GPU"));
    }

    #[test]
    fn policy_matches_the_builtin_registry() {
        let registry = nosh_hub::Registry::builtin();
        assert_eq!(registry.default_model().id, Q8);
        assert!(registry.get(Q4).is_some());
    }
}
