//! Implicit model policy; explicit model IDs and paths always win.

use std::io;

use nosh_llm::InferenceDevice;
use nosh_llm::device::CudaMemorySnapshot;
use nosh_platform::{memory, tr};

const Q8: &str = "minicpm5-2b:q8_0";
const Q4: &str = "minicpm5-2b:q4_k_m";
const SMALL: &str = "qwen3.5-0.8b:q8_0";
const Q8_AVAILABLE_BYTES: u64 = 7 * 1024 * 1024 * 1024;
const Q4_AVAILABLE_BYTES: u64 = Q8_AVAILABLE_BYTES / 2;
const MIN_AVAILABLE_BYTES: u64 = 3 * 1024 * 1024 * 1024 / 2;
const MIN_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Resolve once before loading; callers keep the chosen ID in their setup.
pub fn select(
    requested: Option<String>,
    has_path: bool,
    device: InferenceDevice,
) -> Result<Option<String>, String> {
    select_with(
        requested,
        has_path,
        memory::available_bytes,
        memory::total_bytes,
        || device.available_cuda_memory(),
    )
}

fn observation_error(detail: impl std::fmt::Display) -> String {
    tr!(
        format!(
            "资源检测不完整（{detail}）；暂停自动安装和加载。可用 --model ID、--model-path PATH 或 nosh model pull ID 显式尝试。"
        ),
        format!(
            "resource observations are incomplete ({detail}); automatic installation and loading are disabled. Explicit attempts remain available via --model ID, --model-path PATH or nosh model pull ID."
        )
    )
}

