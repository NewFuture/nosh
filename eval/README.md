# 真实模型评测

评测分成 **Agent 回归**、**CommandAssist 专项**和小规模 **smoke**，共享驱动、观测、评分和报告。场景只在 `scenarios/` 定义一次，`suites/` 显式选择场景 ID 和运行参数。回归与专项仍为 `dataset_revision: 10`、固定 seeds `[0, 1, 2, 3, 4]`，目录拆分不修改旧场景或历史报告。

| suite | 范围 | 单轮／双跑 |
|---|---|---|
| `suites/regression.json`（默认） | 原 27 场景回归，含本地纠错和 CLI 命令生成；显式关闭完成事件辅助，避免混入 Agent 统计 | 135／270 |
| `suites/command-assist.json` | Generate、查询帮助后生成、必要澄清、自动 Fix、自动 Next 无建议 | 25／50 |
| `suites/smoke.json` | 本地纠错、Agent、Generate、Fix、Next 的五个既有场景，seed `[0]` | 5／10 |

事实/状态与适用的体验门槛都通过，任务才算通过。模型评测只在本地显式运行或手动触发，不进入每个 PR 的 CI。

## 本地运行

需要 Linux/WSL（内核 5.3+，支持 pidfd）、Python 3.11+、构建好的 nosh、已下载的模型，以及 `git`、`bash`、`python3`、`tar`、`ss`。Rust 夹具另需 Cargo/rustc/`cc`，Node 夹具另需 Node.js 22+/npm；按选定场景检查工具。没有第三方 Python 依赖，项目夹具也不需要下载依赖。

在 Linux/WSL 仓库根目录执行：

```bash
python3 -m eval.run --model-path MODEL_DIR

# 同构建、同机器双跑
python3 -m eval.run --model-path MODEL_DIR --repeat 2

# 独立验证命令辅助；自动 Fix/Next 由真实用户命令完成事件触发
python3 -m eval.run --suite eval/suites/command-assist.json --model-path MODEL_DIR --repeat 2

# 小规模检查，不代替回归或专项基线
python3 -m eval.run --suite eval/suites/smoke.json --model-path MODEL_DIR

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
  -f source_ref=SOURCE_BRANCH -f source_revision=FULL_SOURCE_SHA

# 相同构建／来源流程，切换为命令辅助专项
gh workflow run eval.yml --repo NewFuture/nosh --ref EVALUATOR_BRANCH \
  -f source_ref=SOURCE_BRANCH -f source_revision=FULL_SOURCE_SHA -f suite=command-assist
```

工作流的 `suite` 可选 `regression`（默认）、`command-assist`、`smoke`，对应 `eval/suites/` 中的清单。

新工作流首次使用前需合入默认分支。`source_ref` 默认 `main`，也可显式指定本仓库的待验收分支；`source_revision` 必须是该分支可达的完整 SHA，留空则在作业开始固定该分支。分支名与 SHA 均校验，不接受 Git 表达式代替固定版本。工作流归档干净源码构建，显式下载并校验模型，在同一个 Ubuntu runner 上以 **threads 2 / Rayon 1 / nice 10** 串行双跑。

Hosted 工作流显式使用 **每次试验 60 秒**期限，本地默认仍为 **240 秒**。270 个试验即使全部耗尽期限，试验时间也为 270 分钟，六小时 job 留有 90 分钟用于构建、夹具、检查点和上传；构建前还会按实际场景、seed、重复次数检查预算，至少预留 90 分钟非试验时间，超出直接报错。准备阶段另设超时，不把正常的最坏评测计划留给 job 强制截断。

这不是静默缩短正式采样：有效 `timeout_s` 保存在报告与工作流来源中，与本地/历史 240 秒运行比较时会产生设置差异警告。需要 240 秒期限的完整基线应在可提供足够运行窗口的本地/受控主机上执行；不能直接提高 hosted 时限而跳过预算检查。

运行时使用 `--ref` 的评测器，分别记录被测源码分支/SHA、评测器和模型来源。首个检查点在 5 分钟后保存，此后每 45 分钟一次，最多八份、保留一天；最终 artifact 为 `evaluation-<run-id>-<attempt>`。检查点复制已结束试验，不改判定或 seed，也不能跨机器拼接成正式双跑。完整性检查逐一核对声明的场景/seed/重复身份，不以过时的固定数量判断完成。

