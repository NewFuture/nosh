# Pinned main native baseline

**The 100-trial campaign is complete; issue #3's repeatability acceptance is not.** Exact merged main `4f602ab8d95d046162adb7d4b202ddf6d3e20bea` produced **70 pass / 30 fail / 0 error / 0 missing** with its unchanged evaluator. Three demonstrated scope-parser false negatives were deterministically corrected to **73 / 27 / 0 / 0**, without rerunning inference: **63/90 model trials** and **10/10 local corrections** pass, so 73% is not the model-only pass rate. Verdict plus final state agrees in **45/50 pairs**, not 50/50. This baseline is related to #3, not evidence to close it.

The [JSON](report.json) contains all original answers, observed inputs, tool calls, metrics, approvals, final states and `original_judgment` for every trial. The generated [Markdown](report.md) contains per-trial answers and metric summaries. [Build info](build-info.json) pins the source archive, binary, command and toolchain; [provenance](provenance.json) records all source/model/artifact hashes and the preservation checks.

## Campaign and attribution

The clean source was archived with native Windows Git, extracted under ignored `target/formal-main-4f602ab/source`, and built in WSL Ubuntu before any tracked edit. All 181 archived source files matched both the extracted source and clean worktree byte-for-byte. The retained release binary hashes to `9e3967568c72b67165fbfee2bf9f03a33cd2841828e263251285951910418c06`; its archive hashes to `fb9b5dd0085136fecaaa550ec242fb87a4f65e52e646b968527eda60f59994fd`. Later report/grader commits do not identify the measured runtime.

One serial native campaign ran from **2026-09-25 06:07:19 to 06:37:54 UTC**, with the pristine main harness, its default five seeds, two repeats, threads 8 / Rayon 1, C.UTF-8 / UTC, and the established private, exclusively locked `/tmp/nosh-eval-1000`. No alternative attempts, failure retries, changed seeds, interrupted recovery or resampling occurred. Exit 1 means task failures/repeat differences; all 100 planned identities are present, with zero runner errors.

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m eval.run \
  --model-path /home/newfuture/.local/share/nosh/models/minicpm5-2b-q4_k_m \
  --binary target/formal-main-4f602ab/build/release/nosh --repeat 2 \
  --build-info target/formal-main-4f602ab/build-info.json \
  --output eval/baselines/main-4f602ab