fn select_with(
    requested: Option<String>,
    has_path: bool,
    available: impl FnOnce() -> io::Result<u64>,
    total: impl FnOnce() -> io::Result<u64>,
    cuda_memory: impl FnOnce() -> CudaMemorySnapshot,
) -> Result<Option<String>, String> {
    if requested.is_some() || has_path {
        return Ok(requested);
    }
    let ram = available().map_err(|error| observation_error(format!("available RAM: {error}")))?;
    if ram >= Q8_AVAILABLE_BYTES {
        return Ok(Some(Q8.into()));
    }
    let cuda = cuda_memory();
    if !cuda.warnings.is_empty() {
        return Err(observation_error(cuda.warnings.join("; ")));
    }
    // No eligible device (CPU request/build, or no visible GPU) is not a failed observation.
    let available = ram.max(cuda.free_bytes.unwrap_or(0));
    let id = match available {
        bytes if bytes >= Q8_AVAILABLE_BYTES => Q8,
        bytes if bytes >= Q4_AVAILABLE_BYTES => Q4,
        bytes if bytes >= MIN_AVAILABLE_BYTES => SMALL,
        _ => {
            let total =
                total().map_err(|error| observation_error(format!("total RAM: {error}")))?;
            if total == 0 || total < ram {
                return Err(observation_error(
                    "inconsistent total/available physical RAM",
                ));
            }
            if total < MIN_TOTAL_BYTES {
                return Err(tr!(
                    "可用内存和可用单卡显存均低于 1.5 GiB，且总内存低于 4 GiB；不建议安装或加载，已暂停自动安装和加载。可用 --model ID、--model-path PATH 或 nosh model pull ID 显式尝试。",
                    "available RAM and usable single-GPU VRAM are both below 1.5 GiB and total RAM is below 4 GiB; installation/loading is not recommended and automatic installation/loading are disabled. Explicit attempts remain available via --model ID, --model-path PATH or nosh model pull ID."
                ).into());
            }
            SMALL
        }
    };
    Ok(Some(id.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_use_available_ram_or_single_gpu_memory_without_adding_them() {
        for (ram, vram, expected) in [
            (0, 0, SMALL),
            (Q4_AVAILABLE_BYTES - 1, Q4_AVAILABLE_BYTES - 1, SMALL),
            (Q4_AVAILABLE_BYTES, 0, Q4),
            (0, Q4_AVAILABLE_BYTES, Q4),
            (Q4_AVAILABLE_BYTES + 1, 0, Q4),
            (2 << 30, 2 << 30, SMALL),
            (Q8_AVAILABLE_BYTES - 1, 0, Q4),
            (0, Q8_AVAILABLE_BYTES - 1, Q4),
            (Q8_AVAILABLE_BYTES, 0, Q8),
            (0, Q8_AVAILABLE_BYTES, Q8),
            (Q8_AVAILABLE_BYTES + 1, 0, Q8),
            (0, Q8_AVAILABLE_BYTES + 1, Q8),
            (4 << 30, 4 << 30, Q4),
            (Q8_AVAILABLE_BYTES - 1, Q8_AVAILABLE_BYTES - 1, Q4),
        ] {
            let id = select_with(
                None,
                false,
                || Ok(ram),
                || Ok(MIN_TOTAL_BYTES),
                || CudaMemorySnapshot {
                    free_bytes: Some(vram),
                    warnings: vec![],
                },
            );
            assert_eq!(id.unwrap().as_deref(), Some(expected));
        }
    }

    #[test]
    fn explicit_ids_and_paths_never_probe_or_switch() {
        for requested in [Q8, Q4, SMALL, "minicpm5-1b:q4_k_m", "no-such-model", ""] {
            for has_path in [false, true] {
                let id = select_with(
                    Some(requested.into()),
                    has_path,
                    || panic!("must not probe RAM"),
                    || panic!("must not probe total RAM"),
                    || panic!("must not probe CUDA"),
                );
                assert_eq!(id.unwrap().as_deref(), Some(requested));
            }
        }
        assert_eq!(
            select_with(
                None,
                true,
                || panic!("explicit path must not probe RAM"),
                || panic!("explicit path must not probe total RAM"),
                || panic!("explicit path must not probe CUDA")
            ),
            Ok(None)
        );
    }

    #[test]
    fn incomplete_observations_pause_instead_of_using_a_success_shaped_fallback() {
        for (ram, vram, gpu_error) in [
            (Err("fixture RAM failure"), None, None),
            (
                Err("fixture RAM failure"),
                Some(Q4_AVAILABLE_BYTES - 1),
                None,
            ),
            (Err("fixture RAM failure"), Some(Q8_AVAILABLE_BYTES), None),
            (Ok(0), None, Some("fixture GPU failure")),
            (Ok(0), Some(Q8_AVAILABLE_BYTES), Some("fixture GPU failure")),
        ] {
            let id = select_with(
                None,
                false,
                || ram.map_err(io::Error::other),
                || panic!("incomplete observations must not query total RAM"),
                || CudaMemorySnapshot {
                    free_bytes: vram,
                    warnings: gpu_error.into_iter().map(str::to_string).collect(),
                },
            );
            let error = id.unwrap_err();
            assert!(error.contains("fixture"), "{error}");
            assert!(error.contains("--model ID"), "{error}");
        }
    }

    #[test]
    fn enough_ram_never_initializes_cuda_for_model_choice() {
        assert_eq!(
            select_with(
                None,
                false,
                || Ok(Q8_AVAILABLE_BYTES),
                || panic!("RAM already qualifies"),
                || panic!("RAM already qualifies")
            ),
            Ok(Some(Q8.into()))
        );
    }

    #[test]
    fn policy_matches_the_builtin_registry() {
        let registry = nosh_hub::Registry::builtin();
        assert_eq!(registry.default_model().id, Q8);
        assert!(registry.get(Q4).is_some());
        assert!(registry.get(SMALL).is_some());
    }

    #[test]
    fn low_resource_pause_uses_three_strict_conditions_with_exact_boundaries() {
        for (ram, vram, total, paused) in [
            (
                MIN_AVAILABLE_BYTES - 1,
                MIN_AVAILABLE_BYTES - 1,
                MIN_TOTAL_BYTES - 1,
                true,
            ),
            (MIN_AVAILABLE_BYTES, 0, MIN_TOTAL_BYTES - 1, false),
            (0, MIN_AVAILABLE_BYTES, MIN_TOTAL_BYTES - 1, false),
            (MIN_AVAILABLE_BYTES + 1, 0, MIN_TOTAL_BYTES - 1, false),
            (MIN_AVAILABLE_BYTES - 1, 0, MIN_TOTAL_BYTES, false),
            (0, 0, MIN_TOTAL_BYTES + 1, false),
        ] {
            let result = select_with(
                None,
                false,
                || Ok(ram),
                || Ok(total),
                || CudaMemorySnapshot {
                    free_bytes: Some(vram),
                    warnings: vec![],
                },
            );
            if paused {
                let reason = result.unwrap_err();
                assert!(
                    reason.contains("1.5 GiB") && reason.contains("4 GiB"),
                    "{reason}"
                );
            } else {
                assert_eq!(result.unwrap().as_deref(), Some(SMALL));
            }
        }
    }

    #[test]
    fn total_ram_is_required_only_when_both_available_resources_are_low() {
        for (ram, vram) in [(MIN_AVAILABLE_BYTES, 0), (0, MIN_AVAILABLE_BYTES)] {
            assert_eq!(
                select_with(
                    None,
                    false,
                    || Ok(ram),
                    || panic!("total is irrelevant"),
                    || CudaMemorySnapshot {
                        free_bytes: Some(vram),
                        warnings: vec![],
                    }
                )
                .unwrap()
                .as_deref(),
                Some(SMALL)
            );
        }
        let error = select_with(
            None,
            false,
            || Ok(0),
            || Err(io::Error::other("fixture total failure")),
            CudaMemorySnapshot::default,
        )
        .unwrap_err();
        assert!(error.contains("fixture total failure"), "{error}");
    }
}
