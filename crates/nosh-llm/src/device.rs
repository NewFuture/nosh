//! Explicit inference device selection. `auto` retains the historical CPU default.

use candle_core::Device;

use crate::LlmError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InferenceDevice {
    #[default]
    Cpu,
    Cuda(usize),
}

impl std::str::FromStr for InferenceDevice {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" | "cpu" => Ok(Self::Cpu),
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
            Self::Cpu => f.write_str("cpu"),
            Self::Cuda(index) => write!(f, "cuda:{index}"),
        }
    }
}

impl InferenceDevice {
    /// Never falls back to CPU when CUDA was explicitly requested.
    pub fn open(self) -> Result<Device, LlmError> {
        match self {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_device_syntax_and_cpu_default() {
        assert_eq!(InferenceDevice::default(), InferenceDevice::Cpu);
        for value in ["cpu", "auto"] {
            assert_eq!(value.parse(), Ok(InferenceDevice::Cpu));
        }
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
    #[cfg(not(feature = "cuda"))]
    fn cuda_without_feature_is_an_error_not_cpu() {
        let error = InferenceDevice::Cuda(0).open().unwrap_err().to_string();
        assert!(error.contains("--features cuda"), "{error}");
    }
}
