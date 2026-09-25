//! Explicit, real-model RSS acceptance; never runs as part of ordinary CI.

use std::path::Path;
use std::process::Command;

use nosh_llm::template;
use nosh_llm::tokenizer::Tok;
use serde_json::{Value, json};

const MODEL: &str = "minicpm5-2b:q4_k_m";
const SYSTEM: &str = "You are a helpful assistant.";
const LIMIT_KIB: u64 = 2_621_440;

fn prompt_tokens(tok: &mut Tok, prompt: &str) -> usize {
    let system = tok
        .encode_segments(&template::render_system(Some(SYSTEM), &[]))
        .unwrap();
    let user = tok.encode_segments(&template::render_user(prompt)).unwrap();
    let generation = tok
        .encode_segments(&[template::generation_prompt(Some(false))])
        .unwrap();
    system.len() + user.len() + generation.len()
}

fn long_prompt(tok: &mut Tok) -> (String, usize) {
    let source = format!(
        "Summarize this plan:\n\n{}",
        include_str!("../../../docs/MVP-PLAN.md").repeat(4)
    );
    let ends: Vec<_> = source
        .char_indices()
        .map(|(i, _)| i)
        .chain([source.len()])
        .collect();
    let (mut lo, mut hi) = (0, ends.len() - 1);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if prompt_tokens(tok, &source[..ends[mid]]) < 8064 {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    for &end in &ends[lo..] {
        let prompt = &source[..end];
        let n = prompt_tokens(tok, prompt);
        assert!(n <= 8127, "no suitable non-four-row-aligned 8K prompt");
        if n >= 8064 && !n.is_multiple_of(4) {
            return (prompt.into(), n);
        }
    }
    panic!("fixture is too short for 8K validation");
}

fn word_after<'a>(line: &'a str, marker: &str) -> &'a str {
    line.split_once(marker)
        .unwrap_or_else(|| panic!("missing {marker:?} in {line:?}"))
        .1
        .split_whitespace()
        .next()
        .unwrap_or_else(|| panic!("missing value after {marker:?}"))
}

struct Run {
    peak_rss_kib: u64,
    cli_peak_mib: u64,
    prompt_tokens: usize,
    context_used: usize,
    context_max: usize,
    prepacked_tensors: usize,
    released_mib: usize,
    stdout: Vec<u8>,
}

impl Run {
    fn summary(&self) -> Value {
        json!({
            "peak_rss_kib": self.peak_rss_kib,
            "cli_peak_mib": self.cli_peak_mib,
            "prompt_tokens": self.prompt_tokens,
            "context_used": self.context_used,
            "context_max": self.context_max,
            "prepacked_tensors": self.prepacked_tensors,
            "released_mib": self.released_mib,
        })
    }
}

fn measure(dir: &Path, weights: &Path, prompt: &str, prepack: bool) -> Run {
    let name = if prepack { "prepacked" } else { "retained" };
    let rss_file = dir.join(format!("{name}-rss-kib.txt"));
    let mut cmd = Command::new("/usr/bin/time");
    cmd.args(["-f", "%M", "-o"])
        .arg(&rss_file)
        .arg(env!("CARGO_BIN_EXE_nosh"))
        .args(["--offline", "--model", MODEL, "--model-path"])
        .arg(weights)
        .args(["--seed", "42", "debug", "gen"])
        .arg(prompt)
        .args([
            "--system",
            SYSTEM,
            "--ctx",
            "8192",
            "--max-tokens",
            "64",
            "--temp",
            "0",
            "--kv",
            "f16",
        ])
        .env("CANDLE_NUM_THREADS", "2")
        .env("RAYON_NUM_THREADS", "1");
    if !prepack {
        cmd.arg("--no-prepack");
    }
    let out = cmd
        .output()
        .expect("run the release nosh binary under GNU time");
    std::fs::write(dir.join(format!("{name}.stdout")), &out.stdout).unwrap();
    std::fs::write(dir.join(format!("{name}.stderr")), &out.stderr).unwrap();
    let log = String::from_utf8(out.stderr).expect("UTF-8 debug log");
    assert!(out.status.success(), "{name}: {}\n{log}", out.status);
    let load = log
        .lines()
        .find(|l| l.contains(" | prepacked "))
        .expect("load statistics");
    let round = log
        .lines()
        .find(|l| l.starts_with("[round 1 |"))
        .expect("generation statistics");
    let (used, max) = word_after(round, " | ctx ")
        .split_once('/')
        .expect("used/max context");
    let run = Run {
        peak_rss_kib: std::fs::read_to_string(rss_file)
            .unwrap()
            .trim()
            .parse()
            .expect("RSS KiB"),
        cli_peak_mib: word_after(round, " peak ").parse().expect("VmHWM MiB"),
        prompt_tokens: word_after(round, " | prompt ")
            .parse()
            .expect("prompt tokens"),
        context_used: used.parse().expect("used context"),
        context_max: max.parse().expect("context capacity"),
        prepacked_tensors: word_after(load, " | prepacked ")
            .parse()
            .expect("prepacked count"),
        released_mib: word_after(load, "matrices, ")
            .parse()
            .expect("released MiB"),
        stdout: out.stdout,
    };
    eprintln!("{name}: {}", run.summary());
    run
}

