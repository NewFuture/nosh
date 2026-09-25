//! `QTensor::prepack_and_release_storage` (vendored candle patch): after the
//! raw blocks are dropped, matmuls must match the lazily tiled tensor exactly and
//! candle's raw-weight kernels within rounding, and every raw-data path must
//! fail loudly. Release eligibility is checked against the CPU and platform;
//! supported ARM Q4K/Q6K matrices must not silently skip the release checks.
//!
//! CI runs this file again with `--nocapture`, so its log shows the CPU
//! features (see `cpu_features`) and what the tests below found with them.

use candle_core::quantized::{GgmlDType, QMatMul, QTensor};
use candle_core::{DType, Device, Module, Tensor};

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

fn quantize(rows: usize, k: usize, dtype: GgmlDType) -> QTensor {
    let w = Tensor::from_vec(pseudo(rows * k, 7), (rows, k), &Device::Cpu).unwrap();
    QTensor::quantize(&w, dtype).unwrap()
}

fn input(m: usize, k: usize) -> Tensor {
    Tensor::from_vec(pseudo(m * k, 11 + m as u32), (1, m, k), &Device::Cpu).unwrap()
}

fn forward(w: &QMatMul, x: &Tensor) -> Vec<f32> {
    w.forward(x)
        .unwrap()
        .to_dtype(DType::F32)
        .unwrap()
        .flatten_all()
        .unwrap()
        .to_vec1()
        .unwrap()
}

