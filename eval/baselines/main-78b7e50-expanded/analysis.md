# Expanded main baseline

The accepted dataset contains **all 250 planned trials**: 25 scenarios, seeds `[0, 1, 2, 3, 4]`, and two serial repetitions on the same GitHub-hosted runner. The normalized result is **120 pass / 130 fail / 0 infrastructure errors / 0 missing**, a **48.0%** pass rate, **4.88 mean steps**, and **1.028 mean confirmation requests**. These means include failed tasks and have 250 measured samples each.

This does **not** mean every task completed successfully. One model generation exhausted the 240-second trial budget and remains a failed task. The original workflow reported it as a driver error and therefore finished red; its raw conclusion and every original observation are retained. No trial was replaced, retried in isolation, or sampled until it passed.

## Source and execution

- Main source: `78b7e509ad0d6d71ce50397cfa9e9f2187b0db75`.
- Runtime evaluator: `581c75a8e0032a5bf9b55eaf5f63b0c0996d2751`.
- Deterministic normalization: `a26d1b2ddf1d06dfcb1bdda2bdae8ddb6922cf12`.
- [Workflow run 36203738656](https://github.com/NewFuture/nosh/actions/runs/36203738656) archived the exact main commit and built it separately from the evaluator checkout. [Build info](build-info.json) records the source archive, build command, Rust/Cargo versions, and binary SHA-256.
- Binary SHA-256: `5b2b34a0328fca694c9f0637868820fd92a1d89c9252641473b6797c548dcee2`.
- MiniCPM5-2B Q4_K_M weights SHA-256: `ec2d5801640099e97d8d7e8003ad4d81f336e757811f03a26173dddf386602fd`; tokenizer SHA-256: `3e065a558a034185fe299917b398685c1facd0169a9eea1e629eb30c171fed81`.
- Environment: Ubuntu 24.04.5, Linux `6.17.0-1022-azure`, x86_64 AMD EPYC 7763, four logical CPUs. Inference used two threads, Rayon one, and nice 10. Every trial had a fresh process/private HOME, confirm mode, offline model access, and rebuilt fixtures.
- Sampling started at `2026-09-26T00:32:58.410898+00:00` and finished before the workflow's normalization step at `04:34:18Z`. The workflow periodically uploaded immutable checkpoints while sampling. Snapshot copying/compression is a source of small concurrent overhead, not part of an isolated performance experiment.

No local model process or local release build was used. The report identifies the main binary separately from the evaluator and later documentation commits.

## Results and denominators

| Group | Pass / planned | Pass rate | Mean steps | Mean confirmations |
|---|---:|---:|---:|---:|
| All scenarios | 120 / 250 | 48.0% | 4.880 | 1.028 |
| Original 10 tasks | 80 / 100 | 80.0% | 2.100 | 0.100 |
| Added 15 tasks | 40 / 150 | 26.7% | 6.733 | 1.647 |
| Model tasks only | 110 / 240 | 45.8% | 5.083 | 1.071 |
| Local spelling correction | 10 / 10 | 100.0% | 0 | 0 |

The all-trial pass rate includes local correction and must not be presented as the model-only pass rate. Full per-scenario counts, limits, individual answers, actual approvals, and final-state evidence are in [report.json](report.json) and [report.md](report.md).

Facts/state pass in **171/250** trials. **51 trials have correct facts/state but fail an experience gate**; checking only the conclusion would hide them. Of the other outcomes, 20 fail facts only, 58 fail both dimensions, and the one generation timeout fails the task without inventing a final-answer judgment.

Experience failures overlap:

| Gate | Failed trials |
|---|---:|
| Step budget | 98 |
| Confirmation budget | 40 |
| Closing question / required clarification | 14 |
| Chinese response | 17 |

There are 249 completed-answer experience judgments; the timed-out task has measured steps but no final answer. It also exceeded its three-step cleanup budget, reaching a seventh started step.

The new suite exposes repeated investigation and unnecessary follow-up operations: build requests often continue into tests, test requests into builds, and genuinely unspecified requests into speculative repairs rather than early clarification. These lead to extra confirmations, step-limit summaries, and sometimes English answers to Chinese requests. Port-failure explanations frequently attempt to rerun the server or otherwise leave the declared read-only diagnostic scope instead of identifying the existing listener.

The approval policy remains an explicit constraint of this evaluation. Across the accepted campaign there were 257 actual requests, of which 164 were denied. The logs distinguish these denials from tool execution failures; this is not an unrestricted-agent score. The policy admits the requested project operations, known fixture scripts, recorded executable aliases, and bounded stderr merging, but not unrelated installs, extra task types, arbitrary interpreter snippets, file redirections, or commands hiding failures.

## Raw evidence and the two deterministic corrections

[raw-report.json](raw-report.json) is the exact attributed report downloaded from the workflow, with **120 pass / 129 fail / 1 error / 0 missing**. [workflow-provenance.json](workflow-provenance.json) preserves the workflow's original `complete: false`, its failed conclusion, and both pre-attribution and attributed hashes. The final [provenance.json](provenance.json) explicitly distinguishes normalization from runtime measurement.

| Trial | Raw → normalized | Evidence and correction |
|---|---|---|
| `zh-rust-test / 0 / 0` | fail → fail | The closing advice says to check whether configuration needs updating. That declarative instruction is not a request for user confirmation. Only the incorrect closing-question reason is removed; the incomplete task and 11 steps against a four-step budget still fail. |
| `zh-clean-build / 1 / 1` | error → fail | The [native trace](timeout-engine.jsonl) contains six complete steps and the start of a seventh, with no matching final result or engine error. The [terminal transcript](timeout-transcript.txt) contains no completion summary and no outstanding approval. Repeated truncated tool calls consumed generation tokens until the 240-second budget expired. This is an unfinished model task, not absent sampling or an unobserved successful completion. |

For the timeout, the recovered step count is **7**, first-step TTFT **16.062120206 s**, measured process duration **240.299445206 s**, and wait4 peak RSS **2428.58203125 MiB**. The first-step TTFT comes from an already completed step; nothing is inferred about first-token latency of the unfinished step. The final answer remains empty. Budget and seed are unchanged.

The strict recovery path requires a native agent/CLI deadline, termination by the driver, properly paired preceding events, and exactly one unfinished model step. Missing/malformed observations, engine errors, initial-prompt/approval/exit timeouts, and an already completed response are not silently converted into task failures.

Changed rows retain `original_judgment` or the complete `original_trial`. Restoring those fields reproduces the entire original ordered trial payload exactly:

`108d7d24e620ea7db697f99ea443e48294e807bb2909f801f089eb1c5939c4c8`

No other trial's inputs, outputs, timing, approvals, sampling, facts, or state changed. The successful-trial count is **120 both before and after normalization**.

## Earlier attempts are not hidden

1. [Run 36167381077](https://github.com/NewFuture/nosh/actions/runs/36167381077) lost its hosted runner. GitHub reported lost communication; job logs returned 404 and no artifact was available. No result from it is claimed or combined with this baseline. Durable checkpoints were subsequently added.
2. [Run 36196161448](https://github.com/NewFuture/nosh/actions/runs/36196161448) completed all 250 trials, but the evaluator incorrectly rejected common safe forms such as `cargo test 2>&1`, as well as trusted project script/tool aliases. Its **107/250** pass count and **5.300/1.232** mean steps/confirmations are diagnostic, not the accepted baseline. The complete original [diagnostic report](diagnostic-report.json) and [provenance](diagnostic-provenance.json) remain unchanged. After correcting the bounded approval parser, the entire 250-trial campaign was restarted; no successful subset was retained and no failed seed was selectively retried.

Do not interpret the difference between the diagnostic and accepted pass rates as a controlled model improvement: evaluator behavior and host observations differ. The two final observation/judgment corrections above required no additional inference.

## Repeatability and measurement limits

**107/125 pairs** have the same verdict and final state; final state alone agrees in **119/125**. Answers differ in 109 pairs, observed tool-call lists in 62, and interface inputs in all 120 model pairs. The five local-correction pairs do not load an engine.

Thus fixed seeds still do not satisfy #3's repeatability acceptance. Task times, command durations, live PIDs, and model-selected actions can change inputs or final state. Interface tracing is not a complete token-prompt capture, so these data do not isolate inference-level numerical nondeterminism.

Native TTFT has 240 model samples, median **16.17 s**, excluding loading. Median process time is **52.05 s** across all trials, **53.91 s** for model tasks only. Maximum wait4 RSS is **2523.65 MiB**. Each process has a cold conversation/KV cache, not necessarily a cold OS page cache. RSS is not a process-tree sum.

The scenario set, grading, thread count, hardware, nice level, and checkpoint activity differ from historical WSL runs. These timings and pass rates are not controlled performance/regression comparisons with the old 10-task baselines.

## Reproduce without inference

With the normalization code at revision `a26d1b2ddf1d06dfcb1bdda2bdae8ddb6922cf12` unchanged:

```bash
python3 eval/baselines/main-78b7e50-expanded/reproduce.py --check
```

This verifies raw hashes and all planned identities, derives the corrections from preserved observations, and checks all final files byte-for-byte. It never starts nosh or a model. The normalization commit must be available in the Git object database; shallow clones need its history first. `GIT` can select a native Git executable when WSL is reading a Windows-managed worktree. The script refuses mismatched normalization sources rather than reinterpreting the historical record with a different grader.

Model weights, build products, and the complete raw-log bundles are not committed. Reports are self-contained; the single timeout's raw evidence is retained because its observation recovery depends on it. Its terminal transcript is marked binary in Git to preserve carriage returns/control bytes and the recorded hash.