**工作流绿色不等于模型全通过**：完整采样、无运行器错误即可成功；模型失败及原始退出码仍保留。runner 失联时保留已有证据，修复后完整重跑，不选择性重抽失败 seed。快照复制/压缩可能影响耗时，比较时需注明。

## 场景与判定

[`scenarios/agent.json`](scenarios/agent.json)、[`scenarios/command_assist.json`](scenarios/command_assist.json)、[`scenarios/shell.json`](scenarios/shell.json) 保存 32 个唯一场景定义。场景保留输入、夹具、审批、`completions`、`expect`、`capture_output` 和可选 `assistance`，不按平台、语言或 seed 复制。

[`suites/regression.json`](suites/regression.json) 与 [`suites/command-assist.json`](suites/command-assist.json) 分别引用原回归和专项场景；[`suites/smoke.json`](suites/smoke.json) 只选已有场景，不另造评分标准。清单的 `catalogs` 使用相对清单文件的显式路径，`scenarios` 是有序 ID 列表。不使用隐式 glob 或按目录排序决定试验顺序；重复定义／选择、未知 ID、缺失文件、非法字段均报错。

加载后展开成原来的 schema v2 完整场景结构，再执行统一校验。报告保存展开后的场景与 `suite_sha256`，因此单纯移动定义不改变场景哈希；自包含 v1/v2 suite 仍可用于历史与外部数据。运行器／评分器哈希覆盖 `checks/` 下全部 Python 模块，不把测试和历史裁判归档算作当前运行时代码。

revision 10 的协议变化：裸 `#`／`ai fix` 现在生成修复建议，因此旧缺文件解释场景改为显式 `ai fix <question>`，保持诊断目标；归档建议允许最多 4 个模型步以覆盖查询循环，不再假定单轮无工具。其他原场景的目标、审批和预算保留。Agent 的 System context＋独立 User 请求、分项采集评分和确定性 `diagnostic_id` 不变；历史结果不重评。

自动专项用 `completions: [{"kind":"assist"}]` 等待原生 trace 中的宿主结果，不用固定 sleep 或猜测屏幕文字判断完成。`tool_choice` 记录单步 Required／Named 解码策略，宿主预填的调用开头计入输入成本。`SessionSpec.label` 标记意图和前台／后台，不进入 prompt。模型发出 `finish` 不代表成功：只有经过 harness 校验的 `observation` 才能作为 command／clarify／none 结果，错误和取消不能折叠成 none。`finish` 不计为工具执行；帮助查询必须观测到实际返回，不以一个未执行的查询调用通过。归档只在评分器的严格 tar 子集内验证，并检查 nosh 本身未修改夹具。

等待自动结果时增量读取 JSONL，每个完整记录只解析一次；跨写入的 UTF-8 和未完成行留待后续读取，截断、消失或损坏显式报错。该优化不改变模型输入、轮询完成条件、seed 或评分；推理仍占主要耗时，线程数通过 `--threads` 显式设置并记录，不靠缩短任务期限提速。

接受结果需同时匹配会话身份、执行 command ID／退出状态和该步实际生成的唯一 finish。`-s` 的 stdout 必须与已接受命令一致，clarify／none 的 stdout 必须为空；不能用 trace 覆盖错误的 CLI 输出。帮助查询只有返回 `exit=0` 才满足成功查询要求。

失败诊断场景必须先声明一个带退出码和诊断特征的 `shell` 输入，再进入 agent 求助；缺少这一前置步骤会在预检时报错，而不是开始运行后才崩溃。