```

The machine/model match the earlier baseline: Ubuntu 26.04.1 on WSL2, Xeon Platinum 8370C, 16 logical CPUs, about 31 GiB RAM, rustc/cargo 1.98.1, MiniCPM5-2B Q4_K_M and its adjacent tokenizer. Weight/tokenizer hashes were checked before sampling and are recorded in provenance. Competing local inference/build owners paused their work. Three existing sleeping REPLs remained resident: the final three-second sample showed 1/0/0 CPU ticks (100 Hz), no swapped memory and about 23.2 GiB available. Limited in-campaign parent sampling also found no swap pressure or competing inference; these observations do not claim complete host isolation. No unrelated process was terminated.

## Original and corrected outcomes

| Scenario | Raw pass/10 | Regraded pass/10 | Verdict + state pairs/5 |
|---|---:|---:|---:|
| largest-files | 9 | 10 | 5 |
| listening-port | 10 | 10 | 5 |
| language-lines | 0 | 1 | 4 |
| rename-files | 10 | 10 | 5 |
| chinese-python | 7 | 8 | 5 |
| typo-correction | 10 | 10 | 5 |
| explain-failure | 8 | 8 | 3 |
| summarize-git-log | 10 | 10 | 5 |
| suggest-archive | 6 | 6 | 3 |
| persistent-cwd | 0 | 0 | 5 |

Only the following three verdicts changed under grader `554d7e87eb44ef47bd7b5eb0e7c0481f7448a62f`:

| Trial (scenario / seed / repeat) | Original evidence and correction |
|---|---|
| largest-files / 1 / 0 | Numbered entries 1-3 are `dump.bin`, `video.bin`, `cache/archive.bin`, correctly ordered. A separate "For reference ... smaller file" bullet names `notes.txt`. Do not merge that reference block into the ranking; an actual fourth ranked entry or incorrect order still fails. |
| language-lines / 2 / 0 | Explicit Python sections contain main/report = 10 lines and lib/maths = 5; the overall language table is 15/4/6/4 and grand total 29 across 6 files. When subtotals are claimed, combine them only for disjoint, complete named file scopes with one line subtotal each. Entirely unclaimed subtotals remain optional when overall language counts are explicit. Global contradictions, partial/overlapping subtotal scopes and incorrect totals still fail. |
| chinese-python / 3 / 1 | A neutral **directory overview** lists all project files, followed by an explicit, correct three-Python-file section and a non-Python explanation. Only explicitly headed neutral overviews are excluded from classification; the Python section must independently include all expected files. |

The three other judgment changes affect reasons only: language-lines / 3 / 0 remains a file-count-only failure; / 4 / 0 still incorrectly claims 34 total lines; / 4 / 1 still incorrectly claims 5 total files. Both chinese-python seed-0 answers explicitly put `check.sh` fourth under a Python heading and remain failures despite parenthetical Shell labels. No approval policy or task fact was relaxed.

The raw JSON SHA-256 is `baab48aeeb668da4f06e127a04bf32595f371cb4aa91177145a0d3267075d54a`; raw Markdown is `b256316c3f8dac3118dd42c9c9027d0ee8ddc9fee8918c3676375a6a971b1b89`. Before editing, both were copied unchanged to ignored local storage. The ordered non-judgment trial payload has the same SHA-256 before and after regrading: `3d13836bc9345e69a92534fc4e59ad7361feffa98834e5242acc194818835be1`. This covers inputs, outputs, sampling, timings, states, facts, approvals and identities, excluding only `status`, `reasons` and `original_judgment`.

Original report bytes can also be verified without the ignored backup: load `report.json`, remove `metadata.regrade`, restore `metadata.grading_content_sha256` from its recorded original value, restore each trial from `original_judgment` and remove that field, then regenerate `summary`/`reproducibility` using `eval.report`. Serialize with the existing `report.save` JSON options and regenerate Markdown. Both hashes equal the raw hashes above; this exact reconstruction was checked, not merely the counts.

## Every remaining repeatability mismatch

All **50/50 final states agree**, including file-content hashes and observed cwd. Raw verdict+state consistency was 44/50; regrading removes the largest-file and directory-overview artifacts but exposes a genuine language-lines pass/fail pair, leaving **45/50** (40/45 model pairs plus 5/5 local-correction pairs). There are 34 pass/pass, 11 fail/fail and 5 mixed pairs, with no unusable pairs.

| Scenario / seed | Repeat 0 -> 1 | Evidence explaining the verdict difference | Initial task time (UTC) |
|---|---|---|---|
| language-lines / 2 | pass -> fail | Both have the same tool calls and correct language totals; repeat 1's final answer instead says **30 lines across 5 files**, not 29/6. Repeat 0's split 10+5 Python subtotals are valid. | 06:10 -> 06:26 |
| explain-failure / 0 | pass -> fail | Repeat 0 reads the script and inspects missing config with `ls`/`cat`. Repeat 1 requests `cd ... && python3 broken.py`; the declared deny policy actually answers `n`, and recorded task status is `incomplete`. Its final missing-file explanation does not override that status. | 06:16 -> 06:31 |
| explain-failure / 3 | fail -> pass | Reverse of the above: repeat 0's Python rerun is denied and status incomplete; repeat 1 uses read-only inspection and completes. Approvals and tool responses are retained. | 06:17 -> 06:33 |
| suggest-archive / 1 | fail -> pass | `cd logs && tar -czvf logs.tar.gz *.log` chooses the wrong archive location and omits nested logs; repeat 1 returns valid `tar -czvf logs.tar.gz logs/`. The invalid command was not executed. | 06:22 -> 06:36 |
| suggest-archive / 3 | fail -> pass | Repeat 0 suggests placeholder `/path/to/logs`; repeat 1 returns valid `tar -czvf logs.tar.gz logs/`. The escaping placeholder was rejected without execution. | 06:22 -> 06:36 |

Answers differ in **43/50** pairs; observed tools differ in **17/45**, interface inputs in **45/45**. The five local correction pairs have no engine, so tool/input comparisons are N/A, not failed observations. All 45 model pairs' `open` + first `step_start` records differ. Masking only task-header times and recent-command durations **for analysis** makes all 45 initial interface records equal; this was never applied to inference. For example, explain-failure seed 1 records a prior-command duration of 0.2 s versus 0.1 s even though both verdicts pass.

Later inputs can differ through tool duration headers, actual listener PIDs, `ls -la` parent-directory timestamps and changed tool branches. The five port pairs identify different real listener PIDs correctly. These are demonstrated input differences, not a controlled experiment isolating their causal contribution. Interface tracing also omits internally retained assistant tokens. Therefore this campaign establishes neither identical complete token prompts nor inference-level numerical nondeterminism. The outstanding acceptance is the five verdict mismatches; collecting a baseline does not make them disappear.

## Failure modes and measurement limits

The 27 retained failures are: **10 cwd** trials using only `list_dir(data)` without an actual `cd` (the physical cwd stays at the fixture root); **9 line-count** trials confusing files/bytes with lines, omitting files, misclassifying languages or contradicting totals; **4 archive suggestions** with wrong path/range or a placeholder; **2 explicit Python misclassifications**; and **2 incomplete failure-analysis tasks after denied execution**. A pass means the declared facts/checks were satisfied, not that every incidental explanatory sentence is correct.

There are **90/90 measured native TTFT values**, including **10/10 `-s`** values (median 2.12 s). All 10 local corrections have steps 0 and TTFT null/N/A. Across the campaign, TTFT median is **4.90 s** over 90 model trials, process time median **16.93 s** over all 100 trials (**17.85 s** for model trials only), and model peak RSS ranges from **2297.21 to 2423.92 MiB**; steps/confirmations average **2.18/0.14** across all trials. Per-scenario values and exact sample counts are in the report, including failures.

Every trial is a new process: cold conversation/KV, **not guaranteed cold OS page cache** (hashing and preceding trials may warm it). First-step Usage TTFT excludes model loading. Process time includes loading, interaction and exit, excluding fixture creation and suggestion validation. RSS is Linux `wait4.ru_maxrss`, converted to MiB for each nosh process, including the kernel's accounting of waited-for descendants, **not a sum of process-tree RSS**; listener and verifier are separate processes.

The historical [main-7c57a88 legacy baseline](../main-7c57a88/report.md) remains byte-for-byte unchanged at 36/50 pass, 14 fail, 0 runner errors. Its single-run/limited-observation results and the earlier manual warm-session MVP results are not controlled before/after comparators for this campaign. Source archives, binaries, weights, personal configuration and 390 raw engine/PTY log files are not committed. Original raw reports/logs remain local; only self-contained reports, source/provenance hashes and this bounded analysis are versioned.
