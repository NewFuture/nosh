# 真实模型评测

评测分成 **Agent 回归**、**CommandAssist 专项**、**真实工作流专项**和小规模 **smoke**，共享驱动、观测、评分和报告。场景只在 `scenarios/` 定义一次，`suites/` 显式选择场景 ID 和运行参数。原有三套套件保持 `dataset_revision: 12`；新增 `workflows` 为 revision 13，包含 7 个新场景并复用 1 个明确请求对照。正式套件固定 seeds `[0, 1, 2, 3, 4]`，历史报告不重评。

| suite | 范围 | 单轮／双跑 |
|---|---|---|
| `suites/regression.json`（默认） | 原 27 场景回归，含本地纠错和 CLI 命令生成；显式关闭完成事件辅助，避免混入 Agent 统计 | 135／270 |
| `suites/command-assist.json` | Generate、查询帮助后生成、必要澄清、自动 Fix、自动 Next 无建议 | 25／50 |
| `suites/smoke.json` | 本地纠错、Agent、Generate、Fix、Next 的五个既有场景，seed `[0]` | 5／10 |
| `suites/workflows.json` | 有依据的 Next、部分执行后的 Fix、拒绝、只读检索正反例、多轮 cwd、自然缺参与明确请求对照 | 40／80 |

事实/状态与适用的体验门槛都通过，任务才算通过。模型评测只在本地显式运行或手动触发，不进入每个 PR 的 CI。

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

框架只支持当前协议：suite/report 为 schema v2，必须显式记录 `dataset_revision`，模型结果必须有原生 trace；不再提供 `--legacy`、旧 schema 适配、旧入口或 helper 转发。原生 trace 与 build-info 各自使用 schema v1，这与已移除的 suite/report v1 不是同一种协议。`--compare` 也只接受当前报告格式。

默认输出为 `eval/results/<run-id>/report.json` 和 `report.md`。`--output` 必须是尚不存在的目录；`--label` 只是名称，不证明构建来源。默认推理线程 8、Rayon 1；单次试验期限来自所选 suite：回归 240 秒，专项和 smoke 120 秒。可用 `--threads`、`--timeout` 调整。模型和工具不会自动安装。

`--device` 默认仍显式 `cpu`，不随生产程序默认 `auto` 改变历史基线。可选 `cuda`／`cuda:N` 或 `auto`：自动模式由引擎按构建与可用显存选择，并必须原生记录具体的 `device`、`device_requested = "auto"` 和非空 `device_reason`；旧二进制没有这些观测时拒绝 auto 评估。评估会将设备写入**每个隔离 trial 的配置**，不读取宿主的 nosh 配置。所有设备模式都继承并在 settings 中记录 `LD_LIBRARY_PATH`：CUDA 构建的二进制即使选择 CPU，也必须先由动态链接器加载运行库。只有 CUDA／auto 额外继承 `CUDA_VISIBLE_DEVICES`、`CUDA_DEVICE_ORDER`，也会记录；不调整用户的 GPU 占用或驱动。显式 CPU/CUDA 的实际设备必须与请求一致，否则是观测错误而非通过。旧版 native-v1 没有 device 字段，只允许作为 CPU 证据；新报告保留每个 engine 的元数据。CPU/GPU settings 或逐 trial 实际设备不同都会触发非受控对照警告，auto 可能在不同 trial 选择不同设备，不能混成固定 GPU/CPU 基线。GPU 型号、驱动、显存／利用率、构建 flags 和并发负载应另存实测来源；RSS 不含 GPU 显存。

真实运行退出码：**0** 全部通过且重复结果一致，**1** 判定失败/不一致，**2** 基础设施或观测错误，**130** 中断。`--plan` 的 0 只表示计划有效，不代表模型通过。失败、超时和未完成试验不从计划分母中消失。

中断或基础设施失败时会尽力保存部分报告；若保存也失败，会明确输出警告并保留原退出类别，不用二次写入错误掩盖中断或原始故障。

## 冷进程与驻留引擎协议

`--execution-mode cold`（默认，保持旧运行口径）每个 trial 启动独立 CLI 并加载模型。
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

Git 归档不包含子模块内容。工作流在 Rust cache/Cargo metadata 之前，运行**被选中归档自身**的源准备工具，按固定上游提交和同仓补丁物化 Reedline。`source-dependencies.json` 及 build-info 的 `source_dependencies` 记录上游、补丁、修补树和固定时间戳源码归档哈希；构建后再次严格校验，并逐文件核对原 Git 归档，防止 metadata/cache 步骤悄悄改写锁文件。旧源码布局明确记录为 `legacy`（仅指依赖来源布局，不是旧评测协议）；托管布局缺少准备工具或补丁则报错，不能回退到原版依赖。详见 [维护说明](../docs/REEDLINE-MAINTENANCE.md)。

