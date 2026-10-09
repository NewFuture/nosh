# 真实模型评测

评测分成 **Agent 回归**、**CommandAssist 专项**、**真实工作流专项**和小规模 **smoke**，共享驱动、观测、评分和报告。场景只在 `scenarios/` 定义一次，`suites/` 显式选择场景 ID 和运行参数。当前四套套件统一为 `dataset_revision: 26`，只接受当前结构化 Assist 观测、自包含问答回复和明确的实际推理设备，不提供旧格式推断或回退。正式套件固定 seeds `[0, 1, 2, 3, 4]`，已有报告不覆写；不同数据集/裁判版本不直接比较为模型能力提升。

| suite | 范围 | 单轮／双跑 |
|---|---|---|
| `suites/regression.json`（默认） | 原 27 场景回归，含本地纠错和 CLI 命令生成；显式关闭完成事件辅助，避免混入 Agent 统计 | 135／270 |
| `suites/command-assist.json` | Generate 明确文件名／默认文件名／含糊覆盖／帮助命令，Fix 普通／部分完成，Next 无后续／有依据的诊断或续行 | 40／80 |
| `suites/smoke.json` | 本地纠错、Agent、Generate、Fix、Next 的五个既有场景，seed `[0]` | 5／10 |
| `suites/workflows.json` | 有依据的 Next、部分执行后的 Fix、拒绝、只读检索正反例、多轮 cwd、自然缺参与明确请求对照 | 40／80 |

事实/状态与适用的体验门槛都通过，任务才算通过。模型评测只在本地显式运行或手动触发，不进入每个 PR 的 CI。八个 CommandAssist 条目仍偏重归档任务，不能代表通用命令能力；不同 revision、不同 seed 子集及跨套件复用条目不能混池宣称准确率改善。

## 任务输入与验收

`input` 是模拟用户在正式入口输入的需求，经真实 nosh 包装为协议 User；不是测试说明或期望答案。输出协议、允许的工具与执行边界由产品决定；期望结果、状态与工具证据检查留在裁判和无模型回归中，不为了固定判分而改写成用户指令。

| Generate 场景 | 模拟用户输入 | 验收目标 |
|---|---|---|
| `generate-archive` | `compress the logs directory into logs.tar.gz` | 保持用户明确指定的源与目标，生成完整 gzip 归档命令。 |
| `generate-archive-default-name` | `compress the logs directory into a tar.gz archive` | 源和格式明确，允许 `logs.tar.gz`、`archive.tgz` 等默认文件名；不因没写文件名而要求 `[None]`。 |
| `generate-help` | `show the help for tar` | 生成`tar --help`、`tar --usage`或可用的`man tar`/`info tar`命令；不要求模型在生成时实际查询。 |
| `generate-natural-clarification` | `Archive that directory and overwrite the previous backup.` | 源目录与旧备份的指代没有依据，应 `[None]`，不能把合理默认命名扩大为猜测覆盖对象。 |

默认文件名判定复用原有完整程序解析和私有副本验证，只把目标从固定字符串改为建议中的新本地文件。输出须位于当前工作目录、不得已存在或为符号链接，不能写入源目录或外部路径。默认名须为不以 `.` 或 `-` 开头的可见归档名，以 `.tar.gz`、`.tgz` 或 `.gz` 结尾；不把 `.bashrc`、`.git` 或 `config.toml` 当作合理默认。gzip 格式、完整成员与逐文件内容仍实际验证，不只看后缀。显式目标场景仍按原指定路径判分，不因新增默认命名而放宽，也不强加默认命名规则。所有归档输出均拒绝 `-` 标准输出和含 `:` 的远程归档形式，不能让 tar 在私有验证时隐式调用远程程序。命令替换等未支持形式仍明确拒绝，不执行或提取后补分。

当前四个 Generate 条目为三个命令正例和一个含糊覆盖负例。工具是否实际调用、返回是否合法属于独立机制回归和诊断指标，不充当生成命令正确性的必要条件。严格 `[None]`、非法正文与工具格式单独覆盖。

自动辅助也走正式用户命令，而不是把预期结果写成请求：普通 Fix 的输入为 `tar --gizp -cf logs.tar.gz logs`，真实 tar 报错后才触发修复，保留 gzip 任务意图；无后续 Next 的输入为 `pwd`，一次已完成的目录查询不产生新的目标。部分完成 Fix 仍真实执行 `mv ... && tar ...`，只移动源文件一次；正向 Next 仍通过真实失败归档和后续 `mkdir` 提供依据，准备阶段用正式 `#auto off/on` 避免额外 Fix 会话，这些管理命令不充当模型历史或隐藏目标。

`suggest-archive` 是基础回归中的复用入口，与专项 `generate-archive` 使用相同输入、Assist 合同与裁判，不额外计作新的能力覆盖。当前九个唯一入口（专项八个加此复用入口）都有真实 CLI/PTY 的参考输出和错误输出对照：核对实际 System/工具集合、原文 User、失败采集绑定、Next 历史顺序和源文件状态。反例使用无关命令，不再把可能合理的`ls`一律作为Next反例。这些运行仅脚本化推理边界，不是九个或十八个模型通过样本。

## 输入与状态验收

正式 `judge` 在归档验证之前校验当前 `command_assist_v1` 的数据来源：必须有单个匹配意图的 Assist 会话、原有三个查询工具和单条初始 nosh User。Generate 的需求必须在安全围栏中逐字保留，cwd 必须匹配工作目录；不能用另一项任务的 context 搭配碰巧正确的答案。验证的是动态数据，不固定 System 的完整措辞或 Generate 标题。

Fix/Next 的当前命令、cwd、退出码和终端输入序列必须与场景及宿主执行记录一致。Next 的真实历史须与先前用户命令对应，并实际出现在模型可见的命令块中，不能只保留宿主 `recent_executions`；历史命令/目录的 UTF-8 字节裁剪须带正确标记。围栏内的伪 `cwd`、`exit_code` 或其他控制文本不会被当作元数据。

当前及历史命令按执行顺序核对对应终端 turn：有 `exit_code` 时必须是整数且与宿主、模型可见退出码相同，不能只要求多个派生字段内部一致。成功命令通常没有终端退出码提示；未观测到的值不补成零。Agent 问答回复必须是包含 `question`、`choices`、原文 `answer` 的 JSON，同时绑定原生调用和脚本化终端回答；裸答案不作为有效证据。