#[test]
#[ignore = "requires Linux ARM64, GNU time and the real model; manual memory workflow"]
fn arm64_8k_rss_and_outputs() {
    assert_eq!(std::env::consts::OS, "linux");
    assert_eq!(std::env::consts::ARCH, "aarch64");
    let features = nosh_llm::cpu::features();
    eprintln!("ARM64 memory validation: {features:?}");
    assert!(
        features.contains(&"dotprod"),
        "cannot validate release without dotprod"
    );
    let r = nosh_hub::ModelHub::new()
        .find(Some(MODEL))
        .expect("model store")
        .expect("download the pinned model with `nosh model pull` first");
    let dir = std::env::var_os("NOSH_MEMORY_ARTIFACTS").map_or_else(
        || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/arm64-memory"),
        std::path::PathBuf::from,
    );
    std::fs::create_dir_all(&dir).unwrap();
    let mut tok = Tok::load(&r.tokenizer).unwrap();
    let (prompt, tokens) = long_prompt(&mut tok);
    std::fs::write(dir.join("prompt.txt"), &prompt).unwrap();
    let retained = measure(&dir, &r.weights, &prompt, false);
    let packed = measure(&dir, &r.weights, &prompt, true);
    let report = json!({
        "model": MODEL,
        "model_sha256": r.entry.weights().sha256,
        "cpu_features": features,
        "context_capacity": 8192,
        "expected_prompt_tokens": tokens,
        "max_new_tokens": 64,
        "kv": "f16",
        "seed": 42,
        "temperature": 0,
        "candle_threads": 2,
        "rayon_threads": 1,
        "limit_kib": LIMIT_KIB,
        "retained": retained.summary(),
        "prepacked": packed.summary(),
        "outputs_identical": retained.stdout == packed.stdout,
    });
    std::fs::write(
        dir.join("memory.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    for run in [&retained, &packed] {
        assert!(
            run.peak_rss_kib > 0 && run.cli_peak_mib > 0,
            "missing RSS statistics"
        );
        assert_eq!(
            run.prompt_tokens, tokens,
            "must actually prefill the full 8K sample"
        );
        assert_eq!(run.context_max, 8192);
        assert!(run.context_used >= tokens && run.context_used <= 8192);
    }
    assert_eq!(retained.prepacked_tensors, 0);
    assert_eq!(retained.released_mib, 0);
    assert!(packed.prepacked_tensors > 0 && packed.released_mib > 0);
    assert!(
        packed.peak_rss_kib <= LIMIT_KIB && packed.cli_peak_mib <= 2560,
        "prepacked RSS exceeds 2.5 GiB: {} KiB (VmHWM {} MiB)",
        packed.peak_rss_kib,
        packed.cli_peak_mib
    );
    assert!(
        packed.peak_rss_kib < retained.peak_rss_kib,
        "prepacking did not reduce peak RSS"
    );
}

#[test]
fn debug_statistics_fields_are_unambiguous() {
    let load = "[model | prepacked 295 matrices, 1339 MiB raw released in 1.00s | RSS 1 MB]";
    let round = "[round 1 | prompt 8065 tok (cached 0) prefill 1.0 tok/s | ctx 8082/8192 | RSS 1 MB peak 2130 MB]";
    assert_eq!(word_after(load, " | prepacked "), "295");
    assert_eq!(word_after(load, "matrices, "), "1339");
    assert_eq!(word_after(round, " | prompt "), "8065");
    assert_eq!(word_after(round, " | ctx "), "8082/8192");
    assert_eq!(word_after(round, " peak "), "2130");
}
