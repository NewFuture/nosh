# v5/general: fixed full-suite audit

**34/50 passed, 16 failed, 0 errors, 0 missing.** The predeclared formal-main comparison is **36/50** on repeat 0: five pass-to-fail pairs and three fail-to-pass pairs. This is **not a no-regression result**. PR #17 remains Draft pending a product decision on these broader regressions, not the former language-lines-only gate. No v6 wording iteration, retry or extra targeted campaign was run.

## Fixed candidate and protocol

- Measured source: `2c43aa96ec45e1a4c3de01c53075f829446d07da`; binary SHA-256: `d1ba56fe415aa8044a0de67f35a8b362c05de93add603a09de96a6a350fd5383`.
- One serial campaign: 10 scenarios x seeds 0-4 x repeat 1. The 50 trials include **45 model tasks (29 passed)** and five local corrections (all passed); these denominators are not interchangeable.
- MiniCPM5-2B Q4_K_M, default temperature 1.0, top-p 0.95, min-p 0, repetition penalty 1.05, tool-call temperature 0.3, eight inference threads, Rayon 1, 8K context, 240s timeout and unchanged confirm approvals. No grader, permission, fixture or sampling changes were made to improve scores.
- Primary pairing was fixed **before inference**: formal main `4f602ab8d95d046162adb7d4b202ddf6d3e20bea`, repeat **0**, same scenario/seed. All 50 match; the baseline's other 50 observations are intentionally unpaired. Repeat 1 is only a whole-run sensitivity reference (37/50), never a per-trial alternative.
- Suite, grading-source, model, settings, machine and tool-version metadata match the formal baseline. The runner correctly warns that **harness sources differ** (including retained #16 driver updates) and **observed suggestion sampling differs** (historical 0.7 versus current default 1.0). Dynamic timestamps, recent-command/tool durations and live listener PIDs also differ. Paired changes are descriptive, not a controlled prompt-only causal estimate.

The final Full prompt has only the two requested task-independent principles plus shared-shell, non-interactive, destructive-action safety, untrusted-output, task-header and brief-answer rules. Installed commands form a flat, actual-installed subset of 36 fixed candidates. Tool descriptions state interface facts. The generic Suggest contract is retained. The exact Full/ReadOnly/Suggest strings and all tool schemas are in [`v5-audit.json`](v5-audit.json); the final design is in [DESIGN section 5.4](../../../docs/DESIGN.md#54-prompt). The v1-v4 prompts were not selected, and their raw archives remain unchanged.

## Per-scenario results and metrics

All cells with arrows are **formal main repeat 0 -> v5**, using every available sample including failed trials. Steps are means, TTFT/process time are medians in seconds, RSS is maximum Linux wait4 peak MiB.

| Scenario | Pass / 5 | Steps | TTFT s | Process s | Peak RSS MiB |
|---|---:|---:|---:|---:|---:|
| largest-files | 5 -> 5 | 2.0 -> 2.2 | 5.71 -> 5.37 | 15.73 -> 15.65 | 2382.90 -> 2397.83 |
| listening-port | 5 -> 4 | 2.0 -> 2.2 | 5.38 -> 5.76 | 12.04 -> 11.51 | 2362.61 -> 2380.20 |
| language-lines | 1 -> 0 | 3.6 -> 6.4 | 5.56 -> 5.59 | 40.73 -> 35.43 | 2409.88 -> 2408.57 |
| rename-files | 5 -> 5 | 3.2 -> 3.2 | 5.31 -> 5.61 | 18.40 -> 17.94 | 2383.77 -> 2402.76 |
| chinese-python | 4 -> 2 | 3.8 -> 8.4 | 5.24 -> 5.48 | 23.44 -> 33.41 | 2367.25 -> 2398.53 |
| typo-correction | 5 -> 5 | 0 -> 0 | N/A -> N/A | 0.064 -> 0.010 | 17.68 -> 20.67 |
| explain-failure | 4 -> 3 | 3.0 -> 2.2 | 5.26 -> 5.17 | 30.61 -> 18.73 | 2365.89 -> 2396.80 |
| summarize-git-log | 5 -> 5 | 1.0 -> 1.0 | 8.45 -> 9.25 | 32.72 -> 23.75 | 2423.92 -> 2453.39 |
| suggest-archive | 2 -> 5 | 1.0 -> 1.0 | 2.16 -> 1.54 | 6.99 -> 4.32 | 2355.93 -> 2323.86 |
| persistent-cwd | 0 -> 0 | 2.0 -> 2.0 | 5.07 -> 5.63 | 10.34 -> 10.38 | 2395.39 -> 2362.24 |

Overall: **72% -> 68%**, model-only **31/45 -> 29/45**. Mean steps **2.16 -> 2.86** (median 2 -> 2); TTFT median **5.33 -> 5.58s** over 45 model trials; process-time median **17.79 -> 16.40s** over all 50; maximum RSS **2423.92 -> 2453.39 MiB**. Faster aggregate process time and suggestions do not offset the recorded correctness regressions or prove a general latency improvement. Baseline repeat 1 was 37/50, with TTFT 4.70s and process time 16.33s, showing the sensitivity of these small-sample timing comparisons. There was no v5 repeatability measurement.

## Paired changes, tool trajectories and fallbacks

| Pair | Change | Audited evidence |
|---|---|---|
| listening-port / seed 3 | pass -> fail | Issued `netstat ...` then `lsof -i:8080`. The first returned exit 1; the **second succeeded and printed the correct Python PID**, but the answer falsely claimed no listener. This is failure to use successful fallback evidence, not a listener/fixture failure. |
| language-lines / seed 2 | pass -> fail | Read all six files. Split Python into 10-line and 5-line groups, then repeated both as separate Python entries under "Combined totals." The unchanged conservative grader does not accept these as one unambiguous per-language total. No manual regrading was applied. |
| chinese-python / seed 1 | pass -> fail | Listed all relevant directories, then made eight content-search calls using filename-like patterns. Responses correctly reported no matches. The final Python list was correct, but only after the step-limit summary; incomplete-task grading remains unchanged. |
| chinese-python / seed 4 | pass -> fail | Repeated the nonexistent `scripts/src` path seven times, reached the step limit and omitted `tools/report.py`. Seed 0 has the same failure pattern but was already failing in baseline. |
| explain-failure / seed 1 | pass -> fail | Used **no tools** and guessed syntax/import errors from an exit-code summary, instead of inspecting `broken.py`; omitted the actual missing `config.json`. |
| suggest-archive / seeds 1, 2, 3 | fail -> pass | Direct single-program suggestions, zero model tool calls, no automatic execution; all five v5 archive suggestions passed the existing restricted tar verifier. Historical suggestion sampling differs. |

No hidden success selection: the JSON audit contains all 50 paired answers, reasons, metrics, approvals and complete before/after tool-call sequences.

First-tool counts: largest-files and listening-port use `run_command` 5/5 each; language-lines, rename-files, chinese-python and persistent-cwd use `list_dir` 5/5 each; explain-failure uses `read_file` 3/5, `list_dir` 1/5 and no tool 1/5. Git-summary, suggestions and local correction use no tools. These trajectories are observations, **not required-tool grading rules**.

The new `search` tool was called ten times across chinese-python seeds 1 and 3. Most calls confused content matching with filename discovery. Seed 3 recovered from `.py$` yielding no matches to `import|from` finding the actual import line, then read the three Python files and passed; seed 1 exhausted the step budget. This provides real interface evidence but **does not establish reliable model search selection**.

Other failures are retained: language-lines seeds 0/4 looped until the step limit; seeds 1/3 did not provide valid per-language counts. Failure-explanation seed 3 hit the unchanged denied-approval path and was incomplete in both baseline and v5, even though its final explanation mentioned the missing file. **Persistent-cwd is 0/5 in both versions**: each only called `list_dir("data")`; no actual `cd` occurred, despite some final answers claiming otherwise. This is an existing model limitation, not a new shell-state regression. Unit tests still verify real shared-shell changes when commands execute.

## Exact static prefix tokens

Measured before inference using actual `Tok` plus `template::render_system`, then checked against **every observed open-session prompt/schema** in the 45 model trials. Counts include BOS/system/tool definitions, not dynamic task input or generation:

| Mode | Formal baseline | v4 | v5/general |
|---|---:|---:|---:|
| Full | 771 | 1000 | **861** |
| Suggest | 301 | 114 | **114** |
| ReadOnly | Not remeasured here | Not remeasured here | **784** |

Full is **139 tokens / 13.9% smaller than v4**, still **90 / 11.7% larger than baseline**; Suggest is **187 / 62.1% smaller than baseline**. Full raw-system text is 296 tokens; Suggest 108. Search schema and the expanded fixed installed-command candidates have a real prefix cost; no claim of Full shrinking below baseline.

## Provenance, verification and resources

[`v5-general.tar.gz`](v5-general.tar.gz) preserves the untouched report JSON/Markdown and all **50 stdout/stderr/transcript sets plus 45 engine traces**, together with build-info, prefix measurement, both preflights and the measurement/runner/audit scripts. [`v5-audit.json`](v5-audit.json) is the machine-readable full paired audit. [`manifest.json`](manifest.json) records binary, archive, raw-report and audit SHA-256 hashes. Formal baseline files and v1-v4 archives were not overwritten. No sample was retried, dropped or reclassified; exit code 1 denotes recorded judgment failures, not an infrastructure error.

Measured source `2c43aa9` passed local `cargo fmt --all -- --check`, workspace/all-targets clippy `-D warnings`, workspace locked tests and **41 Python tests**. Its [three-platform PR CI](https://github.com/NewFuture/nosh/actions/runs/36139625674) passed Linux x64 (2m09s), Linux ARM64 (1m48s) and macOS (1m41s). Later report-only commits do not change the measured source; final-head checks are recorded on the PR. Both earlier review findings remain fixed/resolved. All #16/main compatibility changes remain.

The coordinator reserved a serial build/evaluation window. Immediately before inference, the four existing user REPLs had 5s CPU deltas 3/0/0/0 ticks, available memory 21,678,452 KiB (20.67 GiB), swap zero/no growth, and no other cargo/rustc/nosh jobs. After completion, no task-owned build/inference/eval processes remained, available memory was 21,098 MiB, swap was still zero, and all four user REPLs were preserved. The resource window was released before offline reporting. No further model runs are planned under this request.