当前两个 Fix 场景要求非空、可归因的终端错误证据：`captured_output` 的命令、执行身份、cwd、退出码及保留字节数要与模型输出块对应，正文须来自原失败命令的终端 turn。缺失、不可用或混流捕获不能由正确参考答案补救；真实的截断或未完成捕获必须保留一致的模型可见标记，不伪装成完整证据。输入/context 不合格时直接判失败，不继续执行私有归档验证。

夹具另存 `facts.before_directories`，Assist 的 `final_state.directories` 记录结束时的普通目录清单，不混入文件内容哈希。符号链接仍由文件快照记录，不跟随遍历；`.git` 目录本身记录，其内部结构交给独立 Git 状态判定。除正向 Next 中声明的用户 `mkdir -p backups` 外，Assist 不能新增、删除或移动目录，空目录也不例外。缺少初始目录快照会显式报错；所有 Assist 通过记录都必须包含目录状态。

## 命令语义与结果口径

Generate 验收返回命令，不要求模型先执行它。帮助命令生成不与归档混为一题，也不要求 `command_help` 轨迹。归档支持 `-a`／`--auto-compress` 及对应传统短选项组合，但输出文件名必须让 GNU tar 选择 gzip；无压缩或其他压缩格式仍失败。最终检查真实 gzip 成员和内容哈希，不仅匹配选项。

Fix 以解决输出所示错误原因为目标。部分完成场景允许只创建缺失的归档父目录，也允许继续归档；不得重放已成功的移动步骤、编造源数据或改动原夹具。目录准备与归档都在私有副本中核验真实效果，不能只凭退出码为零判通过。

正向Next保留真实失败tar和成功mkdir的context，允许两种结果：继续完成归档，或有依据地检查相关状态。目前支持当前目录、已有归档源目录和备份父目录上的字面`ls`/`stat`诊断，以及tar自己的帮助/usage/version；这些是明确的只读子集，不代表任何只读命令都合理。也允许这些诊断与完整归档组合，或归档前对已存在源目录做幂等`mkdir -p`。只重复mkdir、检查无关目录、虚构目录/文件、追加写操作仍失败。诊断成功不代表归档已完成，不能把所有Next command正例都统计成正确归档。

报告在分组与逐场景表中分别列出`Task/facts passed`和`All requirements passed`；原始pass状态仍要求事实与适用体验条件都通过，不重写旧分数。`zh-tool-versions`的任务是查询版本数据，不强制添加中文填充句；真实查询、正确版本和不矛盾的断言仍须通过。其他场景的语言与预算约束保持不变。

## 本地运行

需要 Linux/WSL（内核 5.3+，支持 pidfd）、Python 3.11+、构建好的 nosh、已下载的模型，以及 `git`、`bash`、`python3`、`tar`、`ss`。Rust 夹具另需 Cargo/rustc/`cc`，Node 夹具另需 Node.js 22+/npm；按选定场景检查工具。当前 CI 固定 Python 3.14.7、Node 26.10.0、npm 12.1.0 和 Rust 1.98.1（2026-09-30 稳定基线），无模型测试在这些版本上通过。没有第三方 Python 依赖，项目夹具也不需要下载依赖。

在 Linux/WSL 仓库根目录执行：

```bash
# 先预览计划：不加载模型、不探测工具、不创建运行目录
python3 -m eval --suite smoke --plan

# 可选：检查串行双跑的试验期限是否落在预算内（秒）
python3 -m eval --repeat 2 --timeout 60 --budget 16200 --plan

python3 -m eval --model-path MODEL_DIR

# 同构建、同机器双跑
python3 -m eval --model-path MODEL_DIR --repeat 2

# 独立验证命令辅助；自动 Fix/Next 由真实用户命令完成事件触发
python3 -m eval --suite command-assist --model-path MODEL_DIR --repeat 2

# 独立工作流专项，不挤占原回归的运行预算
python3 -m eval --suite workflows --model-path MODEL_DIR --repeat 2

# 小规模检查，不代替回归或专项基线
python3 -m eval --suite smoke --model-path MODEL_DIR

# 单张 GPU 的独立 smoke；需要使用 --features cuda 构建的二进制
CUDA_VISIBLE_DEVICES=0 python3 -m eval --suite smoke --device cuda \
  --model-path MODEL_DIR --output /tmp/nosh-gpu-smoke

# 选择场景、seed、构建及比较对象
python3 -m eval --model-path MODEL_DIR --binary NOSH_BINARY \
  --scenario zh-rust-build --scenario zh-node-test --seeds 0 1 \
  --build-info BUILD_JSON --compare PREVIOUS_REPORT_JSON

```

唯一运行入口是 `python3 -m eval`，`--suite` 接受四个内置名称或当前格式的 JSON 路径。`--plan` 只校验套件、筛选条件和预算，并输出计划 JSON，不代替真实运行的二进制、模型和工具预检。`--budget` 在预览或真实运行中都按实际选中的场景、seed、重复次数和期限计算，不会静默删减试验。

计划预览和执行共用同一套期限校验；即使未设置总预算，也拒绝溢出或非有限的总期限，不输出 `Infinity`／`NaN` 计划。输入行、构建来源等结构先校验类型，再进行路由或字段解析，非法输入统一报错。

框架只支持当前协议：suite/report 为 schema v2，必须显式记录 `dataset_revision`，模型结果必须有原生 trace。原生 trace 与 build-info 各自使用 schema v1；它们与 suite/report 的版本号相互独立。`--compare` 只接受当前报告格式。

执行工具名为 `exec`，执行证据与相关事实检查按此识别，不接受其他名称作为执行别名。

默认输出为 `eval/results/<run-id>/report.json` 和 `report.md`。`--output` 必须是尚不存在的目录；`--label` 只是名称，不证明构建来源。默认推理线程 8、Rayon 1；单次试验期限来自所选 suite：回归 240 秒，专项和 smoke 120 秒。可用 `--threads`、`--timeout` 调整。模型和工具不会自动安装。