Hosted 工作流显式使用 **每次试验 60 秒**期限，本地默认仍为 **240 秒**。270 个试验即使全部耗尽期限，试验时间也为 270 分钟，六小时 job 留有 90 分钟用于构建、夹具、检查点和上传；构建前还会按实际场景、seed、重复次数检查预算，至少预留 90 分钟非试验时间，超出直接报错。准备阶段另设超时，不把正常的最坏评测计划留给 job 强制截断。

`workflows` 独立双跑为 80 个试验，hosted 试验期限预算为 80 分钟；本地清单期限为每次 120 秒。它不修改原回归的 270 个采样身份，也不通过减少 seed 或缩短原有期限容纳新场景。

这不是静默缩短正式采样：有效 `timeout_s` 保存在报告与工作流来源中，与本地/历史 240 秒运行比较时会产生设置差异警告。需要 240 秒期限的完整基线应在可提供足够运行窗口的本地/受控主机上执行；不能直接提高 hosted 时限而跳过预算检查。

运行时使用 `--ref` 的评测器，分别记录被测源码分支/SHA、评测器和模型来源。首个检查点在 5 分钟后保存，此后每 45 分钟一次，最多八份、保留一天；最终 artifact 为 `evaluation-<run-id>-<attempt>`。检查点复制已结束试验，不改判定或 seed，也不能跨机器拼接成正式双跑。完整性检查逐一核对声明的场景/seed/重复身份，不以过时的固定数量判断完成。

**工作流绿色不等于模型全通过**：完整采样、无运行器错误即可成功；模型失败及原始退出码仍保留。runner 失联时保留已有证据，修复后完整重跑，不选择性重抽失败 seed。快照复制/压缩可能影响耗时，比较时需注明。

## 场景与判定

[`scenarios/agent.json`](scenarios/agent.json)、[`scenarios/command_assist.json`](scenarios/command_assist.json)、[`scenarios/shell.json`](scenarios/shell.json)、[`scenarios/workflows.json`](scenarios/workflows.json) 保存 39 个唯一场景定义。场景保留输入、夹具、审批、`completions`、`expect`、`capture_output` 和可选 `assistance`，不按平台、语言或 seed 复制。

[`suites/regression.json`](suites/regression.json) 与 [`suites/command-assist.json`](suites/command-assist.json) 分别引用原回归和专项场景；[`suites/smoke.json`](suites/smoke.json) 只选已有场景，不另造评分标准。清单的 `catalogs` 使用相对清单文件的显式路径，`scenarios` 是有序 ID 列表。不使用隐式 glob 或按目录排序决定试验顺序；重复定义／选择、未知 ID、缺失文件、非法字段均报错。

加载后展开成 schema v2 完整场景结构，再执行统一校验。报告保存展开后的场景与 `suite_sha256`，因此单纯移动定义不改变场景哈希；外部数据也须使用当前 schema v2 并显式声明 revision、体验预算和 REPL 完成事件。运行器／评分器哈希覆盖 `checks/` 下全部 Python 模块，不把测试和历史裁判归档算作当前运行时代码。

check 的 CommandAssist 终态和专用意图也在 `contracts.py` 绑定：例如 Next 审阅不能配置成 Generate，部分归档修复不能配置成 Next。矛盾配置和 REPL 上无效的 stdin 声明会在预检时报错，而不是开始采样后才失败。

revision 10 的协议变化：裸 `#`／`ai fix` 现在生成修复建议，因此旧缺文件解释场景改为显式 `ai fix <question>`，保持诊断目标；归档建议允许最多 4 个模型步以覆盖查询循环，不再假定单轮无工具。其他原场景的目标、审批和预算保留。Agent 的 System context＋独立 User 请求、分项采集评分和确定性 `diagnostic_id` 不变；历史结果不重评。

revision 11 修正语义审阅中确认的误判，不增加模型裁判或放宽任务目标：

