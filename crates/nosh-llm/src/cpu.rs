//! CPU features that decide which of candle's quantized kernels run.

/// The features candle checks at runtime when it picks a kernel (x86_64:
/// AVX2/AVX-512/VNNI tiles; aarch64: dotprod gemv and i8mm tiles), detected
/// the same way. Empty on other architectures.
pub fn features() -> Vec<&'static str> {
    detect()
}

#[cfg(target_arch = "x86_64")]
fn detect() -> Vec<&'static str> {
    let mut f = Vec::new();
    macro_rules! feat {
        ($($name:tt),*) => {$(
            if std::arch::is_x86_feature_detected!($name) {
                f.push($name);
            }
        )*};
    }
    feat!(
        "avx",
        "avx2",
        "fma",
        "f16c",
        "avx512f",
        "avx512bw",
        "avx512vl",
        "avx512vnni",
        "avx512bf16",
        "avxvnni"
    );
    f
}

#[cfg(target_arch = "aarch64")]
fn detect() -> Vec<&'static str> {
    let mut f = vec!["neon"];
    macro_rules! feat {
        ($($name:tt),*) => {$(
            if std::arch::is_aarch64_feature_detected!($name) {
                f.push($name);
            }
        )*};
    }
    feat!("dotprod", "i8mm", "fp16", "bf16");
    f
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn detect() -> Vec<&'static str> {
    Vec::new()
}
