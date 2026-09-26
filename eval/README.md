# 真实模型评测

25 个场景，每场景固定 seeds `[0, 1, 2, 3, 4]`。单轮 125 次试验，双跑 250 次；每轮包含 5 次不加载模型的本地纠错。事实/状态与适用的体验门槛都通过，任务才算通过。模型评测只在本地显式运行或手动触发，不进入每个 PR 的 CI。

## 本地运行

需要 Linux/WSL（内核 5.3+，支持 pidfd）、Python 3.11+、构建好的 nosh、已下载的模型，以及 `git`、`bash`、`python3`、`tar`、`ss`。Rust 夹具另需 Cargo/rustc/`cc`，Node 夹具另需 Node.js 22+/npm；按选定场景检查工具。没有第三方 Python 依赖，项目夹具也不需要下载依赖。

在 Linux/WSL 仓库根目录执行：

```bash
python3 -m eval.run --model-path MODEL_DIR

# 同构建、同机器双跑
python3 -m eval.run --model-path MODEL_DIR --repeat 2

# 选择场景、seed、构建及比较对象
python3 -m eval.run --model-path MODEL_DIR --binary NOSH_BINARY \
  --scenario zh-rust-build --scenario zh-node-test --seeds 0 1 \
  --build-info BUILD_JSON --compare PREVIOUS_REPORT_JSON

# 旧构建必须显式选择 legacy；新增执行证据判定不支持它
python3 -m eval.run --model-path MODEL_DIR --binary OLD_NOSH --legacy \
  --scenario largest-files --build-info BUILD_JSON
```

默认输出为 `eval/results/<run-id>/report.json` 和 `report.md`。`--output` 必须是尚不存在的目录；`--label` 只是名称，不证明构建来源。默认推理线程 8、Rayon 1、单次试验超时 240 秒；可用 `--threads`、`--timeout` 调整。模型和工具不会自动安装。

退出码：**0** 全部通过且重复结果一致，**1** 判定失败/不一致，**2** 基础设施或观测错误，**130** 中断。失败、超时和未完成试验不从计划分母中消失。

## 手动 GitHub Actions 基线

独立的 [Evaluation 工作流](../.github/workflows/eval.yml) 不占用本地 CPU/RAM，也不会被常规 CI 的 push 取消：

```bash
gh workflow run eval.yml --repo NewFuture/nosh --ref EVALUATOR_BRANCH \
  -f source_revision=FULL_MAIN_SHA
```

新工作流首次使用前需合入默认分支。`source_revision` 是 main 上的完整 SHA；留空则在作业开始固定 main。工作流归档干净源码构建，显式下载并校验模型，在同一个 Ubuntu runner 上以 **threads 2 / Rayon 1 / nice 10** 串行双跑。

运行时使用 `--ref` 的评测器，分别记录 main、评测器和模型来源。首个检查点在 5 分钟后保存，此后每 45 分钟一次，最多八份、保留一天；最终 artifact 为 `evaluation-<run-id>-<attempt>`。检查点复制已结束试验，不改判定或 seed，也不能跨机器拼接成正式双跑。

**工作流绿色不等于模型全通过**：完整采样、无运行器错误即可成功；模型失败及原始退出码仍保留。runner 失联时保留已有证据，修复后完整重跑，不选择性重抽失败 seed。快照复制/压缩可能影响耗时，比较时需注明。

## 场景与判定

[`scenarios.json`](scenarios.json) 是唯一的场景配置。v2 声明输入、夹具、审批、判定器、`completions` 和 `expect`；仍支持 v1 场景及旧报告。REPL 输入原样发送，失败求助先观察真实退出码和诊断，不插入会覆盖“上次失败命令”的探针。