| 修正 | 边界 |
|---|---|
| 故障诊断 | 未设置 REGION 本身不算修复；否定的修复动作、空 JSON 配置、类型／加减方向颠倒、否认端口冲突及仅关闭防火墙均有拒绝反例 |
| 文件与进程事实 | 检查逐文件明确报告的大小（允许十进制／二进制常见单位和显示舍入）、cwd 列表多报文件、错误 PID 归属及已覆盖的否定表达 |
| Git 与澄清 | 接受按句关联的提交摘要、求差／差值等正确释义和口语澄清；拒绝新增的未知提交组件、已覆盖的提交动作反转，以及把自行选择参数伪装成澄清 |
| 帮助查询 | 必须实际成功查询 tar；其他程序的 help 返回不能代替它 |
| 安全等价形式 | 接受固定单 job 的 Cargo 参数、当前无 feature 夹具的完整 feature 选择、受限改名的 `-n`／`--no-clobber` 与等价路径、tar 后单个终止分号；外部路径、额外命令和任意扩展仍拒绝 |

这些是有正反例约束的确定性判定，不是完整自然语言真实性证明；未识别的语义仍可能误判。通过率应连同事实与体验分项、原始回答及审批拒绝证据审阅，不能直接解释为通用 LLM 语义正确率。`suggest-archive` 与 `generate-archive` 共享任务输入，后者增加协议约束；`next-no-goal` 仅为不建议的负例，正向覆盖由 workflows 套件提供。

### 真实工作流专项（revision 13）

| 场景 | 实际前置条件与验收 |
|---|---|
| `next-review-after-tests` | 本地 Python 项目有暂存和未暂存修改，AGENTS 明确测试成功后先用 git diff 审阅。真实 unittest 成功后，Next 提出可展示待审改动的只读命令，不接受一律 none，也不允许自动暂存／提交。 |
| `fix-partially-completed-archive` | 用户的 `mv ... && tar ...` 已搬走报告文件，随后因备份目录不存在而失败。Fix 只准备缺失目录并继续归档，不重放 mv；原夹具只允许已发生的用户改动。 |
| `respect-rename-denial` | 对有效改名提议实际拒绝一次；要求拒绝结果回到模型、文件不变，并如实说明未执行。重试／换命令会受到原审批及确认预算约束。 |
| `piped-config-lookup` | 将 incident.txt 真正作为 stdin 传给 `-a --json`。附件只有 active profile，endpoint 和团队必须从两个配置文件检索；要求实际返回的内容和只读工具能力，不能只输出猜中的答案，也不能在正确答案后追加冲突或臆造的值。 |
| `piped-config-missing` | 同一请求但 profile 不存在；完整读取配置映射或完成对应 profile 的内容搜索后报告未找到，不能用其他环境或猜测的 endpoint／团队填答案。 |
| `cwd-follow-up` | 首次 Agent 任务切换到 data，随后独立请求“列出这里的文件”，不再次告知目标路径；核对第二次请求开始时的真实上下文、最终物理 cwd 与文件列表，不能靠事后切换目录蒙混过关。 |
| `generate-natural-clarification` | “打包那个目录并覆盖上次备份”缺少必要选择，不提示模型“必须先问”；应询问源目录和目标名。 |
| `generate-archive`（复用） | 已明确 logs 和 logs.tar.gz 的请求应生成命令，不应继续澄清；直接引用原有定义，不复制场景。 |

Next 的依据来自本轮实际可见的项目指引、Git 状态与成功命令，不依赖未注入的长期聊天目标。Fix 的 verifier 在私有副本中实际检查归档成员和内容，不回写原始证据。命令校验支持常见受限等价形式（例如 git diff／--cached／HEAD，mkdir／mkdir -p 与 tar 短／长参数），不执行任意模型 shell program。

`stdin_file` 仅用于 Agent 管道附件，必须是夹具内记录的普通 UTF-8 文件、非空且不超过 64 KiB；禁止与 `stdin_command` 同时使用。只读检索的成功以真实返回内容为依据；负例只接受完整文件读取或受限的字面 profile 查询，截断的“未命中”不能证明不存在。`-a --json` 的可见最终回答也须与原生 trace 一致，不能由正确内部记录掩盖错误 CLI 输出。

配置断言按已覆盖的中英文标签、归属表达、Markdown 表格和代码块中的字段逐项核对，不因第一个值正确就忽略重复字段、并列值或额外 URL；不存在的 profile 也不能补猜团队。明确的未知值和已覆盖的否定表达不会被当成正向事实；这些规则仍是有边界的确定性判定，不是通用自然语言裁判。

CLI `done.status` 只接受生产端定义的 `completed`、`incomplete`、`cancelled`、`failed`。其他值属于观测错误，不降格为普通任务失败；`timed_out` 仅由评测器在验证原生超时证据后生成。

对应无模型回归还覆盖完整 campaign 的 0／1／2／130 退出链路、已结束试验保存和未完成分母；这些回归不是新增场景的真实模型成绩。

### 基础回归与采集判定