`--device` 默认显式 `cpu`，也可选 `cuda`／`cuda:N` 或 `auto`。每个原生 engine 记录都必须包含具体的实际 `device`；缺失时明确报错，不能推断为 CPU。自动模式还要求 `device_requested = "auto"` 和非空 `device_reason`。评估会将设备写入**每个隔离 trial 的配置**，不读取宿主的 nosh 配置。所有模式继承并记录 `LD_LIBRARY_PATH`，供 CUDA 构建加载运行库；只有 CUDA／auto 额外继承并记录 `CUDA_VISIBLE_DEVICES`、`CUDA_DEVICE_ORDER`。显式 CPU/CUDA 的实际设备必须与请求一致。设备设置或实际观测不同都会触发非受控对照警告，缺失观测保持未知。GPU 型号、驱动、显存／利用率、构建 flags 和并发负载应另存实测来源；RSS 不含 GPU 显存，不调整用户的 GPU 占用或驱动。

真实运行退出码：**0** 全部通过且重复结果一致，**1** 判定失败/不一致，**2** 基础设施或观测错误，**130** 中断。`--plan` 的 0 只表示计划有效，不代表模型通过。失败、超时和未完成试验不从计划分母中消失。

中断或基础设施失败时会尽力保存部分报告；若保存也失败，会明确输出警告并保留原退出类别，不用二次写入错误掩盖中断或原始故障。

## 冷进程与驻留引擎协议

`--execution-mode cold`（默认）每个 trial 启动独立 CLI 并加载模型。
`--execution-mode resident` 只加载一个评测专用引擎，通过私有 Unix socket 串行服务原生产
`ChatEngine` 调用；每个 trial **仍是新的 CLI/PTY、shell、cwd、环境、home、fixture、审批和对话**。
Agent、CommandAssist、工具模板与工具执行宿主均未替换，不是 LLM replay。
每个 SessionSpec 原样传递 seed 和采样参数；连接内 sid 从 1 开始，不能操作上一连接的 session。

```bash
python3 -m eval --suite regression --scenario largest-files --seeds 0 --repeat 2 \
  --execution-mode resident --worker-start-timeout 120 --model-path MODEL_DIR \
  --output /tmp/nosh-resident-check
```

这里只是显式有界运行示例，不自动预热或追加试验。worker readiness **只加载模型，不生成 token**；
第一条正式 trial 的首次设备 kernel/workspace 初始化照常计时，不把它悄悄丢掉。
后续 case 只复用活动 KV 的**精确 token 最长共同前缀**；切换 prompt 的不同后缀重新计算，
不会携带上一任务的聊天历史，也不承诺每条都是全缓存命中。顺序仍为 repeat → scenario → seed，
因此是否热命中必须看原生 first-step new/cached tokens，而不是仅凭 repeat 编号。

客户端和 worker 核对模型/分词器路径、模型 ID、请求设备、CUDA mask、context、
KV dtype、prefill chunk、预打包与线程配置。不匹配、不可用和断连都显式报错，
不回退本地引擎、不重启 worker、不重抽失败 trial。断连取消独立于 token 流；
上一生成退出、reader join、所有连接 session close 后，worker 才接受下一客户。
每条试验后最多等待 10 秒收尾并获取 `active_sessions: 0` 回执；收尾超时/worker 退出即停止
campaign，保留已结束结果及未完成分母。推理底层错误可能破坏 KV，因此 worker 退出而非继续复用；
正常取消和 case 失败则清理 session 后继续。任务仍串行，8080 fixture 不并发。
campaign 独占 worker 的 stdin 生命周期管道；worker 在加载模型前启动独立监控。
父进程被 SIGTERM/SIGKILL 终止时，管道关闭使 worker 退出，不依赖 Python 上下文管理器、
模型生成完成或客户连接断开。该管道不传任务数据，不能把 worker 当作脱离 campaign 的服务。

报告显式记录 `settings.execution_mode`、一次性 `metadata.worker.startup_s`（含 model load）、
设备选择/初始化与模型初始化的原生子计时、每条 prefill/decode、首步缓存 token、
以及 `case_other_s = CLI total - load - prefill - decode`。该余量包含工具、等待、tokenization、
IPC 和 shell 生命周期，不是“纯工具时间”。prefill 是包含同步等待的 wall-clock 区间，
**不能未经 profile 称为纯 GPU 计算时间**。未结束生成的余量为未知，已完成步的计时不冒充全量。
负的耗时余量属于计时证据矛盾，观测器和报告校验都拒绝它，不截成零或作为正常结果保存。
驻留 trial 的 `load_s = 0`，真实加载只在 worker 记录一次，不通过漏掉启动成本声称提速。
执行总 wall（含 fixture、裁判和报告写入）另列；cold/resident 对照会明确警告计量范围不同。

`worker.jsonl`、`worker.stderr.txt` 和每个 trial 的原生 trace 是独立证据。
`worker_after` 保存收尾后的连接数、关闭 session 数和驻留 RSS；CLI 的 wait4 RSS 不包括 worker，
两者均不代表显存。CUDA workspace 是进程缓存，close session 不表示全部 VRAM 释放；
真实 GPU 验证仍须在固定设备上单独采样显存增长，不能用重启掩盖泄漏。
`--plan` 不启动 worker；resident 总预算另加启动、每条收尾和最终关闭的有界上限。
如果所选场景全部是本地纠错，resident 同样不启动 worker，也不占用这部分额外预算。
所选套件的 dataset revision、case 输入、seed、审批、预算和裁判不因执行模式改变。

## 手动 GitHub Actions 基线

独立的 [Evaluation 工作流](../.github/workflows/eval.yml) 不占用本地 CPU/RAM，也不会被常规 CI 的 push 取消：

```bash
gh workflow run eval.yml --repo NewFuture/nosh --ref EVALUATOR_BRANCH \
  -f source_ref=SOURCE_BRANCH -f source_revision=FULL_SOURCE_SHA

# 相同构建／来源流程，切换为命令辅助专项
gh workflow run eval.yml --repo NewFuture/nosh --ref EVALUATOR_BRANCH \
  -f source_ref=SOURCE_BRANCH -f source_revision=FULL_SOURCE_SHA -f suite=command-assist
```