/// Largest difference relative to the largest magnitude in `want`.
fn rel_err(got: &[f32], want: &[f32]) -> f32 {
    assert_eq!(got.len(), want.len());
    assert!(got.iter().chain(want).all(|v| v.is_finite()));
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

fn expected_release(dtype: GgmlDType, n: usize, k: usize) -> bool {
    if n == 0 || k == 0 || !k.is_multiple_of(256) {
        return false;
    }
    let f = nosh_llm::cpu::features();
    match std::env::consts::ARCH {
        "aarch64" => {
            f.contains(&"dotprod")
                && n.is_multiple_of(8)
                && matches!(dtype, GgmlDType::Q4K | GgmlDType::Q6K)
        }
        "x86_64" => {
            let avx2 = f.contains(&"avx2") && f.contains(&"fma");
            let force_avx2 = std::env::var("MISTRALRS_FORCE_AVX2").as_deref() == Ok("1");
            let force_vnni = std::env::var("MISTRALRS_FORCE_AVXVNNI").as_deref() == Ok("1");
            let vnni = !force_avx2
                && ((!force_vnni && f.contains(&"avx512f") && f.contains(&"avx512vnni"))
                    || (f.contains(&"avx2") && (force_vnni || f.contains(&"avxvnni"))));
            n.is_multiple_of(16)
                && ((dtype == GgmlDType::Q4K && (avx2 || vnni))
                    || (dtype == GgmlDType::Q8_0 && vnni))
        }
        _ => false,
    }
}

fn prepack(t: &mut QTensor) -> bool {
    let (n, k) = t.shape().dims2().unwrap();
    let bytes = t.storage_size_in_bytes();
    let released = t.prepack_and_release_storage().unwrap();
    eprintln!("{:?} n={n} k={k}: released={released}", t.dtype());
    assert_eq!(released, expected_release(t.dtype(), n, k));
    assert_eq!(t.storage_size_in_bytes(), if released { 0 } else { bytes });
    released
}

fn print_cpu_features() {
    let f = nosh_llm::cpu::features();
    eprintln!(
        "cpu: {} {} | {} threads | {} | dotprod={} i8mm={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(1, |n| n.get()),
        f.join(" "),
        f.contains(&"dotprod"),
        f.contains(&"i8mm")
    );
}

/// Which of candle's kernels the tests here ran: the tile layouts and the
/// dotprod/i8mm paths are picked by these runtime features.
#[test]
fn cpu_features() {
    let f = nosh_llm::cpu::features();
    print_cpu_features();
    if cfg!(target_arch = "aarch64") {
        assert!(f.contains(&"neon"), "{f:?}");
    }
}

#[test]
fn prepacked_matmul_matches_lazy_tiles_and_raw_kernels() {
    print_cpu_features();
    for dtype in [GgmlDType::Q4K, GgmlDType::Q6K, GgmlDType::Q8_0] {
        for (n, k) in [(48, 256), (48, 512), (40, 512)] {
            // n-1 cannot use any ARM (4/8-row) or x86 (16-row) layout.
            let mut packed = quantize(n, k, dtype);
            let lazy = quantize(n, k, dtype);
            let raw = quantize(n - 1, k, dtype);
            let row_bytes = raw.storage_size_in_bytes() / (n - 1);
            assert_eq!(
                &packed.data().unwrap()[..(n - 1) * row_bytes],
                &raw.data().unwrap()[..],
                "{dtype:?}: same rows, same blocks"
            );
            let reference = lazy.dequantize(&Device::Cpu).unwrap();
            prepack(&mut packed);
            let (packed, lazy, raw) = (
                QMatMul::from_qtensor(packed).unwrap(),
                QMatMul::from_qtensor(lazy).unwrap(),
                QMatMul::from_qtensor(raw).unwrap(),
            );
            for m in (0..=64).chain([511, 512, 513]) {
                let x = input(m, k);
                let got = forward(&packed, &x);
                assert_eq!(got.len(), m * n);
                assert_eq!(got, forward(&lazy, &x), "{dtype:?} m={m}: lazy tiles");
                // K-quants share Q8K activations. Q8_0's raw kernel instead
                // uses Q8_0 activations, with different quantization rounding.
                let tol = if dtype == GgmlDType::Q8_0 { 3e-2 } else { 1e-4 };
                let err = rel_err(&columns(&got, n, n - 1), &forward(&raw, &x));
                eprintln!("{dtype:?} n={n} k={k} m={m}: max error vs raw {err:.1e}");
                assert!(err < tol, "{dtype:?} m={m}: vs raw kernels {err}");
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
}

#[test]
fn released_tensor_refuses_raw_access() {
    print_cpu_features();
    for dtype in [GgmlDType::Q4K, GgmlDType::Q6K, GgmlDType::Q8_0] {
        let mut t = quantize(32, 512, dtype);
        let lazy = QMatMul::from_qtensor(quantize(32, 512, dtype)).unwrap();
        if !prepack(&mut t) {
            eprintln!("{dtype:?}: raw-access errors not applicable on this CPU");
            continue;
        }
        assert!(t.prepack_and_release_storage().unwrap());
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
            w.forward(&input(2, 512).to_dtype(DType::F16).unwrap())
                .unwrap_err(),
        );
        for m in 0..=64 {
            // A contiguous view with a nonzero offset must slice tail rows
            // relative to the view, not the start of the tensor's storage.
            let x = input(m + 1, 512).narrow(1, 1, m).unwrap();
            assert_eq!(forward(&w, &x), forward(&lazy, &x), "{dtype:?} m={m}");
            let x = x.to_dtype(DType::BF16).unwrap();
            assert_eq!(forward(&w, &x), forward(&lazy, &x), "{dtype:?} bf16 m={m}");
        }
    }
}

#[test]
fn raw_data_stays_where_tiles_do_not_serve_every_m() {
    print_cpu_features();
    let mut cases = vec![
        (GgmlDType::Q4K, 39, 512),
        (GgmlDType::Q6K, 39, 512),
        (GgmlDType::Q4_0, 32, 512),
        (GgmlDType::Q5K, 32, 512),
        (GgmlDType::Q8_0, 32, 128),
    ];
    if cfg!(target_arch = "aarch64") {
        cases.push((GgmlDType::Q8_0, 32, 512));
    } else {
        cases.extend([(GgmlDType::Q6K, 48, 512), (GgmlDType::Q4K, 40, 512)]);
    }
    for (dtype, rows, k) in cases {
        let mut t = quantize(rows, k, dtype);
        let bytes = t.storage_size_in_bytes();
        let data = t.data().unwrap().into_owned();
        assert!(!prepack(&mut t), "{dtype:?}");
        assert_eq!(t.storage_size_in_bytes(), bytes);
        assert_eq!(&*t.data().unwrap(), data);
        assert!(t.dequantize(&Device::Cpu).is_ok());
        let w = QMatMul::from_qtensor(t).unwrap();
        assert_eq!(forward(&w, &input(1, k)).len(), rows);
    }
    for shape in [vec![1024], vec![2, 8, 256]] {
        let w = Tensor::from_vec(pseudo(shape.iter().product(), 3), shape, &Device::Cpu).unwrap();
        let mut t = QTensor::quantize(&w, GgmlDType::Q4K).unwrap();
        let bytes = t.storage_size_in_bytes();
        assert!(!t.prepack_and_release_storage().unwrap());
        assert_eq!(t.storage_size_in_bytes(), bytes);
        assert!(t.data().is_ok());
    }
}

#[test]
fn fused_gemv_reads_released_tiles() {
    print_cpu_features();
    for dtype in [GgmlDType::Q4K, GgmlDType::Q6K, GgmlDType::Q8_0] {
        let mut ts = [quantize(16, 512, dtype), quantize(48, 512, dtype)];
        if !prepack(&mut ts[0]) {
            continue;
        }
        assert!(prepack(&mut ts[1]));
        let x = input(1, 512);
        let ys = QTensor::gemv_fused_shared_lhs(&[&ts[0], &ts[1]], &x).unwrap();
        if cfg!(target_arch = "aarch64") {
            assert!(ys.is_some(), "{dtype:?}: ARM fused GEMV must use the cache");
        }
        if let Some(ys) = ys {
            assert_eq!(ys.len(), ts.len());
            for (t, y) in ts.into_iter().zip(ys) {
                let want = forward(&QMatMul::from_qtensor(t).unwrap(), &x);
                let got = y.flatten_all().unwrap().to_vec1::<f32>().unwrap();
                assert!(rel_err(&got, &want) < 1e-4, "{dtype:?}: fused GEMV");
            }
        }
    }
}

#[test]
fn x86_compatibility_entry_point_keeps_its_scope() {
    for dtype in [GgmlDType::Q4K, GgmlDType::Q6K, GgmlDType::Q8_0] {
        let mut t = quantize(48, 512, dtype);
        let old = t.prepack_x86_and_release_storage().unwrap();
        assert_eq!(
            old,
            cfg!(target_arch = "x86_64") && expected_release(dtype, 48, 512)
        );
        assert_eq!(
            t.prepack_and_release_storage().unwrap(),
            expected_release(dtype, 48, 512)
        );
    }
}