| 覆盖 | 场景 | 主要证据 |
|---|---|---|
| 原 10 个任务 | 大文件、监听端口、语言行数、改名、中文 Python 文件、本地纠错、失败解释、git 摘要、归档建议、cwd 连续性 | 最终回答中的事实、文件状态、审批记录、物理 cwd；纠错/建议不自动执行 |
| 中文构建/测试 | Rust/Node `编译`，Rust/Node/Python `跑测试` | 真实命令退出码、完整测试集、构建产物与未改动的源码 |
| 中文 git | `看看改了什么`、`提交改动`、`最近的提交` | 改动事实、commit parent/tree/index、提交事实与顺序 |
| 其他中文短指令 | 8080 端口、清理构建产物、工具版本、歧义对照 | 真实 PID、受保护文件、实际版本查询、必要澄清 |
| 执行失败后求助 | Rust 编译错误、Python 断言失败、端口冲突 | 实际失败现场、原因和修复方法，不要求自动改代码 |
| 一次性错误诊断 | `captured-diagnosis` | 首次执行的真实诊断进入第一个模型请求，回答解释 REGION 配置问题并给出合理修复，执行计数仍为 1 |
| 诊断标识引用 | `captured-diagnostic-id` | 显式要求引用 `diagnostic_id`，不得以 `error_code` 或进程退出码代替；诊断要求不变 |

两个采集场景显式声明 `capture_output: "last"`，并通过真实 CLI 配置启用。其他场景未声明时继承被测二进制的默认值，报告用 `settings.capture_output = "binary_default"` 记录这一策略；当前二进制默认 `last`，历史默认关闭的构建仍按其旧行为运行。需要控制变量时可为场景显式设置 `off`；不默默替换用户默认值，也不向没有覆盖项的旧构建写入新键。

夹具明确输出 `diagnostic_id: CAPTURE-...`、`error_code: REGION_UNSET`、报错文本与 `exit_code: 17`；诊断标识由完整 trial 身份的 SHA-256 前缀确定，不依赖运行时随机数，也不预置在用户输入中。第二次执行不再产生原诊断。场景要求 native trace，终端回显、后续工具读取、错误命令 ID、空/混流/不可用记录均不能冒充首请求证据。两个场景预算均为 3 步、0 次确认，不因结果较差放宽。

评分在 `grading.facts.components` 中分别记录 `capture`（证据、关联、单次执行）、`diagnosis`（原因、修复及已覆盖的无依据断言）、`citation`（仅专门引用场景要求）。普通诊断不必复述 trial 专用标识；引用场景漏标识仍失败，但不能把它归为采集失败。报告逐项列出通过、失败、N/A 和未观测数量。无依据的 Kubernetes/内容审核用途、已覆盖的退出码臆测等有正反例约束；这些规则不是完整语义真实性证明，不把“未检出已知错误”夸大为所有解释均正确。

早期试验数据只读留存，不重新标记此前 6/10、9/10 等定向结果；当前评分器不再支持旧采集入口和消息形态。

可单独运行新诊断场景，结果写入新的目录，不覆盖任何历史基线：

```bash
python3 -m eval.run --model-path MODEL_DIR --binary NOSH_BINARY \
  --scenario captured-diagnosis --scenario captured-diagnostic-id --repeat 2
```

零输出、截断、混流、全屏、信号和 100 MiB 有界缓存由无模型真实 PTY 回归覆盖，不把这些测试计为真实模型通过率。采集不新增日常输出日志；本评测显式开启的 `NOSH_EVAL_TRACE` 仍记录输入证据，遵守私有文件和来源记录契约。

新增单轮中文请求都不带 `#`，不替换成 `-a`。Rust、Node、Python 和 dirty git 都是可运行的无外部依赖小项目；原混合语言夹具保持不变。

revision 2 修正了任务的隐含要求，而不是按历史失败结果放宽分数：

| 任务 | 明确的完成条件 |
|---|---|
| 提交改动 | 使用只有预期已暂存改动的夹具，不混入范围不明的未暂存修改；仍要求正确提交内容、parent 和干净状态 |
| 查看改动 | 准确说明文件及变化即可；暂存标签可以省略，显式说错或否认真实改动仍失败 |
| 行数统计 | 区分逐文件明细与语言总计，接受正确的完整表格；错误计数、重复/遗漏和矛盾汇总不能通过 |
| 需求澄清 | 请求缺失的任务目标即可，例如“请说明具体目标，我再处理。”；不要求问号，也不把泛泛的继续操作建议当作澄清 |
| 构建、测试、失败诊断 | 允许夹具内合理的受限执行路径；额外步骤/确认仍由独立体验门槛计分，不用人为拒绝制造失败现场 |