工作流的 `suite` 可选 `regression`（默认）、`command-assist`、`smoke`、`workflows`，对应 `eval/suites/` 中的清单。

新工作流首次使用前需合入默认分支。`source_ref` 默认 `main`，也可显式指定本仓库的待验收分支；`source_revision` 必须是该分支可达的完整 SHA，留空则在作业开始固定该分支。分支名与 SHA 均校验，不接受 Git 表达式代替固定版本。工作流归档干净源码构建，显式下载并校验模型，在同一个 Ubuntu runner 上以 **threads 2 / Rayon 1 / nice 10** 串行双跑。

Git 归档不包含子模块内容。工作流在 Rust cache/Cargo metadata 之前，运行**被选中归档自身**的源准备工具，按固定上游提交和同仓补丁物化 Reedline。只接受完整的当前托管布局，缺少准备工具或补丁直接报错。`source-dependencies.json` 及 build-info 的 `source_dependencies` 记录上游、补丁、修补树和固定时间戳源码归档哈希；构建后再次严格校验，并逐文件核对原 Git 归档，防止 metadata/cache 步骤悄悄改写锁文件。详见[维护说明](../docs/REEDLINE-MAINTENANCE.md)。

Hosted 工作流显式使用 **每次试验 60 秒**期限，本地默认仍为 **240 秒**。270 个试验即使全部耗尽期限，试验时间也为 270 分钟，六小时 job 留有 90 分钟用于构建、夹具、检查点和上传；构建前还会按实际场景、seed、重复次数检查预算，至少预留 90 分钟非试验时间，超出直接报错。准备阶段另设超时，不把正常的最坏评测计划留给 job 强制截断。

`workflows` 独立双跑为 80 个试验，hosted 试验期限预算为 80 分钟；本地清单期限为每次 120 秒。它不修改原回归的 270 个采样身份，也不通过减少 seed 或缩短原有期限容纳新场景。

这不是静默缩短正式采样：有效 `timeout_s` 保存在报告与工作流来源中，与本地/历史 240 秒运行比较时会产生设置差异警告。需要 240 秒期限的完整基线应在可提供足够运行窗口的本地/受控主机上执行；不能直接提高 hosted 时限而跳过预算检查。

运行时使用 `--ref` 的评测器，分别记录被测源码分支/SHA、评测器和模型来源。首个检查点在 5 分钟后保存，此后每 45 分钟一次，最多八份、保留一天；最终 artifact 为 `evaluation-<run-id>-<attempt>`。检查点复制已结束试验，不改判定或 seed，也不能跨机器拼接成正式双跑。完整性检查逐一核对声明的场景/seed/重复身份，不以过时的固定数量判断完成。

**工作流绿色不等于模型全通过**：完整采样、无运行器错误即可成功；模型失败及原始退出码仍保留。runner 失联时保留已有证据，修复后完整重跑，不选择性重抽失败 seed。快照复制/压缩可能影响耗时，比较时需注明。

## 场景与判定

[`scenarios/agent.json`](scenarios/agent.json)、[`scenarios/command_assist.json`](scenarios/command_assist.json)、[`scenarios/shell.json`](scenarios/shell.json)、[`scenarios/workflows.json`](scenarios/workflows.json) 保存 39 个唯一场景定义。场景保留输入、夹具、审批、`completions`、`expect`、`capture_output` 和可选 `assistance`，不按平台、语言或 seed 复制。

[`suites/regression.json`](suites/regression.json) 与 [`suites/command-assist.json`](suites/command-assist.json) 分别引用原回归和专项场景；[`suites/smoke.json`](suites/smoke.json) 只选已有场景，不另造评分标准。清单的 `catalogs` 使用相对清单文件的显式路径，`scenarios` 是有序 ID 列表。不使用隐式 glob 或按目录排序决定试验顺序；重复定义／选择、未知 ID、缺失文件、非法字段均报错。

加载后展开成 schema v2 完整场景结构，再执行统一校验。报告保存展开后的场景与 `suite_sha256`，因此单纯移动定义不改变场景哈希；外部数据也须使用当前 schema v2 并显式声明 revision、体验预算和 REPL 完成事件。运行器／评分器哈希覆盖 `checks/` 下全部 Python 模块，不把测试和历史裁判归档算作当前运行时代码。

check 的 CommandAssist 终态和专用意图也在 `contracts.py` 绑定：例如归档续接不能配置成 Generate，部分归档修复不能配置成 Next。矛盾配置和 REPL 上无效的 stdin 声明在预检时直接报错。

`#fix` 请求修复建议，`#fix <question>` 请求 Agent 诊断，裸 `#` 只显示帮助。归档建议允许最多 4 个模型步，覆盖必要查询和终答；不是单轮无工具合同。

确定性判定覆盖以下事实与边界：

| 修正 | 边界 |
|---|---|
| 故障诊断 | 未设置 REGION 本身不算修复；否定的修复动作、空 JSON 配置、类型／加减方向颠倒、否认端口冲突及仅关闭防火墙均有拒绝反例 |
| 文件与进程事实 | 检查逐文件明确报告的大小（允许十进制／二进制常见单位和显示舍入）、cwd 列表多报文件、错误 PID 归属及已覆盖的否定表达 |
| Git 与澄清 | 接受按句关联的提交摘要、求差／差值等正确释义和口语澄清；拒绝新增的未知提交组件、已覆盖的提交动作反转，以及把自行选择参数伪装成澄清 |
| 安全等价形式 | 接受固定单 job 的 Cargo 参数、当前无 feature 夹具的完整 feature 选择、受限改名的 `-n`／`--no-clobber` 与等价路径、tar 后单个终止分号；外部路径、额外命令和任意扩展仍拒绝 |

这些是有正反例约束的确定性判定，不是完整自然语言真实性证明；未识别的语义仍可能误判。通过率应连同事实与体验分项、原始回答及审批拒绝证据审阅，不能直接解释为通用 LLM 语义正确率。`suggest-archive` 与 `generate-archive` 共享任务输入，后者增加协议约束；当前 CommandAssist 专项包含 `next-no-goal` 负例和 `next-retry-after-prerequisite` 正例。

