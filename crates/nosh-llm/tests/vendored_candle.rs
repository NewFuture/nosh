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
        "prepack_x86_and_release_storage",
        "dummy_metal.rs",
        "nosh.patch",
    ] {
        assert!(notes.contains(topic), "NOSH_PATCH.md: {topic}");
    }
}
