//! Guards for the vendored candle-core (third_party/candle-core): the nosh
//! patch keeps its fixes when the directory is refreshed from upstream.

use std::path::Path;

fn vendored(rel: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/candle-core");
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn backend_stubs_report_their_own_backend() {
    // Upstream's Metal stub answered one call with the CUDA error.
    let metal = vendored("src/quantized/dummy_metal.rs");
    assert!(
        !metal.contains("Err(Error::NotCompiledWithCudaSupport)"),
        "dummy_metal.rs"
    );
    assert!(metal.contains("Err(Error::NotCompiledWithMetalSupport)"));
    let cuda = vendored("src/quantized/dummy_cuda.rs");
    assert!(
        !cuda.contains("Err(Error::NotCompiledWithMetalSupport)"),
        "dummy_cuda.rs"
    );
}

#[test]
fn cuda_kernels_keep_the_same_git_revision_as_core() {
    let revision = "5ba5d5b468b5b1df40e82dd3d556987bedeea041";
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let workspace = std::fs::read_to_string(root).unwrap();
    for dependency in ["candle-core", "candle-nn"] {
        let line = workspace
            .lines()
            .find(|line| line.starts_with(&format!("{dependency} = {{ git =")))
            .unwrap();
        assert!(line.contains(&format!("rev = \"{revision}\"")));
    }
    let manifest = vendored("Cargo.toml");
    let kernels = manifest
        .split_once("[dependencies.candle-kernels]\n")
        .unwrap()
        .1
        .split("\n[")
        .next()
        .unwrap();
    assert!(kernels.contains("git = \"https://github.com/huggingface/candle\""));
    assert!(kernels.contains(&format!("rev = \"{revision}\"")));
    assert!(kernels.contains("optional = true"));
}

#[test]
#[cfg(feature = "cuda")]
#[ignore = "requires a real NVIDIA CUDA device"]
fn cuda_small_sums_match_cpu_for_contiguous_and_strided_inputs() -> candle_core::Result<()> {
    use candle_core::{DType, Device, Tensor};

    let device = Device::new_cuda(0)?;
    for dtype in [DType::F32, DType::F16] {
        for width in [1, 4, 8, 16, 32, 33, 128] {
            let values: Vec<_> = (0..3 * width * 2)
                .map(|i| ((i % 17) as f32 - 8.0) / 4.0)
                .collect();
            let cpu = Tensor::from_vec(values, (3, width, 2), &Device::Cpu)?.to_dtype(dtype)?;
            let gpu = cpu.to_device(&device)?;
            for contiguous in [false, true] {
                let (cpu, gpu, axis) = if contiguous {
                    (
                        cpu.transpose(1, 2)?.contiguous()?,
                        gpu.transpose(1, 2)?.contiguous()?,
                        2,
                    )
                } else {
                    (cpu.clone(), gpu.clone(), 1)
                };
                let result = |input: &Tensor| {
                    input
                        .sum_keepdim(axis)?
                        .to_dtype(DType::F32)?
                        .flatten_all()?
                        .to_vec1::<f32>()
                };
                assert_eq!(
                    result(&cpu)?,
                    result(&gpu)?,
                    "{dtype:?}, width {width}, contiguous {contiguous}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn the_patch_file_lists_every_patched_source() {
    let patch = vendored("nosh.patch");
    for f in [
        "src/quantized/mod.rs",
        "src/quantized/repack.rs",
        "src/quantized/dummy_metal.rs",
    ] {
        assert!(
            patch.contains(&format!("+++ b/candle-core/{f}")),
            "nosh.patch: {f}"
        );
        assert!(vendored(f).contains("nosh patch"), "{f} is marked");
    }
    let notes = vendored("NOSH_PATCH.md");
    for topic in [
        "prepack_and_release_storage",
        "prepack_x86_and_release_storage",
        "aarch64",
        "dotprod",
        "dummy_metal.rs",
        "nosh.patch",
    ] {
        assert!(notes.contains(topic), "NOSH_PATCH.md: {topic}");
    }
}