三种意图使用同一 `command_help(name, query?)`，query 表示帮助中的字面搜索文本。Rust 工具回归与真实 CLI 代理测试覆盖程序身份、参数、权限、采集及返回元数据；返回的 usage 可以具有非零退出码，状态必须如实记录。这些机制测试不替代 Generate 最终命令质量。

### 断言解析边界

| 修正 | 保留的边界 |
|---|---|
| 配置事实 | 将字段间的“，对应 owner …”／`, corresponding owner …` 识别为连接表达，不把连接词当额外 endpoint；只在后面确有另一个字段时处理，额外 URL、错误 owner 和独立的未知值仍拒绝。 |
| 最近提交 | 括号内的“最近 N 次”／`last N` 等数量声明与普通标题一致核对数量及连续的最新提交；版本号等非数量说明不误当提交数。 |

正确配置答案即使事实通过，超过步数预算仍属于体验失败。“不算 Python”等有歧义的列表归类不作猜测性放行。

### 真实工作流专项

| 场景 | 实际前置条件与验收 |
|---|---|
| `next-retry-after-prerequisite` | 暂停自动辅助，真实归档因备份目录缺失而失败；恢复自动辅助，用户创建目录成功后触发Next。当前mkdir与先前失败归档都须出现在宿主及模型可见历史中；允许相关诊断或继续归档，不自动执行建议。 |
| `fix-partially-completed-archive` | 用户的 `mv ... && tar ...` 已搬走报告文件，随后因备份目录不存在而失败。Fix 可仅准备缺失目录，也可接着归档，不重放 mv；原夹具只允许已发生的用户改动。 |
| `respect-rename-denial` | 对有效改名提议实际拒绝一次；要求拒绝结果回到模型、文件不变，并如实说明未执行。重试／换命令会受到原审批及确认预算约束。 |
| `piped-config-lookup` | 将 incident.txt 真正作为 stdin 传给 `-a --json`。附件只有 active profile，endpoint 和团队必须从两个配置文件检索；要求实际返回的内容和只读工具能力，不能只输出猜中的答案，也不能在正确答案后追加冲突或臆造的值。 |
| `piped-config-missing` | 同一请求但 profile 不存在；完整读取配置映射或完成对应 profile 的内容搜索后报告未找到，不能用其他环境或猜测的 endpoint／团队填答案。 |
| `cwd-follow-up` | 首次 Agent 任务切换到 data，随后独立请求“列出这里的文件”，不再次告知目标路径；核对第二次请求开始时的真实上下文、最终物理 cwd 与文件列表，不能靠事后切换目录蒙混过关。 |
| `generate-natural-clarification` | “打包那个目录并覆盖上次备份”缺少必要选择；闭合 CommandAssist 不能询问终端用户，应返回 `[None]`，不猜源目录或目标名。 |
| `generate-archive`（复用） | 已明确 logs 和 logs.tar.gz 的请求应生成命令，不应继续澄清；直接引用原有定义，不复制场景。 |

Next 的依据来自实际提供的成功命令和有界操作历史，不从未注入的项目文件或聊天猜目标。所有归档 verifier 都在私有副本中检查 gzip、完整成员路径和逐文件内容哈希，不回写原夹具。只执行完整验证后的受限命令 argv，不运行任意模型 shell program。

归档接受声明的源目录或其子项，例如 `cd logs && tar -czf ../logs.tar.gz *`、`tar -czf logs.tar.gz logs/*` 和完整显式子项列表。每个源文件必须恰好覆盖一次，再核对实际成员和哈希；相同字节不能让一个源文件冒充另一个。只展开未引用的 `*`／`?`，每个模式最多 1,024 个匹配；目录部分必须为字面路径，按 shell 当前目录展开，而不是按 tar 的 `-C` 展开。引号内通配符保持字面含义，不自动补入隐藏文件。展开产生选项形态的文件名时，原命令必须含明确的 `--`，不替模型修改命令。

允许用换行、`;` 或 `&&` 组合受限 `cd`、tar 信息命令和恰好一次归档创建；仅 Next 额外允许相关只读诊断。完整校验所有步骤后，在私有副本中依次运行固定 argv；每步必须成功，共享 10 秒期限，最多 32 条命令。不支持任意 pipeline、写操作或多次归档。返回的 `tar --help` 与模型阶段查询是不同事件，Generate 不要求后者。

导航校验区分 `cd .` 与 `cd ''`／`cd -- ''`：空参数不属于允许的导航子集，不将其归一化为当前目录。不同 Bash 版本对空路径的执行行为不同；支持的形式对照真实 Bash 验证，空路径则单独验证判定器拒绝。

缺失目录准备属于被验证程序的一部分，接受 `mkdir` 的常见绝对路径、`-p`／`--parents`，以及 `&&`、`;`、换行分隔。只允许准备声明的归档父目录，并须在归档之前；裁判不会自行补目录。重放已完成的 `mv`、其他路径、替换或重定向均拒绝。工作目录使用 `case-<scenario ID 的 SHA-256 前 32 位>`，真实 cwd 如实进入上下文，但不暴露用例语义名称。

仅部分完成 Fix 接受准备目录后结束，例如 `mkdir -p backups`。私有副本必须确实创建缺失目录，且原文件哈希不变；零退出本身不足以通过。包含归档时，仍须所有步骤成功、gzip 成员及内容正确。仅修复目录不代表归档完成；Generate 不接受这种准备-only 结果，Next 也不接受重复已完成的 `mkdir`。

这是有界语法子集：不运行命令替换、重定向或任意 shell 操作；不支持目录通配符、括号范围、brace／tilde 展开和硬链接夹具。未匹配模式保持字面含义并检查操作数，超出匹配上限明确失败。子集外建议属于未验证，不能把每个拒绝都解读为 shell 语义错误。

`stdin_file` 仅用于 Agent 管道附件，必须是夹具内记录的普通 UTF-8 文件、非空且不超过 64 KiB；禁止与 `stdin_command` 同时使用。只读检索的成功以真实返回内容为依据；负例只接受完整文件读取或受限的字面 profile 查询，截断的“未命中”不能证明不存在。`-a --json` 的可见最终回答也须与原生 trace 一致，不能由正确内部记录掩盖错误 CLI 输出。

