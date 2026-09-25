//! Explicit, real-model RSS acceptance; never runs as part of ordinary CI.

use std::path::Path;
use std::process::Command;

use nosh_llm::template;
use nosh_llm::tokenizer::Tok;
use serde_json::json;

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

fn check_stats(log: &str, tokens: usize, prepack: bool) {
    let number = |marker| {
        word_after(log, marker)
            .parse::<usize>()
            .expect("numeric debug statistic")
    };
    let round = log
        .lines()
        .find(|line| line.starts_with("[round 1 |"))
        .expect("generation statistics");
    let (used, max) = word_after(round, " | ctx ")
        .split_once('/')
        .expect("used/max context");
    let used: usize = used.parse().expect("used context");
    assert_eq!(max, "8192");
    assert!(used >= tokens && used <= 8192);
    assert_eq!(
        number(" | prompt "),
        tokens,
        "must prefill the full 8K sample"
    );
    assert_eq!(number(" | prepacked ") > 0, prepack);
    assert_eq!(number("matrices, ") > 0, prepack);
    let peak = number(" peak ");
    assert!(peak > 0, "missing VmHWM");
    assert!(
        !prepack || peak <= 2560,
        "VmHWM exceeds 2.5 GiB: {peak} MiB"
    );
}

fn measure(
    dir: &Path,
    weights: &Path,
    prompt: &str,
    tokens: usize,
    prepack: bool,
) -> (u64, Vec<u8>) {
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
    check_stats(&log, tokens, prepack);
    let rss = std::fs::read_to_string(rss_file)
        .unwrap()
        .trim()
        .parse()
        .expect("RSS KiB");
    assert!(rss > 0, "missing GNU time RSS");
    eprintln!("{name}: peak RSS {rss} KiB");
    (rss, out.stdout)
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
    let (retained, retained_output) = measure(&dir, &r.weights, &prompt, tokens, false);
    let (packed, packed_output) = measure(&dir, &r.weights, &prompt, tokens, true);
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
        "peak_rss_kib": { "retained": retained, "prepacked": packed },
        // Diagnostic only; numerical acceptance is enforced by
        // `prepacked_weights_match_retained_weights` (DESIGN section 13.2).
        "outputs_identical": retained_output == packed_output,
    });
    std::fs::write(
        dir.join("memory.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    assert!(
        packed <= LIMIT_KIB,
        "prepacked RSS exceeds 2.5 GiB: {packed} KiB"
    );
    assert!(packed < retained, "prepacking did not reduce peak RSS");
}

#[test]
fn debug_statistics_enforce_acceptance() {
    let log = "[model | ctx 8192 | prepacked 295 matrices, 1340 MiB raw released in 0.38s | RSS 1617 MB]\n\
        [round 1 | prompt 8065 tok (cached 0) prefill 20.3 tok/s | ctx 8131/8192 | RSS 2036 MB peak 2103 MB]";
    check_stats(log, 8065, true);
    let retained = log
        .replace("295 matrices, 1340", "0 matrices, 0")
        .replace("2103", "3492");
    check_stats(&retained, 8065, false);
    for invalid in [
        log.replace("295 matrices", "0 matrices"),
        log.replace("matrices, 1340", "matrices, 0"),
        log.replace("prompt 8065", "prompt 2048"),
        log.replace("8131/8192", "8131/4096"),
        log.replace("8131/8192", "8000/8192"),
        log.replace("8131/8192", "9000/8192"),
        log.replace("peak 2103", "peak 0"),
        log.replace("peak 2103", "peak 2561"),
        log.replace(" peak ", " missing "),
    ] {
        assert!(std::panic::catch_unwind(|| check_stats(&invalid, 8065, true)).is_err());
    }
}