| 覆盖 | 场景 | 主要证据 |
|---|---|---|
| 原 10 个任务 | 大文件、监听端口、语言行数、改名、中文 Python 文件、本地纠错、失败解释、git 摘要、归档建议、cwd 连续性 | 最终回答中的事实、文件状态、审批记录、物理 cwd；纠错/建议不自动执行 |
| 中文构建/测试 | Rust/Node `编译`，Rust/Node/Python `跑测试` | 真实命令退出码、完整测试集、构建产物与未改动的源码 |
| 中文 git | `看看改了什么`、`提交改动`、`最近的提交` | staged/unstaged、commit parent/tree/index、提交事实与顺序 |
| 其他中文短指令 | 8080 端口、清理构建产物、工具版本、歧义对照 | 真实 PID、受保护文件、实际版本查询、必要澄清 |
| 执行失败后求助 | Rust 编译错误、Python 断言失败、端口冲突 | 实际失败现场、原因和修复方法，不要求自动改代码 |

新增单轮中文请求都不带 `#`，不替换成 `-a`。Rust、Node、Python 和 dirty git 都是可运行的无外部依赖小项目；原混合语言夹具保持不变。

`expect` 必须包含：

| 字段 | 含义 |
|---|---|
| `max_steps` | engine step 上限，含总结，不是命令数；原样记录超出预算的执行 |
| `max_confirmations` | 实际审批请求上限，拒绝也计数 |
| `response_language` | `zh` / `any` / `not_applicable` |
| `final_question` | `forbid` / `require` / `not_applicable`；仅歧义对照要求澄清 |

具体门槛直接见场景文件：普通只读查询 3–4 步，构建/测试 4 步，提交 5 步，诊断 6 步；只读/版本不确认。正式采样前冻结门槛，不因模型表现不佳放宽。

事实裁判使用最终回答、实际执行结果和状态，不把输入回显或工具输出直接当作正确回答。新执行类场景要求 native trace：未执行的工具调用、已有产物、只编译不跑测试都不算成功。构建必须保留源码，清理不能删配置，提交必须覆盖正确改动且不增加多余提交。语言行数、列表作用域等保守规则和正反例见 [checks.py](checks.py) 与[测试](tests/)。

语言和收尾是确定性启发式，不用另一模型裁判。剔除代码、引用诊断、路径、URL、版本号和工具名后，中文正文至少两个汉字，且汉字数大于残余拉丁词数；收尾识别末段提问/继续确认，区分“请检查是否需要更新”这类建议。JSON 保留抽取正文、统计及原因，供复核。纠错和 `-s` 纯命令输出不检查回答语言/收尾。

## 审批与隔离

保持 **confirm**，不用 auto/yolo 或“本会话同类放行”。[审批模块](approval.py) 只允许声明任务的有限命令：

- 改名与 cwd 保留固定、可验证的命令形式。
- 项目操作只允许对应 Cargo 命令、已知 npm/Node 脚本、完整 unittest、有限 git add/commit；源码与脚本内容必须仍匹配夹具。
- 可组合受限的 `cd`、pwd、ls、cat。允许本夹具 Cargo.toml 和解析后仍指向已记录工具的链接。
- 唯一重定向例外是未加引号的命令末尾 `2>&1`；外部路径、替换脚本、任意解释器代码、文件重定向、管道、`|| true`、push 等不放行。
- `-s` 只验证严格解析的 tar 子集，不把任意模型建议交给 shell 执行。

每个试验使用新进程、私有 HOME/NOSH_HOME、固定 locale/TZ/PATH 和终端大小，显式传入 `--norc --offline --no-download --seed`。预检记录工具路径、版本、哈希；私有 `HOME/bin` 接入已解析的真实工具，Rustup shim 不依赖试验 HOME。Cargo/npm 缓存隔离且离线，Rust 构建单 job、关闭 incremental。

工作目录默认 `/tmp/nosh-eval-<uid>`，以所有权标记和独占锁保护。自定义 `--work-dir` 必须属于当前用户、0700、无符号链接祖先，位于仓库/NOSH.md 祖先之外，且不能包含二进制、模型或报告。只回收本次创建的目录和进程；端口只绑定 loopback 8080，外部服务占用时明确报错，不抢占或终止它。

**临时目录不是安全沙箱。** 建议在评测专用用户或受限容器中运行。模型权重、个人配置、大日志不入库。

## 指标与复现