当前回归保留 27 场景、seed、审批和以下解释规则；revision 10 的入口与归档建议预算变化见上文：

| 修正 | 边界 |
|---|---|
| tar 传统选项 | 支持 `tar czf` 等受限形式；按操作数位置解析 `-C`，只允许归档 logs；仍校验 gzip 和全部内容，不执行任意 shell |
| 版本查询 | 识别受限重定向和字面后备提示，仍要求成功执行及精确输出；横／纵表格按列关联，空值、未安装和矛盾版本不能被另一正确值掩盖；比较约束不冒充安装版本 |
| 行数占比 | 区分“某语言占总共 N 行中的 M 行”的分量与总量，二者都校验；矛盾或错误数字仍失败 |
| Python 文件作用域 | “没有／无 Python 文件”不污染后续目录；数量按同一分句的显式目录或当前标题计算，递归范围需明确；临时目录段落与 Markdown 标题分别处理 |
| 进程名称 | 正确 PID 加 Python 或 Python 3 均可；显式错误主版本仍拒绝 |
| 回答语言 | 在路径／文件名清洗前匹配完整 Git 原文及常见行内标记，再排除已知 hash；不把去掉文件名后剩余的普通词当成整条原文 |
| 最近提交 | 区分“仓库最近 N 个”和“仓库共 N 个”；数量需符合范围，重复项、未知列表项、把分支标签算成提交均不能通过 |
| 中文收尾 | 识别无问号的条件式继续邀请；保留必要澄清和不索要回复的直接建议 |
| 编译错误位置 | 接受可唯一定位的文件名；位置遗漏与类型解释分开报告，反向类型检查只针对明确的肯定断言 |

报告继续记录裁判源码哈希，并通过 `dataset_revision` 区分语义。采集评分只接受当前 System context 中的失败命令、退出码、原始输出及独立请求，不要求模型复述路由标签。revision 10 不用于重评旧协议记录，旧报告与归档不改写；对照须使用相同输入契约和裁判。

`expect` 必须包含：

| 字段 | 含义 |
|---|---|
| `max_steps` | engine step 上限，含总结，不是命令数；原样记录超出预算的执行 |
| `max_confirmations` | 实际审批请求上限，拒绝也计数 |
| `response_language` | `zh` / `any` / `not_applicable` |
| `final_question` | `forbid` / `require` / `not_applicable`；`require` 在歧义对照中衡量是否请求缺失信息，不限疑问句形式 |

具体门槛直接见场景文件：普通只读查询 3–4 步，构建/测试 4 步，提交 5 步，诊断 6 步；只读/版本不确认。端口失败诊断允许一次对已占用本地端口的原命令复现，因此最多 1 次确认；这是对合法诊断路径的授权，不要求必须重跑。其余既有预算不变。正式采样前冻结门槛，不因模型表现不佳放宽。

事实裁判使用最终回答、实际执行结果和状态，不把输入回显或工具输出直接当作正确回答。新执行类场景要求 native trace：未执行的工具调用、已有产物、只编译不跑测试都不算成功。构建必须保留源码，清理不能删配置，提交必须覆盖正确改动且不增加多余提交。语言行数、列表作用域等保守规则和正反例见 [checks](checks/) 与[测试](tests/)。

语言和收尾是确定性启发式，不用另一模型裁判。语言计数先匹配完整的已记录 Git 原文，再剔除代码、引用诊断、路径、URL、版本号、工具名及已知 hash；中文正文至少两个汉字，且汉字数大于残余拉丁词数。不按英文列表外观整段豁免；事实中的数量、条目和收尾邀请仍单独判断。JSON 保留抽取正文、统计及原因，供复核。纠错和 `-s` 纯命令输出不检查回答语言/收尾。

## 审批与隔离

保持 **confirm**，不用 auto/yolo 或“本会话同类放行”。[审批模块](approval.py) 只允许声明任务的有限命令：

