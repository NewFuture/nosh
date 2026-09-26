# Expanded main baseline summary

**120/250 pass (48.0%)**, **4.88 mean steps**, **1.028 mean confirmations**.
All 250 planned identities are accounted for: 25 scenarios, seeds `[0, 1, 2, 3, 4]`, two serial repetitions.

This is a compact index, not the full per-trial report.
Complete reports, original judgments, diagnostic evidence and standalone replay are in the
[issue #4 archive](https://github.com/NewFuture/nosh/issues/4#issuecomment-5844795358).
The [archive manifest](archive.json) pins the download and its SHA-256.

| Group | Pass / planned | Pass rate | Mean steps | Mean confirmations |
|---|---:|---:|---:|---:|
| All scenarios | 120 / 250 | 48.0% | 4.880 | 1.028 |
| Original 10 tasks | 80 / 100 | 80.0% | 2.100 | 0.100 |
| Added 15 tasks | 40 / 150 | 26.7% | 6.733 | 1.647 |
| Model tasks only | 110 / 240 | 45.8% | 5.083 | 1.071 |
| Local correction | 10 / 10 | 100.0% | 0 | 0 |

| Scenario | Pass / planned | Mean steps | Mean confirmations |
|---|---:|---:|---:|
| largest-files | 10 / 10 | 2.1 | 0 |
| listening-port | 9 / 10 | 2.4 | 0 |
| language-lines | 1 / 10 | 3.5 | 0 |
| rename-files | 10 / 10 | 3.2 | 1 |
| chinese-python | 9 / 10 | 3.0 | 0 |
| typo-correction | 10 / 10 | 0 | 0 |
| explain-failure | 9 / 10 | 2.8 | 0 |
| summarize-git-log | 10 / 10 | 1.0 | 0 |
| suggest-archive | 10 / 10 | 1.0 | 0 |
| persistent-cwd | 2 / 10 | 2.0 | 0 |
| zh-rust-build | 3 / 10 | 8.1 | 3.1 |
| zh-node-build | 0 / 10 | 10.7 | 2.1 |
| zh-rust-test | 2 / 10 | 7.5 | 1.0 |
| zh-node-test | 0 / 10 | 8.7 | 1.2 |
| zh-python-test | 0 / 10 | 7.4 | 1.8 |
| zh-git-diff | 0 / 10 | 6.6 | 0 |
| zh-git-commit | 4 / 10 | 7.4 | 1.9 |
| zh-git-log | 4 / 10 | 2.1 | 0 |
| zh-listening-port | 10 / 10 | 2.0 | 0 |
| zh-clean-build | 0 / 10 | 7.0 | 0.2 |
| zh-tool-versions | 9 / 10 | 2.0 | 0 |
| zh-clarify-task | 0 / 10 | 8.0 | 2.7 |
| zh-build-failure | 2 / 10 | 7.1 | 0.9 |
| zh-test-failure | 6 / 10 | 5.9 | 1.1 |
| zh-port-failure | 0 / 10 | 10.5 | 8.7 |

Means include failures; steps and confirmations each have 250 measured samples.
The raw campaign was **120 pass / 129 fail / 1 error**. Deterministic correction of
one proven model-generation timeout gives **120 pass / 130 fail / 0 errors**;
the successful-trial count did not increase and no seed was resampled.

Verdict plus final state agrees in **107/125** pairs; state alone in **119/125**.
This does not satisfy issue #3's repeatability acceptance.

Measured main: `78b7e509ad0d6d71ce50397cfa9e9f2187b0db75`.
The [compact JSON](summary.json), [build information](build-info.json),
[provenance](provenance.json) and [analysis](analysis.md) distinguish source,
evaluator and normalization versions. The provenance's report hashes refer to
the full files inside the archive, not this summary.
