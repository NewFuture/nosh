# Minimal tools / prompt experiments

**Historical v1–v4 result: the core tool changes are implemented, but the language-lines acceptance gate was not met. This was not a passing prompt optimization.** All four raw archives and their conclusions remain unchanged. A subsequent product decision replaces the experimental Full prompt with **v5/general**, not v2 or v4: two task-independent principles, a flat installed-command list and neutral tool descriptions. Its [fixed 50-trial full-suite audit](v5-general.md) is complete: **34/50 versus formal main repeat 0's 36/50**, with five regressions and three improvements. PR #17 remains Draft pending a product decision on those regressions; language-lines is no longer a standalone merge gate. No further wording iteration is planned.

## Fixed campaigns

Each v1–v4 candidate ran both scenarios with seeds 0–4 × repeat 2, serially, at **temperature 1.0**. All 80 planned trials finished recording; no retries, replacement samples, missing trials, grader changes or tool-use requirements were introduced. The formal baseline is unchanged. Its agent temperature was 1.0 but its suggestion temperature was **0.7**, so the suggestion comparison is an overall behavior comparison, not a temperature-controlled prompt-only experiment.

| Candidate / measured source | language-lines pass / 10 | Median steps | Fail / error | suggest-archive pass / 10 | Median steps | First LOC tool | CI at measured source |
|---|---:|---:|---|---:|---:|---|---|
| Formal main `4f602ab8d95d046162adb7d4b202ddf6d3e20bea` | 1 | 4 | 9 / 0 | 6 | 1 | Historical baseline | Previously recorded |
| v1 `fee7dd62484c993651c441680ae996c0575d2e65` | 1 | 3 | 9 / 0 | 4 | 1 | list_dir 10/10 | Three platforms passed, [run](https://github.com/NewFuture/nosh/actions/runs/36130086930) |
| v2 `5df7060f6b5a4a01dcb27b84592a00679a8ee262` | 2 | 3.5 | 8 / 0 | 10 | 1 | list_dir 8, run_command 2 | Three platforms passed, [run](https://github.com/NewFuture/nosh/actions/runs/36130905074) |
| v3 `1993da25c27b200f72f156a805de523418d6c8ff` | 1 | 4¹ | 8 / 1 | 10 | 1 | run_command 10/10 | Three platforms passed, [run](https://github.com/NewFuture/nosh/actions/runs/36132036930) |
| v4 `5a4c28c91e7d3385accd439e9fc730b45b78de3a` | 0 | 11 | 10 / 0 | 9 | 1 | run_command 10/10 | Linux x64/ARM passed; macOS skipped on push and PR run blocked by new-main conflict; see post-merge CI on PR |

¹ Nine observable completed-task step counts; the tenth trial timed out at 240 seconds, remains in the denominator and has its complete available raw trace. It is not treated as a successful or missing sample.

**Best observed historical candidate: v2**, based on its complete fixed samples (2/10 LOC, 10/10 suggestions), not on individual successes. It still fails both historical LOC targets: ≥8/10 and median ≤2 steps. The campaign stopped after v4: **the gated 50-trial full regression and independent 0.7 experiment were not started**, and there was no real-model rerun at that stage after merging main. The subsequent product decision explicitly rejects these benchmark-oriented Full prompts rather than selecting v2; v5/general is a separately recorded full-suite campaign.

## What the traces show

- **v1:** all LOC trials first listed directories; subsequent `wc` output was often per-file and manually added or mislabeled. Suggestions added unrequested `mkdir`/fallback commands (6 failures). This motivated only generic wording about final aggregates and no extra setup.
- **v2:** suggestions became 10/10. LOC still usually started with directory exploration, then per-file output and manual totals; 2 passes, with step-limit loops in other trials. Two review fixes were also included (function/coprocess suggestion validation and sudo handoff despite a later successful command), so v1→v2 is not described as a perfectly isolated prompt-only code change.
- **v3:** direct `run_command` became the first tool in all LOC trials, but long generated awk pipelines grouped by the wrong key, repeated unchanged, or produced invalid totals. One 240-second timeout and several incomplete tasks occurred. Tool choice alone did not predict correctness. This motivated generic short-pipeline/key-accumulation/no-repeat wording, not a language-specific command template.
- **v4:** 0/10 LOC. Wrong grouping, per-file rather than language totals and unchanged-command loops persisted; six trials reached 11 model steps. Suggestions were 9/10; the last trial generated the wrong archive destination/format. This is a regression, not an improvement hidden by the core-feature gains.

Arbitrary Python programs require approval under the existing permissions policy, and this scenario's declared policy denies such calls. The experiment did not loosen permissions to make scripts pass. Some answers mix correct totals with incorrect or ambiguously labeled detail; the unchanged conservative grader checks the whole answer. No failures were manually reclassified.

Although the experiments initially described these algorithmic instructions as generic, product review rejected the v2–v4 Full prompts as too specific to the targeted computation benchmark. None of those instructions form the final design; v5/general removes them rather than selecting the best historical sample.

## Independently verified core behavior

Full tools changed from `run_command/read_file/list_dir/propose_command` to `run_command/read_file/list_dir/search`. ReadOnly now includes search; Suggest offers **no tools**.

`-s`/Ctrl+G return one direct brush-validated shell program (a single sh/bash fence or complete multi-line loop/conditional is accepted), never execute it, reject prose/multiple candidates/incomplete syntax, and retain stdout-only commands / editor prefill behavior. All observed suggestions used zero tool calls. Ordinary agent advice stays in final text. Actual SIGTTIN is covered by an integration test; terminal or recognized sudo-password handoff returns the original command and ends the task without another model step or executing later calls.

`search(pattern, path?, glob?)` uses `grep-regex` + `grep-searcher` + `ignore`, with no external rg dependency. Tests cover gitignore, glob intersection, hidden/binary files, relative paths/line numbers, multiple files, no matches, regex/path/read errors, output limits, symlinks, protected roots/descendants and `..`. Maximum output is 200 matching lines / 6,000 characters with explicit truncation. These tests establish the API/permission behavior, not model search quality (the targeted tasks did not exercise search).

## Static prefix tokens

Measured with nosh's actual Rust `Tok` and `template::render_system`, using retained open-session specs and the exact model tokenizer. “Rendered” includes BOS, the system wrapper and tool definitions, but excludes dynamic task messages and generation tokens; raw system counts include the tool placeholder where present. This is not an estimate from character count.

| Version | Full raw system | Full rendered prefix | Suggest raw system | Suggest rendered prefix |
|---|---:|---:|---:|---:|
| Formal baseline | 252 | 771 | 75 | 301 |
| v1 | 349 | 958 | 80 | 86 |
| v2 | 380 | 989 | 108 | 114 |
| v3 | 356 | 964 | 108 | 114 |
| v4 | 394 | 1,000 | 108 | 114 |

The suggestion prefix fell **187 tokens / 62.1%** from baseline to v4. The full prefix grew **229 tokens / 29.7%**: the richer search schema, capability groups and rules outweigh removal of the old suggestion tool. Do not claim the full prompt became smaller. The installed-capability probe has only 36 fixed candidates, not a complete PATH command list, and remains static per conversation.

## Performance and memory

All available samples, including failures, contribute. TTFT/process total are medians; RSS is maximum Linux wait4 peak MiB. Cold model/KV sessions do not imply cold OS page cache.

| Version | LOC TTFT / total seconds | LOC peak MiB | Suggest TTFT / total seconds | Suggest peak MiB |
|---|---|---:|---|---:|
| Formal baseline | 5.12 / 38.62 | 2409.88 | 2.12 / 6.97 | 2355.93 |
| v1 | 5.87 / 29.90 | 2437.19 | 1.34 / 4.29 | 2318.50 |
| v2 | 6.19 / 32.27 | 2531.02 | 1.46 / 4.13 | 2323.83 |
| v3 | 5.89 / 64.14 | 2582.30 | 1.39 / 4.01 | 2323.83 |
| v4 | 6.56 / 97.19 | 2566.64 | 1.40 / 4.08 | 2323.91 |

Suggestions are observably faster with a smaller prefix. LOC v4 regresses in task time and peak memory because of longer/repeated trajectories, and its TTFT is higher. There is no evidence for a blanket “no performance regression” claim. Different prompts, task timestamps, suggestion temperatures versus baseline, and main's intervening ARM-only changes limit causal attribution; no statistical significance claim is made from ten trials.

## Provenance, resource isolation and merge

[`manifest.json`](manifest.json) contains each full source revision, binary SHA-256, archive/report hashes, aggregate metrics and first-tool counts. The four `.tar.gz` archives preserve every report and raw stdout/stderr/transcript/engine JSONL, including all failures. Each archive has one top-level versioned target folder plus its build record; v2–v4 also contain preflight records. These are local synthetic-fixture observations, not user workspace content. Existing official `eval/baselines` files were not replaced.

Builds used isolated `~/.cache/nosh-minimal-tools-target`, Rust 1.98.1, locked release, eight build threads. Evaluation used the existing runner: MiniCPM5-2B Q4_K_M, 8 inference threads, Rayon 1, 8K context, 240-second timeout, confirm approvals and five fixed seeds. Every campaign was serial. Before each, no other cargo/rustc/eval job was active, 5-second CPU samples of the four pre-existing user REPLs were only 0–3 ticks, available memory was about 20.5–20.7 GiB, and swap use/growth was zero. Those user REPLs were never stopped.

After v4, main `826a825` (#16) was merged as `5a081ef`, retaining all terminal/UI/handler/dependency/README/DESIGN and eval-driver updates; both README additions were retained. The v4 prompt was not changed by this merge. Local fmt, clippy `-D warnings`, full workspace tests (including #16 terminal tests), and **41** Python tests passed. Current-head three-platform CI is reported in the PR; the evaluated binaries remain the pre-merge revisions above, not the final documentation/merge head.

Limitations remain explicit: shell syntax/name checks do not prove intent; unavailable literal command names are rejected; dynamically expanded names are not statically resolved; streaming binary detection and an 8 MiB search line-buffer limit apply; existing terminal-process-group limitations persist; only specific English sudo diagnostics trigger the equivalent password handoff. No v1–v4 full-suite quality or independent temperature-0.7 conclusion is available; the subsequent v5/general full-suite results are reported separately above.
