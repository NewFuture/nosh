//! Model-aware device selection. Only automatic requests may choose CPU as a fallback.

use candle_core::Device;
use std::path::Path;

use crate::LlmError;
use crate::model::attn::KvDtype;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InferenceDevice {
    #[default]
    Auto,
    Cpu,
    Cuda(usize),
}

impl std::str::FromStr for InferenceDevice {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "cpu" => Ok(Self::Cpu),
            "cuda" => Ok(Self::Cuda(0)),
            _ => value
                .strip_prefix("cuda:")
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse().ok())
                .filter(|&index| index <= i32::MAX as usize)
                .map(Self::Cuda)
                .ok_or_else(|| {
                    format!("invalid device {value:?}: expected cpu, auto, cuda or cuda:N")
                }),
        }
    }
}

impl std::fmt::Display for InferenceDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Cpu => f.write_str("cpu"),
            Self::Cuda(index) => write!(f, "cuda:{index}"),
        }
    }
}

impl InferenceDevice {
    /// Opens an explicit device; automatic selection needs the model's memory budget.
    pub fn open(self) -> Result<Device, LlmError> {
        match self {
            Self::Auto => Err(LlmError::Config(
                "auto device selection requires model/context information; use select".into(),
            )),
            Self::Cpu => Ok(Device::Cpu),
            Self::Cuda(index) => {
                if index > i32::MAX as usize {
                    return Err(LlmError::Config(format!(
                        "CUDA device index out of range: {index}"
                    )));
                }
                if !cfg!(feature = "cuda") {
                    return Err(LlmError::Config(
                        "CUDA is not enabled in this build; rebuild with --features cuda or select cpu".into(),
                    ));
                }
                Device::new_cuda(index).map_err(|error| {
                    LlmError::Config(format!(
                        "cannot initialize {self}: {error}; check the NVIDIA driver and CUDA_VISIBLE_DEVICES"
                    ))
                })
            }
        }
    }