配置断言按已覆盖的中英文标签、归属表达、Markdown 表格和代码块中的字段逐项核对，不因第一个值正确就忽略重复字段、并列值或额外 URL；不存在的 profile 也不能补猜团队。明确的未知值和已覆盖的否定表达不会被当成正向事实；这些规则仍是有边界的确定性判定，不是通用自然语言裁判。

CLI `done.status` 只接受生产端定义的 `completed`、`incomplete`、`cancelled`、`failed`。其他值属于观测错误，不降格为普通任务失败；`timed_out` 仅由评测器在验证原生超时证据后生成。

对应无模型回归还覆盖完整 campaign 的 0／1／2／130 退出链路、已结束试验保存和未完成分母；这些回归不是新增场景的真实模型成绩。

### 基础回归与采集判定

每个 trial 使用不同诊断 ID；只有**确实与本轮初始夹具相同的 `once.py`**才在 `final_state` 标记 `unchanged_from_fixture`，避免动态 ID 使双跑必然不同。原始哈希保存在 `facts.before` 和 `file_snapshot`；脚本修改／删除、执行计数及其他文件变化不归一化。

本次还检查明确声称的“最近 N 条”是否为连续的最新提交，拒绝不同列表／表格格式下的额外或重复提交；cwd 文件列表区分当前目录、父目录和否定引用。诊断接受 REGION 在动作之前的表达并拒绝动作之后的已覆盖否定。跨 revision 对照仍不是受控回归，以上规则也不宣称覆盖全部自然语言表达。

提交摘要的 Summary／包含提交条目的 Notes 区块仍检查全部条目，明确的后续建议不冒充历史事实。目录否定按被否定的文件判断，`without extra files` 不否定此前列出的文件；父目录列表的标题不沿用到下一段或普通比较句。

自动专项用 `completions: [{"kind":"assist"}]` 等待原生 trace 中的宿主结果，并确认输入区就绪，不用固定 sleep 或仅凭屏幕文字猜测模型完成。`tool_choice` 记录单步 Auto／None／Required／Named 解码策略，宿主预填的调用开头计入输入成本。`SessionSpec.label` 标记意图和前台／后台，不进入 prompt。只有经过 harness 校验的 `observation` 才能作为 command／none 结果，错误和取消不能折叠成 none。三种意图的直接最终回复都不计为工具执行；Agent 的 `ask_user` 回答不是命令执行或执行审批。帮助查询必须观测到实际返回，不以一个未执行的查询调用通过。

中间准备输入可声明 `{"kind":"observe"}`，等待 shell 返回就绪输入区，并通过原生 trace 拒绝期间意外启动的模型会话；它不推断命令成功，最后一条仍必须是 `agent`／`assist`。历史 Next 场景由最终宿主记录另行验证先前非零退出、当前零退出、命令和 cwd，并要求恰好一次辅助。

会话 `open` 直接记录实际 system、tools 与采样配置；只有一套工具模板，没有单调用模式或对应的 trace 字段。

Generate/Fix/Next 都由 nosh 以单 User 任务包提交请求和上下文，元数据平铺、原文分块；终答反馈也使用 User。observation 必须标记 `input_format: "command_assist_v1"`。Fix/Next 附结构化 `execution`（完整 command、绝对 execution_cwd、command_id、exit、status），Generate 不带执行记录。观测器验证宿主字段，不从显示文本中的 `[execution]`、cwd 行或 JSON 反推身份。CommandAssist 不提供 `ask_user`，Agent 的交互观察和运行时问答证据独立保留。

Fix observation 的 `captured_output` 保存原始采集身份、耗时、字节计数和全部质量标记。执行身份以 `execution` 为准；不从省略了计数的模型文本逆推元数据。

Next 可附 `recent_executions`，最多三条完整宿主记录，ID 严格递增且小于当前执行 ID。观察器验证意图、类型、绝对 cwd、退出码与状态；模型 User 内的历史摘要仍是数据，不能伪造宿主身份。命令／cwd 的展示裁剪不改变完整审计字段，缺失记录不补造。

`InteractionTrace` 增量读取 JSONL，每个完整记录只解析一次，分别跟踪已完成的命令辅助结果与正在等待用户输入的问题；读取刷新不隐式返回某一类结果。跨写入的 UTF-8 和未完成行留待后续读取，截断、消失或损坏显式报错。最终观测器独立验证完整会话证据。该重构不改变模型输入、轮询完成条件、seed 或评分；推理仍占主要耗时，线程数通过 `--threads` 显式设置并记录，不靠缩短任务期限提速。

PTY 驱动固定 `PS1` 和 `PS2` 为评测标记，追踪自动折行、滚屏及显式续行的逻辑输入起点；当前光标行不必再次包含 PS1。识别仍要求宿主 completion，普通输出或没有起始提示符的 PS2 不算输入就绪。这样长／多行建议只被保留和清空，不会被误执行或因预填换行误报超时；终端仍为 160 列，没有靠加宽掩盖边界。

新版编辑器可能把超长预填的 PS1 完全裁出可视区。仅在已收到前台 Assist 的成功命令记录以及完成摘要、统计行后，驱动才允许发送一次 Ctrl+U 清空该草稿并等待提示符恢复；不发送回车来探测就绪，也不清空仍在生成的任务。完整建议仍保留在原生记录中，评分内容不变；实际 CLI 回归覆盖超屏 ASCII／中文多行及写文件建议，确认没有执行。

接受结果需同时匹配会话身份、执行 command ID／退出状态和实际生成的终态。三种意图必须声明 `observation.response_format = "command_or_none"`，核对正常结束、无调用／解析错误的完整最终文本，精确 `[None]` 才是无建议。缺失格式字段、`finish/clarify` 终态或失败／取消记录携带已接受结果均拒绝，不从解释中提取命令。`-s` 的 stdout 必须与已接受命令一致，none 的 stdout 必须为空。

交互 `agent` completion 可声明 `answers` 字符串数组。每个回答必须是非空、可打印的单行文本，最多 4,096 UTF-8 bytes；按实际问题顺序发送，不从模型问题中猜答案。驱动同时看到原生 trace 中已注册的单一 `ask_user` 调用和终端 `answer>` 就绪提示后才填写。回答按 engine／sid／step 去重，必须在原会话的 Tool 回复中以自包含 JSON 观测到，问题、选项与原文回答逐项匹配。`questions` 与执行审批 `approvals` 分开，不能增加确认计数或授权执行。没有预设回答的提问按 Ctrl-C 取消并判失败，声明但未发生的问题也判失败。