revision 12 保留每次 trial 不同的诊断 ID，但仅对**确实与本轮初始夹具相同的 `once.py`**在 `final_state` 中标记 `unchanged_from_fixture`，避免动态 ID 导致双跑必然不一致。原始内容哈希仍完整保存在 `facts.before` 和 `file_snapshot`；脚本修改／删除、执行计数及其他文件变化不被归一化。输入和回答的差异仍如实报告，不重写历史复现结果。

本次还检查明确声称的“最近 N 条”是否为连续的最新提交，拒绝不同列表／表格格式下的额外或重复提交；cwd 文件列表区分当前目录、父目录和否定引用。诊断接受 REGION 在动作之前的表达并拒绝动作之后的已覆盖否定；归档澄清分别识别请求的源目录与目标名，不把问题中已经选定的另一参数当成缺失信息请求。跨 revision 对照仍不是受控回归，以上规则也不宣称覆盖全部自然语言表达。

归档字段的问句识别仅在该专项启用，不用“请提供源目录”代替未知任务的目标澄清；行内路径和值也不会在字段判定前被清洗掉。提交摘要的 Summary／包含提交条目的 Notes 区块仍检查全部条目，明确的后续建议不冒充历史事实。目录否定按被否定的文件判断，`without extra files` 不否定此前列出的文件；父目录列表的标题不沿用到下一段或普通比较句。

自动专项用 `completions: [{"kind":"assist"}]` 等待原生 trace 中的宿主结果，不用固定 sleep 或猜测屏幕文字判断完成。`tool_choice` 记录单步 Required／Named 解码策略，宿主预填的调用开头计入输入成本。`SessionSpec.label` 标记意图和前台／后台，不进入 prompt。模型发出 `finish` 不代表成功：只有经过 harness 校验的 `observation` 才能作为 command／clarify／none 结果，错误和取消不能折叠成 none。`finish` 不计为工具执行；帮助查询必须观测到实际返回，不以一个未执行的查询调用通过。归档只在评分器的严格 tar 子集内验证，并检查 nosh 本身未修改夹具。

等待自动结果时增量读取 JSONL，每个完整记录只解析一次；跨写入的 UTF-8 和未完成行留待后续读取，截断、消失或损坏显式报错。该优化不改变模型输入、轮询完成条件、seed 或评分；推理仍占主要耗时，线程数通过 `--threads` 显式设置并记录，不靠缩短任务期限提速。

接受结果需同时匹配会话身份、执行 command ID／退出状态和该步实际生成的唯一 finish。`-s` 的 stdout 必须与已接受命令一致，clarify／none 的 stdout 必须为空；不能用 trace 覆盖错误的 CLI 输出。帮助查询只有返回 `exit=0` 才满足成功查询要求。

自动辅助在版本校验和发布成功后才记录 completed；在发布前被新输入取代的结果记录 cancelled，即使模型已经生成合法 finish，也不能作为接受成功的证据。

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

早期试验数据只读留存，不重新标记此前 6/10、9/10 等定向结果；当前评分器不再支持旧采集入口和消息形态。

可单独运行新诊断场景，结果写入新的目录，不覆盖任何历史基线：