    /// Chooses once, before loading weights. Errors reading the model are not fallbacks.
    pub fn select(
        self,
        weights: &Path,
        context_length: usize,
        prefill_chunk: usize,
        kv_dtype: KvDtype,
    ) -> Result<(Device, DeviceSelection), LlmError> {
        if self != Self::Auto {
            if matches!(self, Self::Cuda(_)) {
                crate::model::llama::LoadOptions {
                    kv_dtype,
                    ..Default::default()
                }
                .validate_cuda()?;
            }
            return Ok((
                self.open()?,
                DeviceSelection {
                    requested: self,
                    actual: self,
                    reason: "explicit device selection".into(),
                    required_cuda_bytes: None,
                    free_cuda_bytes: None,
                },
            ));
        }
        if kv_dtype != KvDtype::F16 {
            return Ok(cpu_selection("f32 KV is supported on CPU only", None));
        }
        #[cfg(not(feature = "cuda"))]
        {
            let _ = (weights, context_length, prefill_chunk);
            Ok(cpu_selection("CUDA support is not compiled in", None))
        }
        #[cfg(feature = "cuda")]
        {
            let required =
                crate::model::llama::cuda_memory_estimate(weights, context_length, prefill_chunk)?;
            select_cuda(required)
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceSelection {
    pub requested: InferenceDevice,
    pub actual: InferenceDevice,
    pub reason: String,
    pub required_cuda_bytes: Option<u64>,
    pub free_cuda_bytes: Option<u64>,
}

fn cpu_selection(reason: impl Into<String>, required: Option<u64>) -> (Device, DeviceSelection) {
    (
        Device::Cpu,
        DeviceSelection {
            requested: InferenceDevice::Auto,
            actual: InferenceDevice::Cpu,
            reason: reason.into(),
            required_cuda_bytes: required,
            free_cuda_bytes: None,
        },
    )
}

#[cfg(any(feature = "cuda", test))]
#[derive(Debug)]
struct CudaMemory {
    ordinal: usize,
    free: u64,
    name: String,
}

#[cfg(any(feature = "cuda", test))]
fn best_fit(devices: &[CudaMemory], required: u64) -> Option<usize> {
    devices
        .iter()
        .enumerate()
        .filter(|(_, d)| d.free >= required)
        .max_by(|(_, a), (_, b)| a.free.cmp(&b.free).then_with(|| b.ordinal.cmp(&a.ordinal)))
        .map(|(index, _)| index)
}

#[cfg(feature = "cuda")]
fn select_cuda(required: u64) -> Result<(Device, DeviceSelection), LlmError> {
    use candle_core::cuda_backend::cudarc::driver::CudaContext;

    let count = match CudaContext::device_count() {
        Ok(count) if count > 0 => count,
        Ok(_) => return Ok(cpu_selection("no visible CUDA devices", Some(required))),
        Err(error) => {
            return Ok(cpu_selection(
                format!("CUDA detection unavailable: {error}"),
                Some(required),
            ));
        }
    };
    let mut devices = Vec::new();
    let mut diagnostics = Vec::new();
    for ordinal in 0..count as usize {
        let probe = CudaContext::new(ordinal).and_then(|context| {
            Ok(CudaMemory {
                ordinal,
                free: context.mem_get_info()?.0 as u64,
                name: context.name()?,
            })
        });
        match probe {
            Ok(device) => {
                diagnostics.push(format!(
                    "cuda:{ordinal}: {} MiB free",
                    device.free / (1 << 20)
                ));
                devices.push(device);
            }
            Err(error) => diagnostics.push(format!("cuda:{ordinal}: probe failed: {error}")),
        }
    }
    while let Some(index) = best_fit(&devices, required) {
        let candidate = devices.remove(index);
        let selection = InferenceDevice::Cuda(candidate.ordinal);
        let device = match selection.open() {
            Ok(device) => device,
            Err(error) => {
                diagnostics.push(error.to_string());
                continue;
            }
        };
        // Recheck after Candle initializes its CUDA libraries; never treat the probe as a reservation.
        let free = match device
            .as_cuda_device()?
            .cuda_stream()
            .context()
            .mem_get_info()
        {
            Ok((free, _)) => free as u64,
            Err(error) => {
                diagnostics.push(format!("{selection}: memory recheck failed: {error}"));
                continue;
            }
        };
        if free < required {
            diagnostics.push(format!(
                "{selection}: memory changed to {} MiB free",
                free / (1 << 20)
            ));
            continue;
        }
        return Ok((
            device,
            DeviceSelection {
                requested: InferenceDevice::Auto,
                actual: selection,
                reason: format!(
                    "{}: {} MiB free >= {} MiB estimated requirement; most free memory among eligible devices",
                    candidate.name,
                    free / (1 << 20),
                    required.div_ceil(1 << 20),
                ),
                required_cuda_bytes: Some(required),
                free_cuda_bytes: Some(free),
            },
        ));
    }
    Ok(cpu_selection(
        format!(
            "no usable CUDA device with {} MiB estimated free-memory requirement ({})",
            required.div_ceil(1 << 20),
            diagnostics.join("; "),
        ),
        Some(required),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_device_syntax_and_auto_default() {
        assert_eq!(InferenceDevice::default(), InferenceDevice::Auto);
        assert_eq!("auto".parse(), Ok(InferenceDevice::Auto));
        assert_eq!("cpu".parse(), Ok(InferenceDevice::Cpu));
        assert_eq!("cuda".parse(), Ok(InferenceDevice::Cuda(0)));
        assert_eq!("cuda:2".parse(), Ok(InferenceDevice::Cuda(2)));
        for value in [
            "",
            "metal",
            "CUDA",
            "cuda:",
            "cuda:-1",
            "cuda:+1",
            "cuda:x",
            "cuda:2147483648",
        ] {
            assert!(value.parse::<InferenceDevice>().is_err(), "{value}");
        }
    }

    #[test]
    fn automatic_f32_uses_cpu_without_cuda_or_model_io() {
        let (device, selection) = InferenceDevice::Auto
            .select(Path::new("missing.gguf"), 8192, 512, KvDtype::F32)
            .unwrap();
        assert!(device.is_cpu());
        assert_eq!(selection.actual, InferenceDevice::Cpu);
        assert!(selection.reason.contains("f32 KV"));
    }

    #[test]
    fn selects_most_free_fitting_device_with_stable_ties() {
        let devices = [
            CudaMemory {
                ordinal: 0,
                free: 3999,
                name: "busy".into(),
            },
            CudaMemory {
                ordinal: 2,
                free: 8000,
                name: "free".into(),
            },
            CudaMemory {
                ordinal: 1,
                free: 8000,
                name: "free".into(),
            },
        ];
        assert_eq!(best_fit(&devices, 8001), None);
        assert_eq!(best_fit(&devices, 8000), Some(2));
        assert_eq!(best_fit(&devices[..1], 4000), None);
        assert_eq!(best_fit(&devices[..1], 3999), Some(0));
        assert_eq!(best_fit(&[], 0), None);
        assert_eq!(devices[2].name, "free");
    }

    #[test]
    #[cfg(not(feature = "cuda"))]
    fn auto_in_cpu_build_needs_neither_model_nor_cuda() {
        let (device, selection) = InferenceDevice::Auto
            .select(Path::new("missing.gguf"), 8192, 512, KvDtype::F16)
            .unwrap();
        assert!(device.is_cpu());
        assert_eq!(selection.requested, InferenceDevice::Auto);
        assert!(selection.reason.contains("not compiled"));
    }

    #[test]
    #[cfg(feature = "cuda")]
    #[ignore = "requires an NVIDIA GPU"]
    fn auto_rejects_a_budget_larger_than_available_vram() {
        let (device, selection) = select_cuda(u64::MAX).unwrap();
        assert!(device.is_cpu());
        assert_eq!(selection.actual, InferenceDevice::Cpu);
        assert_eq!(selection.required_cuda_bytes, Some(u64::MAX));
        assert!(
            selection
                .reason
                .contains("estimated free-memory requirement"),
            "{}",
            selection.reason
        );
    }

    #[test]
    #[cfg(not(feature = "cuda"))]
    fn cuda_without_feature_is_an_error_not_cpu() {
        let error = InferenceDevice::Cuda(0).open().unwrap_err().to_string();
        assert!(error.contains("--features cuda"), "{error}");
    }
}