`zh-clarify-task` 使用原请求“帮我处理一下”，在提问后固定回复“先不要执行任何操作，请结束本次任务。”。评分核对原生问题确实询问缺失的任务目标、回复已回填、回复后没有其他工具操作、最终正常结束且不再追问；不再要求最终回答重复问题。总模型步数仍最多 2，试验外部 deadline 不变。选择／自由输入、重复提示、同会话回填、取消及证据错配有真实 PTY 和无模型回归覆盖。跨 revision 的结果不视为单一改动对照。

自动辅助在版本校验和发布成功后才记录 completed；在发布前被新输入取代的结果记录 cancelled，即使模型已经生成合法候选，也不能作为接受成功的证据。

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

两个采集场景显式声明 `capture_output: "last"`，并通过真实 CLI 配置启用。其他场景未声明时继承当前二进制的默认值，报告用 `settings.capture_output = "binary_default"` 记录这一策略；当前默认 `last`。需要控制变量时可为场景显式设置 `off`；不默默替换未覆盖的设置。

夹具明确输出 `diagnostic_id: CAPTURE-...`、`error_code: REGION_UNSET`、报错文本与 `exit_code: 17`；诊断标识由完整 trial 身份的 SHA-256 前缀确定，不依赖运行时随机数，也不预置在用户输入中。第二次执行不再产生原诊断。场景要求 native trace，终端回显、后续工具读取、错误命令 ID、空/混流/不可用记录均不能冒充首请求证据。两个场景预算均为 3 步、0 次确认，不因结果较差放宽。

评分在 `grading.facts.components` 中分别记录 `capture`（证据、关联、单次执行）、`diagnosis`（原因、修复及已覆盖的无依据断言）、`citation`（仅专门引用场景要求）。普通诊断不必复述 trial 专用标识；引用场景漏标识仍失败，但不能把它归为采集失败。报告逐项列出通过、失败、N/A 和未观测数量。无依据的 Kubernetes/内容审核用途、已覆盖的退出码臆测等有正反例约束；这些规则不是完整语义真实性证明，不把“未检出已知错误”夸大为所有解释均正确。

可单独运行新诊断场景，结果写入新的目录，不覆盖任何历史基线：

```bash
python3 -m eval --model-path MODEL_DIR --binary NOSH_BINARY \
  --scenario captured-diagnosis --scenario captured-diagnostic-id --repeat 2
```

零输出、截断、混流、全屏、信号和 100 MiB 有界缓存由无模型真实 PTY 回归覆盖，不把这些测试计为真实模型通过率。采集不新增日常输出日志；本评测显式开启的 `NOSH_EVAL_TRACE` 仍记录输入证据，遵守私有文件和来源记录契约。

新增单轮中文请求都不带 `#`，不替换成 `-a`。Rust、Node、Python 和 dirty git 都是可运行的无外部依赖小项目；原混合语言夹具保持不变。

基础任务的完成条件：

| 任务 | 明确的完成条件 |
|---|---|
| 提交改动 | 使用只有预期已暂存改动的夹具，不混入范围不明的未暂存修改；仍要求正确提交内容、parent 和干净状态 |
| 查看改动 | 准确说明文件及变化即可；暂存标签可以省略，显式说错或否认真实改动仍失败 |
| 行数统计 | 区分逐文件明细与语言总计，接受正确的完整表格；错误计数、重复/遗漏和矛盾汇总不能通过 |
| 需求澄清 | 通过 `ask_user` 请求缺失目标，并处理真实回复；泛泛的继续操作建议不能代替交互 |
| 构建、测试、失败诊断 | 允许夹具内合理的受限执行路径；额外步骤/确认仍由独立体验门槛计分，不用人为拒绝制造失败现场 |

当前回归包含 27 个场景，使用以下解释规则：

| 修正 | 边界 |
|---|---|
| tar 归档等价 | 支持 `tar czf`、短／长选项及源目录内容的受限 `*`／`?`；按操作数位置解析 `-C`，只允许声明源目录及子项，在副本中校验 gzip、完整成员与内容，不执行任意 shell |
| 版本查询 | 识别受限重定向和字面后备提示，仍要求成功执行及精确输出；横／纵表格按列关联，空值、未安装和矛盾版本不能被另一正确值掩盖；比较约束不冒充安装版本 |
| 行数占比 | 区分“某语言占总共 N 行中的 M 行”的分量与总量，二者都校验；矛盾或错误数字仍失败 |
| Python 文件作用域 | “没有／无 Python 文件”不污染后续目录；数量按同一分句的显式目录或当前标题计算，递归范围需明确；临时目录段落与 Markdown 标题分别处理 |
| 进程名称 | 正确 PID 加 Python 或 Python 3 均可；显式错误主版本仍拒绝 |
| 回答语言 | 在路径／文件名清洗前匹配完整 Git 原文及常见行内标记，再排除已知 hash；不把去掉文件名后剩余的普通词当成整条原文 |
| 最近提交 | 区分“仓库最近 N 个”和“仓库共 N 个”；数量需符合范围，重复项、未知列表项、把分支标签算成提交均不能通过 |
| 中文收尾 | 识别无问号的条件式继续邀请；保留必要澄清和不索要回复的直接建议 |
| 编译错误位置 | 接受可唯一定位的文件名；位置遗漏与类型解释分开报告，反向类型检查只针对明确的肯定断言 |

报告继续记录裁判源码哈希，并通过 `dataset_revision` 区分语义。采集评分只接受当前 System context 中的失败命令、退出码、原始输出及独立请求，不要求模型复述路由标签。当前评分不用于重评旧协议记录，旧报告与归档不改写；对照须使用相同输入契约和裁判。

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

观测只接受 `NOSH_EVAL_TRACE` 的私有版本化 JSONL；缺失或损坏直接报错，不从终端文字猜测回答、步数或 TTFT。终端／CLI 完成记录仍用于确认任务状态；建议模式必须有宿主接受的最终结果。必须观测不到的数据不填 0。