- 改名与 cwd 保留固定、可验证的命令形式。
- 项目操作只允许同一夹具中的受限构建/测试组合、已知 npm/Node 脚本、完整 unittest、有限 git add/commit；源码与脚本内容必须仍匹配夹具。完成判定仍要求本任务的主要动作真实执行，不能只构建代替跑测试。
- 端口诊断仅为本次自有监听器开放原命令的 loopback 重现，不开放其他端口/地址、任意 Python 代码、文件修改或终止进程；超时与监听器存活检查保持有效。
- 可组合受限的 `cd`、pwd、ls、cat。允许本夹具 Cargo.toml 和解析后仍指向已记录工具的链接。
- 项目操作的多命令组合必须用 `&&`，不允许 `cargo test; ls` 这类以末尾命令掩盖失败的写法；单条命令末尾的 `;` 和只读版本查询不受影响。
- 唯一重定向例外是未加引号的命令末尾 `2>&1`；外部路径、替换脚本、任意解释器代码、文件重定向、管道、`|| true`、push 等不放行。
- `-s` 只验证严格解析的 tar 子集，不把任意模型建议交给 shell 执行。

版本查询的评分解析与审批解析分开；识别已经返回的查询证据，不额外批准 `||`、重定向或其他命令。

每个试验使用新进程、私有 HOME/NOSH_HOME、固定 locale/TZ/PATH 和终端大小，显式传入 `--norc --offline --no-download --seed`。预检记录工具路径、版本、哈希；私有 `HOME/bin` 接入已解析的真实工具，Rustup shim 不依赖试验 HOME。Cargo/npm 缓存隔离且离线，Rust 构建单 job、关闭 incremental。

工作目录默认 `/tmp/nosh-eval-<uid>`，以所有权标记和独占锁保护。自定义 `--work-dir` 必须属于当前用户、0700、无符号链接祖先，位于仓库/AGENTS.md/README 祖先之外，且不能包含二进制、模型或报告。夹具自身提供的 README 不受此祖先隔离规则影响。只回收本次创建的目录和进程；端口只绑定 loopback 8080，外部服务占用时明确报错，不抢占或终止它。

**临时目录不是安全沙箱。** 建议在评测专用用户或受限容器中运行。模型权重、个人配置、大日志不入库。

## 指标与复现

| 指标 | 口径 |
|---|---|
| 通过率 | 通过数 / 计划试验数；同时报告全部、原任务、新任务、模型与本地纠错 |
| 步数/确认 | 逐次有效样本的均值，包含失败；不对场景均值简单再平均 |
| TTFT | 首步 Usage 的首 token 延迟，不含加载；汇总为中位数 |
| 总耗时 | 进程启动至退出，含加载/交互/退出，不含夹具准备与独立验证；汇总为中位数 |
| RSS | Linux wait4 峰值 MiB，含内核对已等待后代的统计，不是进程树求和；汇总取最大值 |
| Token 成本 | 分别记录新增 prompt、复用缓存、生成 token；包含工具 schema 与模板，缺失观测不填零 |
| CommandAssist 结果 | 记录宿主接受的 kind、意图、command ID、前后台来源及完成／错误／取消状态 |

native 观测由 `NOSH_EVAL_TRACE` 的私有版本化 JSONL 提供；缺失/损坏时不自动降级。legacy 的 `-s` TTFT 为 N/A，旧 REPL 显示精度有限，差异记录在报告中。必须观测不到的数据不填 0。

启动、审批/终端协议、退出阶段超时属于观测/基础设施错误。任务阶段的总时限耗尽仅在 native trace 能确定边界时归为任务失败：或者此前步骤完整且恰有一个模型步骤仍在生成，或者最终 `end_of_turn` 已完成、无工具调用/调用错误，但 REPL 完成标记尚未出现。前者没有最终回答；后者保留 trace 中的最终回答作为证据，但仍判超时失败。其他不完整或矛盾观测仍是基础设施错误；不伪造未完成步骤的 TTFT。

报告保存模型、二进制、工具、场景和评测器哈希及原文证据。`--build-info` 需要 schema v1、完整 `source_revision`、匹配的 `binary_sha256`；没有时注明来源未验证，不把当前 checkout 当作被测构建。