| 指标 | 口径 |
|---|---|
| 通过率 | 通过数 / 计划试验数；同时报告全部、原任务、新任务、模型与本地纠错 |
| 步数/确认 | 逐次有效样本的均值，包含失败；不对场景均值简单再平均 |
| TTFT | 首步 Usage 的首 token 延迟，不含加载；汇总为中位数 |
| 总耗时 | 进程启动至退出，含加载/交互/退出，不含夹具准备与独立验证；汇总为中位数 |
| RSS | Linux wait4 峰值 MiB，含内核对已等待后代的统计，不是进程树求和；汇总取最大值 |

native 观测由 `NOSH_EVAL_TRACE` 的私有版本化 JSONL 提供；缺失/损坏时不自动降级。legacy 的 `-s` TTFT 为 N/A，旧 REPL 显示精度有限，差异记录在报告中。必须观测不到的数据不填 0。

启动、审批/终端协议、退出阶段超时属于观测/基础设施错误。仅当 native trace 能证明此前步骤完整、恰有一个模型步骤仍在生成、无引擎错误且由驱动结束时，预算耗尽才归为任务失败；保留已开始步骤数，不伪造最终回答或未完成步骤的 TTFT。

报告保存模型、二进制、工具、场景和评测器哈希及原文证据。`--build-info` 需要 schema v1、完整 `source_revision`、匹配的 `binary_sha256`；没有时注明来源未验证，不把当前 checkout 当作被测构建。

双跑比较**判定和最终状态**，输入/回答/工具差异另列。固定 seed 不固定任务时间、命令耗时或 PID；新进程只保证冷会话/KV，不保证冷 OS 页缓存。不同数据集、评分、模型、主机、工具链或参数的配对差值只是描述，不是受控回归结论。

## 基线生命周期

| 基线 | 范围与结果 | 证据 |
|---|---|---|
| main `78b7e50` | 25 × 5 × 2；120/250（48.0%），平均 4.88 步/1.028 次确认；模型 110/240 | [报告](baselines/main-78b7e50-expanded/report.md)、[来源与分析](baselines/main-78b7e50-expanded/analysis.md) |
| main `4f602ab` | 原 10 场景双跑；原始 70/100，经透明判定修正为 73/100 | [原生观测基线](baselines/main-4f602ab/report.md)、[分析](baselines/main-4f602ab/analysis.md) |
| main `7c57a88` | 原 10 场景单轮 legacy；36/50 | [有限观测基线](baselines/main-7c57a88/report.md) |

新基线的全部 250 个身份保留。原始 120/129/1/0（通过/失败/错误/缺失），仅确定性恢复一条真实模型超时的指标并改为失败，另去掉一条不影响通过数的收尾误判，归一化为 120/130/0/0；没有重抽 seed。失联和过窄审批规则的前次运行也保留说明及可得证据。

判定加状态仅 **107/125** 对一致，最终状态 **119/125** 对一致，#3 的复现要求仍未满足。历史数据不改写；[复核脚本](baselines/main-78b7e50-expanded/reproduce.py) 从 Git 提取固定处理版本，在隔离临时目录中重建报告，不依赖当前评测实现，也不运行模型。

## 开发与无模型自测

| 模块 | 职责 |
|---|---|
| `run.py` | CLI、工具预检、环境/来源、逐次执行与保存 |
| `suite.py` | v1/v2 契约与场景/夹具/审批兼容性 |
| `driver.py` / `observations.py` | PTY/进程生命周期；原生/legacy 观测解码 |
| `fixtures.py` / `approval.py` | 夹具、隔离和允许变化；受限命令解析/审批 |
| `checks.py` / `report.py` | 事实/体验判定；统计、比较、JSON/Markdown |
| `checkpoint.py` | 复制原子报告与已结束试验日志，不改实时结果 |

```bash
python3 -m unittest discover -s eval -v
python3 eval/baselines/main-78b7e50-expanded/reproduce.py --check
```

测试使用真正的无依赖小项目构建/测试、脚本化 PTY、协议正反例和已保存的基线，不加载模型。CLI 观测的 Rust 回归可运行 `cargo test -p nosh-cli --locked`。无模型测试不能替代真实模型基线。
