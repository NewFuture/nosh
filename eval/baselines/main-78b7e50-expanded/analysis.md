# Expanded baseline: sources and archived evidence

The baseline remains **120/250 pass (48.0%)**, **4.88 mean steps**, and **1.028 mean confirmations**.
The repository retains a [readable summary](report.md), [compact metrics](summary.json),
[build information](build-info.json) and [provenance](provenance.json).
Full reports and one-off material are indexed in [issue #4](https://github.com/NewFuture/nosh/issues/4#issuecomment-5844795358).

## Archive

The [evidence ZIP](https://github.com/NewFuture/nosh/releases/download/eval-main-78b7e50-evidence/nosh-eval-main-78b7e50-evidence.zip)
is **1,159,368 bytes**, SHA-256 **`c417c0dba5a6ec96e5d256771a6e54188f3baae4053cd602abc8ed25a96f6764`**.
[archive.json](archive.json) pins its location, size, expanded size and manifest hash.

GitHub's token-authenticated issue attachment endpoint rejected ZIP uploads.
The file is therefore hosted as a non-latest, evidence-only Release asset, with a permanent issue comment linking it;
it is not a product release or a short-lived Actions artifact.

The ZIP preserves every pre-migration baseline file byte-for-byte under `baseline/`:
the diagnostic, raw and normalized JSON, detailed Markdown, original analysis,
provenance, timeout trace/transcript and replay script.
It also includes the exact normalization modules and a standalone verifier.
The public download, all 22 files, and full replay were verified before the Git copies were removed.
No old commit was rewritten; earlier Git history still contains the previous files.

## Source and interpretation

Main: `78b7e509ad0d6d71ce50397cfa9e9f2187b0db75`;
runtime evaluator: `581c75a8e0032a5bf9b55eaf5f63b0c0996d2751`;
normalization: `a26d1b2ddf1d06dfcb1bdda2bdae8ddb6922cf12`.
[Run 36203738656](https://github.com/NewFuture/nosh/actions/runs/36203738656) sampled all 250 trials on one
GitHub-hosted Ubuntu runner, using threads 2 / Rayon 1 / nice 10.

Raw results were **120 pass / 129 fail / 1 error**. Two deterministic corrections are recorded in provenance:
one declarative closing-advice reason was removed without changing its failed verdict, and one proven
in-flight generation deadline was classified as a failed task with seven observed started steps.
No final answer or unfinished-step TTFT was invented. Original judgments and all prior evidence remain in the archive.

The earlier lost-runner attempt has no recoverable report. A subsequent complete diagnostic campaign
scored 107/250 but had an overly restrictive approval parser; its 250 records are archived separately,
not mixed into the accepted baseline. Neither that run nor the final corrections used selective seed retries.

Model tasks pass **110/240**; 51 trials have correct facts/state but fail experience gates.
Verdict and state agree in **107/125** pairs, state alone in **119/125**; issue #3 remains unresolved.
Changed hardware, thread count, grading and checkpoint activity preclude controlled comparisons with older WSL baselines.
The full archived analysis contains the failure breakdown and measurement caveats.

## Offline verification

Download the ZIP from `archive.json`, then:

```bash
python3 eval/baselines/main-78b7e50-expanded/reproduce.py --archive PATH_TO_ZIP --check
```

The command validates the outer hash, archive paths and individual files before running the archived verifier
in an isolated temporary directory. It compares the compact metrics and regenerates the full normalized reports byte-for-byte.
No Git history, network, model or project dependencies are needed; routine CI never downloads the archive.
After checking the ZIP hash, `python3 -I -B verify.py` also works directly in an extracted archive.

`summary.json` is deliberately not a full eval report. For `eval.run --compare`, use the archive's
`baseline/report.json` after verification. Recorded provenance hashes refer to those archived full reports.
