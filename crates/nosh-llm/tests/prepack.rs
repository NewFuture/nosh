//! `QTensor::prepack_x86_and_release_storage` (vendored candle patch): after the
//! raw blocks are dropped, matmuls must match the lazily tiled tensor exactly and
//! candle's raw-weight kernels within rounding, and every raw-data path must
//! fail loudly. On CPUs where candle has no x86 tiles nothing is released and
//! the checks that need a released tensor are skipped.

use candle_core::quantized::{GgmlDType, QMatMul, QTensor};
use candle_core::{DType, Device, Module, Tensor};

const K: usize = 512;

fn pseudo(n: usize, seed: u32) -> Vec<f32> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x % 20_001) as f32 / 10_000.0 - 1.0
        })
        .collect()
}

fn weights(rows: usize) -> Tensor {
    Tensor::from_vec(pseudo(rows * K, 7), (rows, K), &Device::Cpu).unwrap()
}

fn quantize(rows: usize, dtype: GgmlDType) -> QTensor {
    QTensor::quantize(&weights(rows), dtype).unwrap()
}

fn input(m: usize) -> Tensor {
    Tensor::from_vec(pseudo(m * K, 11 + m as u32), (1, m, K), &Device::Cpu).unwrap()
}

fn forward(w: &QMatMul, x: &Tensor) -> Vec<f32> {
    w.forward(x)
        .unwrap()
        .flatten_all()
        .unwrap()
        .to_vec1()
        .unwrap()
}

/// Largest difference relative to the largest magnitude in `want`.
fn rel_err(got: &[f32], want: &[f32]) -> f32 {
    assert_eq!(got.len(), want.len());
    let scale = want.iter().fold(0f32, |a, b| a.max(b.abs())).max(1e-6);
    got.iter()
        .zip(want)
        .map(|(a, b)| (a - b).abs())
        .fold(0f32, f32::max)
        / scale
}

/// Columns `0..n` of a `[m][ld]` row-major matrix.
fn columns(y: &[f32], ld: usize, n: usize) -> Vec<f32> {
    y.chunks(ld).flat_map(|r| r[..n].to_vec()).collect()
}

const MS: [usize; 9] = [1, 2, 3, 4, 5, 17, 32, 33, 64];

#[test]
fn prepacked_matmul_matches_lazy_tiles_and_raw_kernels() {
    for dtype in [GgmlDType::Q4K, GgmlDType::Q8_0] {
        // 48 rows: candle tiles them (n % 16 == 0). 40 rows: candle keeps its
        // raw-weight kernels; the rows are quantized the same way in both.
        let mut packed = quantize(48, dtype);
        let lazy = quantize(48, dtype);
        let raw = quantize(40, dtype);
        let row_bytes = raw.storage_size_in_bytes() / 40;
        assert_eq!(
            &packed.data().unwrap()[..40 * row_bytes],
            &raw.data().unwrap()[..],
            "{dtype:?}: same rows, same blocks"
        );
        let reference = lazy.dequantize(&Device::Cpu).unwrap();
        let released = packed.prepack_x86_and_release_storage().unwrap();
        eprintln!("{dtype:?}: released={released}");
        if released {
            assert_eq!(packed.storage_size_in_bytes(), 0);
        }
        let (packed, lazy, raw) = (
            QMatMul::from_qtensor(packed).unwrap(),
            QMatMul::from_qtensor(lazy).unwrap(),
            QMatMul::from_qtensor(raw).unwrap(),
        );
        for m in MS {
            let x = input(m);
            let got = forward(&packed, &x);
            assert_eq!(got, forward(&lazy, &x), "{dtype:?} m={m}: lazy tiles");
            // Q4K tiles and raw kernels both quantize activations to Q8K; for
            // Q8_0 the raw kernels use Q8_0 activations instead.
            let tol = if dtype == GgmlDType::Q4K { 1e-4 } else { 3e-2 };
            let err = rel_err(&columns(&got, 48, 40), &forward(&raw, &x));
            eprintln!("{dtype:?} m={m}: max error vs raw kernels {err:.1e}");
            assert!(err < tol, "{dtype:?} m={m}: vs raw kernels {err}");
            // Both quantize the activations to 8 bits; the f32 product is close.
            let want: Vec<f32> = x
                .squeeze(0)
                .unwrap()
                .matmul(&reference.t().unwrap())
                .unwrap()
                .flatten_all()
                .unwrap()
                .to_vec1()
                .unwrap();
            let err = rel_err(&got, &want);
            assert!(err < 3e-2, "{dtype:?} m={m}: vs f32 {err}");
        }
    }
}

#[test]
fn released_tensor_refuses_raw_access() {
    let mut t = quantize(32, GgmlDType::Q4K);
    if !t.prepack_x86_and_release_storage().unwrap() {
        eprintln!("skipped: no x86 tiles for Q4K on this CPU");
        return;
    }
    // Idempotent.
    assert!(t.prepack_x86_and_release_storage().unwrap());
    let released = |e: candle_core::Error| {
        let s = e.to_string();
        assert!(s.contains("released"), "{s}");
    };
    released(t.dequantize(&Device::Cpu).unwrap_err());
    released(t.dequantize_f16(&Device::Cpu).unwrap_err());
    released(t.data().unwrap_err());
    let ids = Tensor::new(&[0u32, 3], &Device::Cpu).unwrap();
    released(t.embedding(&ids).unwrap_err());
    let w = QMatMul::from_qtensor(t).unwrap();
    released(
        w.forward(&input(2).to_dtype(DType::F16).unwrap())
            .unwrap_err(),
    );
    // The packed path still works, also for bf16 inputs.
    assert_eq!(forward(&w, &input(3)).len(), 3 * 32);
    let y = w.forward(&input(2).to_dtype(DType::BF16).unwrap()).unwrap();
    assert_eq!(y.dims(), &[1, 2, 32]);
}

#[test]
fn raw_data_stays_where_tiles_do_not_serve_every_m() {
    // Q6K gemv (m == 1) reads the raw blocks; odd row counts are never tiled;
    // only 2D matrices are considered.
    for (dtype, rows) in [(GgmlDType::Q6K, 48), (GgmlDType::Q4K, 40)] {
        let mut t = quantize(rows, dtype);
        let bytes = t.storage_size_in_bytes();
        assert!(!t.prepack_x86_and_release_storage().unwrap(), "{dtype:?}");
        assert_eq!(t.storage_size_in_bytes(), bytes);
        assert!(t.dequantize(&Device::Cpu).is_ok());
        let w = QMatMul::from_qtensor(t).unwrap();
        assert_eq!(forward(&w, &input(1)).len(), rows);
    }
    let flat = Tensor::from_vec(pseudo(2 * K, 3), (2 * K,), &Device::Cpu).unwrap();
    let mut t = QTensor::quantize(&flat, GgmlDType::Q4K).unwrap();
    assert!(!t.prepack_x86_and_release_storage().unwrap());
}