双跑比较**判定和最终状态**，输入/回答/工具差异另列。固定 seed 不固定任务时间、命令耗时或 PID；新进程只保证冷会话/KV，不保证冷 OS 页缓存。报告记录 `dataset_revision`；跨 revision 比较会明确警告，即使仍可配对场景/seed，也不把差值当作受控回归结论。

## 基线生命周期

以下均是 **revision 1 的历史基线**。revision 10 尚无完整固定 seed 双跑基线，也没有用新规则重评或替换旧记录；开发 smoke run 不能代替正式基线，48.0% 不是当前协议下的通过率。

| 基线 | 范围与结果 | 证据 |
|---|---|---|
| main `78b7e50` | 25 × 5 × 2；120/250（48.0%），平均 4.88 步/1.028 次确认；模型 110/240 | [报告](baselines/main-78b7e50-expanded/report.md)、[来源与分析](baselines/main-78b7e50-expanded/analysis.md) |
| main `4f602ab` | 原 10 场景双跑；原始 70/100，经透明判定修正为 73/100 | [原生观测基线](baselines/main-4f602ab/report.md)、[分析](baselines/main-4f602ab/analysis.md) |
| main `7c57a88` | 原 10 场景单轮 legacy；36/50 | [有限观测基线](baselines/main-7c57a88/report.md) |

新基线的全部 250 个身份保留。原始 120/129/1/0（通过/失败/错误/缺失），仅确定性恢复一条真实模型超时的指标并改为失败，另去掉一条不影响通过数的收尾误判，归一化为 120/130/0/0；没有重抽 seed。失联和过窄审批规则的前次运行也保留说明及可得证据。

判定加状态仅 **107/125** 对一致，最终状态 **119/125** 对一致，#3 的复现要求仍未满足。完整报告、原始/诊断记录及一次性材料已迁到 [#4 的归档索引](https://github.com/NewFuture/nosh/issues/4#issuecomment-5844795358)，实际托管在同仓库的非最新版证据 Release 附件；[archive.json](baselines/main-78b7e50-expanded/archive.json) 固定下载地址与 SHA-256。Git 只保留精简指标和来源，原文件在附件中逐字节保留，已有历史不重写。

[复核脚本](baselines/main-78b7e50-expanded/reproduce.py) 只接受显式下载的 ZIP，先核验整体及每个文件，再用归档内固定处理源码重建报告，无需 Git 历史、联网或模型。常规 CI 不下载附件。`summary.json` 不是完整报告；需要 `--compare` 时，使用验证后的归档内 `baseline/report.json`。

## 开发与无模型自测

| 模块 | 职责 |
|---|---|
| `run.py` | CLI、工具预检、环境/来源、逐次执行与保存 |
| `suite.py` / `scenarios/` / `suites/` | 场景定义、显式套件选择、v1/v2 契约与夹具/审批兼容性 |
| `driver.py` / `observations.py` | PTY/进程生命周期；原生/legacy 观测解码 |
| `fixtures.py` / `approval.py` | 夹具、隔离和允许变化；受限命令解析/审批 |
| `checks/` | `agent`、`project`、`command_assist`、`capture`、`experience` 分别评分，`common` 共用文本解析，`__init__` 保留统一接口 |
| `report.py` | 统计、比较、JSON/Markdown |
| `checkpoint.py` | 复制原子报告与已结束试验日志，不改实时结果 |

```bash
python3 -m unittest discover -s eval -v

# 按职责选择无模型回归
python3 -m unittest eval.tests.test_suite eval.tests.test_observations
python3 -m unittest eval.tests.test_agent_checks eval.tests.test_command_assist

# 从 archive.json 的地址下载 ZIP 后，可选地完整复核证据
python3 eval/baselines/main-78b7e50-expanded/reproduce.py --archive PATH_TO_ZIP --check
```

测试使用真正的无依赖小项目构建/测试、脚本化 PTY、协议正反例和已保存的基线，不加载模型。CLI 观测的 Rust 回归可运行 `cargo test -p nosh-cli --locked`。无模型测试不能替代真实模型基线。