正常完成和超时恢复共用逐引擎／会话生命周期校验：需要引擎加载记录、唯一的会话打开记录和配对的模型步骤；拒绝错配、重叠、关闭后继续生成等矛盾记录。会话可以在任务结束时仍保持打开，但不能把未结束步骤当成完整结果。TTFT 取第一个开始步骤对应的结果，而不是并发记录中最先结束的另一步骤。

启动、审批/终端协议、退出阶段超时属于观测/基础设施错误。任务阶段的总时限耗尽仅在 native trace 能确定边界时归为任务失败：或者此前步骤完整且恰有一个模型步骤仍在生成，或者最终 `end_of_turn` 已完成、无工具调用/调用错误，但 REPL 完成标记尚未出现。前者没有最终回答；后者保留 trace 中的最终回答作为证据，但仍判超时失败。其他不完整或矛盾观测仍是基础设施错误；不伪造未完成步骤的 TTFT。

报告保存模型、二进制、工具、场景和评测器哈希及原文证据。`--build-info` 需要 schema v1、完整 `source_revision`、匹配的 `binary_sha256`；没有时注明来源未验证，不把当前 checkout 当作被测构建。

双跑比较**判定和最终状态**，输入/回答/工具差异另列。固定 seed 不固定任务时间、命令耗时或 PID；新进程只保证冷会话/KV，不保证冷 OS 页缓存。报告记录 `dataset_revision`；跨 revision 比较会明确警告，即使仍可配对场景/seed，也不把差值当作受控回归结论。

通过样本必须有最终状态对象，缺失状态不能算“通过”或“双跑一致”；输入和工具证据各自判断是否可观测。报告按场景、seed 和重复编号的范围校验身份，不预先分配全部计划身份的笛卡尔积；完整性检查在验证唯一且在计划内之后核对数量。统计缓存每次重新生成，过期的采集分项表不会残留。

## 基线生命周期

正式测量前固定源码、二进制、模型、套件、seed、预算和裁判；每个计划身份都要记录，通过、模型失败、基础设施错误和缺失分别统计。禁止为追求通过率重抽失败样本或把本地纠错、重复条目混入模型能力分母。

结果写入独立输出目录，记录当前协议和哈希；不同裁判版本或 cold/resident 模式的分数不直接合并。仓库只维护当前套件、观测和报告结构，不保留历史基线副本或独立归档复核工具；旧材料由 Git 历史保存。

源码构建只接受当前受管布局：Reedline 路径依赖、brush-core / brush-parser 路径覆盖，以及两个依赖各自的 pin。默认 provenance 必须同时包含 `reedline` 和 `brush-core`；缺项、额外项、单依赖对象或不支持的 schema 明确拒绝，不自动适配。源码 revision、锁文件和二进制哈希仍用于精确复现，不表示兼容任意历史布局。

## 开发与无模型自测

| 模块 | 职责 |
|---|---|
| `__main__.py` / `campaign.py` | 唯一 CLI、计划预览／预算、串行 campaign、退出码与保存 |
| `contracts.py` | 当前 check 到夹具、评分族和审批的唯一关系表 |
| `suite.py` / `scenarios/` / `suites/` | 内置名称／路径解析、显式目录展开、独立内存校验、场景单一来源 |
| `runtime.py` | 工具预检、隔离配置、模型定位、源码／二进制／运行器来源与哈希 |
| `trial.py` | 单次试验的准备、执行、观测、判分、原始日志保存和清理 |
| `driver.py` / `observations.py` | PTY/进程生命周期、显式完成事件、原生观测解码 |
| `fixtures.py` / `approval.py` | 夹具、隔离、快照和用于比较的最终状态；受限命令解析/审批 |
| `checks/` | `files` / `git` 按事实域承载解析；`agent` / `project` 组合任务判定，`workflows` 检查跨操作的事实与证据，`command_assist` / `capture` / `experience` 各自独立，`common` 共用文本解析，`__init__` 保留统一接口 |
| `report.py` | 每次保存只计算一份汇总，JSON／Markdown 共用；独立渲染会重新计算，不信任过时缓存 |
| `checkpoint.py` | 预检报告、来源及逐次日志身份后复制；失败回滚本次创建的目标，不改实时结果或已有检查点 |

数据流为 **套件校验 → 环境/来源预检 → 单次执行 → 原生观测 → 事实/体验判定 → 报告/检查点**。单次执行始终在 `finally` 保存可得日志并清理自己的夹具；未完成、基础设施错误和模型失败仍分别记录。

当前维护面只覆盖现行场景与结构化观测，保留串行执行与每次试验后的原子保存。报告统计复用只减少辅助路径的重复工作，不宣称模型推理提速。

新增场景优先复用既有 check：在 `scenarios/` 定义一次并加入所需 `suites/`。只有新增判定类型才扩展 `contracts.py`、对应评分器、必要的夹具／审批实现及正反例；不在加载器、运行器和评分入口各维护一份关系表。评分统一通过 `checks.judge`；其余函数直接从各职责模块导入，不设置兼容转发层。

```bash
cargo build -p nosh-cli --locked
NOSH_TEST_BINARY="$PWD/target/debug/nosh" python3 -m unittest eval.tests -v

# 按职责选择无模型回归
python3 -m unittest eval.tests.test_cli eval.tests.test_suite eval.tests.test_observations
python3 -m unittest eval.tests.test_agent_checks eval.tests.test_command_assist

```

测试使用真正的无依赖小项目构建/测试、脚本化 PTY、当前协议正反例和构建来源完整性检查，不加载模型。设置 `NOSH_TEST_BINARY` 后还运行真实 CLI/PTY 与脚本化 worker 的集成用例；CI 默认启用，不设置时会明确跳过这些用例。长草稿可能遮住提示符，驱动须等任务完成后的编辑器重绘再清空草稿，避免启动光标查询吞掉按键；不缩短输入或放宽超时掩盖交接问题。

统一使用 `eval.tests` 包入口，避免把运行时子包作为顶层包加载。CLI 观测的 Rust 回归可运行 `cargo test -p nosh-cli --locked`。无模型测试不能替代真实模型测量；需要的 Node/npm 等工具仍须按上方工具链说明准备。