```bash
python3 -m eval --model-path MODEL_DIR --binary NOSH_BINARY \
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

观测只接受 `NOSH_EVAL_TRACE` 的私有版本化 JSONL；缺失或损坏直接报错，不从终端文字猜测回答、步数或 TTFT。终端／CLI 完成记录仍用于确认任务状态；建议模式必须有宿主接受的 finish 结果。必须观测不到的数据不填 0。

正常完成和超时恢复共用逐引擎／会话生命周期校验：需要引擎加载记录、唯一的会话打开记录和配对的模型步骤；拒绝错配、重叠、关闭后继续生成等矛盾记录。会话可以在任务结束时仍保持打开，但不能把未结束步骤当成完整结果。TTFT 取第一个开始步骤对应的结果，而不是并发记录中最先结束的另一步骤。

启动、审批/终端协议、退出阶段超时属于观测/基础设施错误。任务阶段的总时限耗尽仅在 native trace 能确定边界时归为任务失败：或者此前步骤完整且恰有一个模型步骤仍在生成，或者最终 `end_of_turn` 已完成、无工具调用/调用错误，但 REPL 完成标记尚未出现。前者没有最终回答；后者保留 trace 中的最终回答作为证据，但仍判超时失败。其他不完整或矛盾观测仍是基础设施错误；不伪造未完成步骤的 TTFT。

报告保存模型、二进制、工具、场景和评测器哈希及原文证据。`--build-info` 需要 schema v1、完整 `source_revision`、匹配的 `binary_sha256`；没有时注明来源未验证，不把当前 checkout 当作被测构建。

双跑比较**判定和最终状态**，输入/回答/工具差异另列。固定 seed 不固定任务时间、命令耗时或 PID；新进程只保证冷会话/KV，不保证冷 OS 页缓存。报告记录 `dataset_revision`；跨 revision 比较会明确警告，即使仍可配对场景/seed，也不把差值当作受控回归结论。

通过样本必须有最终状态对象，缺失状态不能算“通过”或“双跑一致”；输入和工具证据各自判断是否可观测。报告按场景、seed 和重复编号的范围校验身份，不预先分配全部计划身份的笛卡尔积；完整性检查在验证唯一且在计划内之后核对数量。统计缓存每次重新生成，过期的采集分项表不会残留。

## 基线生命周期

以下均是 **revision 1 的历史基线**。当前 revision 12／13 套件尚无完整固定 seed 双跑基线，也没有用新规则重评或替换旧记录；开发 smoke run 不能代替正式基线，48.0% 不是当前协议下的通过率。

| 基线 | 范围与结果 | 证据 |
|---|---|---|
| main `78b7e50` | 25 × 5 × 2；120/250（48.0%），平均 4.88 步/1.028 次确认；模型 110/240 | [报告](baselines/main-78b7e50-expanded/report.md)、[来源与分析](baselines/main-78b7e50-expanded/analysis.md) |
| main `4f602ab` | 原 10 场景双跑；原始 70/100，经透明判定修正为 73/100 | [原生观测基线](baselines/main-4f602ab/report.md)、[分析](baselines/main-4f602ab/analysis.md) |
| main `7c57a88` | 原 10 场景单轮 legacy；36/50 | [有限观测基线](baselines/main-7c57a88/report.md) |

新基线的全部 250 个身份保留。原始 120/129/1/0（通过/失败/错误/缺失），仅确定性恢复一条真实模型超时的指标并改为失败，另去掉一条不影响通过数的收尾误判，归一化为 120/130/0/0；没有重抽 seed。失联和过窄审批规则的前次运行也保留说明及可得证据。

判定加状态仅 **107/125** 对一致，最终状态 **119/125** 对一致，#3 的复现要求仍未满足。完整报告、原始/诊断记录及一次性材料已迁到 [#4 的归档索引](https://github.com/NewFuture/nosh/issues/4#issuecomment-5844795358)，实际托管在同仓库的非最新版证据 Release 附件；[archive.json](baselines/main-78b7e50-expanded/archive.json) 固定下载地址与 SHA-256。Git 只保留精简指标和来源，原文件在附件中逐字节保留，已有历史不重写。

[复核脚本](baselines/main-78b7e50-expanded/reproduce.py) 是随旧证据保留的独立工具，只接受显式下载的 ZIP，先核验整体及每个文件，再用归档内固定处理源码重建旧报告。常规 CI 不下载附件，也不使用当前评分器重评这些记录。`summary.json` 不是完整报告；旧格式报告不再接入当前 `--compare` 流程。

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

当前维护面覆盖基础 revision 12 与工作流 revision 13 的场景及原生观测流程，保留串行执行与每次试验后的原子保存。不为旧格式增加分支或补默认字段；历史文件只读归档。报告统计复用只减少辅助路径的重复工作，不宣称模型推理提速。

新增场景优先复用既有 check：在 `scenarios/` 定义一次并加入所需 `suites/`。只有新增判定类型才扩展 `contracts.py`、对应评分器、必要的夹具／审批实现及正反例；不在加载器、运行器和评分入口各维护一份关系表。评分统一通过 `checks.judge`；其余函数直接从各职责模块导入，不设置兼容转发层。

```bash
python3 -m unittest eval.tests -v

# 按职责选择无模型回归
python3 -m unittest eval.tests.test_cli eval.tests.test_suite eval.tests.test_observations
python3 -m unittest eval.tests.test_agent_checks eval.tests.test_command_assist

# 从 archive.json 的地址下载 ZIP 后，可选地完整复核证据
python3 eval/baselines/main-78b7e50-expanded/reproduce.py --archive PATH_TO_ZIP --check
```

测试使用真正的无依赖小项目构建/测试、脚本化 PTY、当前协议正反例及归档完整性检查，不加载模型，也不要求当前判定器复现旧分数。统一使用 `eval.tests` 包入口，避免把运行时子包作为顶层包加载。CLI 观测的 Rust 回归可运行 `cargo test -p nosh-cli --locked`。无模型测试不能替代真实模型基线。
