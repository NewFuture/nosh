# nosh 当前系统设计

本文只描述当前实现的行为、配置与限制，不维护历史版本、实施计划或未交付方案。功能介绍见[项目 README](../README.md)，构建、crate 依赖和开发入口见[架构与维护](ARCHITECTURE.md)，详细功能契约见[文档导航](README.md)。

## 0. 概述

nosh 是兼容 Bash 的本地 AI shell。普通命令由嵌入的 brush 执行；Agent 与用户共享同一 shell 会话，模型输出不直接获得执行权限。

### 0.1 使用方式

```bash
nosh
nosh -c 'printf "%s\n" hello'       # 纯 shell，不加载模型
nosh script.sh                     # 纯 shell 脚本
nosh -a "解释当前项目的构建方式"     # 一次性 Agent
nosh -s "列出最大的十个文件"         # 只输出命令建议
nosh init bash                     # 只输出现有 Bash 的集成脚本
```

交互输入以 `# 任务` 显式请求 AI，`#subcommand` 调用内置命令，单独键入 `#` 自动弹出本地命令补全，`#help` 显示帮助。执行命令、生成草稿和输入诊断是不同路径，不把“建议有效”当作授权执行。

### 0.2 术语

**Shell 会话**持有 cwd、变量、函数和作业；**Agent**协调模型、工具与审批；**CommandAssist**生成不自动执行的 shell 草稿；**ChatEngine**是模型无关的消息与事件接口。

### 0.3 实现状态

支持 Linux（含 WSL）和 macOS 的本地 shell、共享会话、Agent、命令辅助、模型下载与离线使用。默认 CPU 推理，可显式构建单卡 CUDA。原生 Windows shell、远程会话服务、跨进程共享模型、沙箱和自动回滚均不提供。

现有 Bash 可通过 `eval "$(nosh init bash)"` 接入主动建议，要求 Bash 5.1+、交互 Readline 和可用终端；不替换宿主 shell。其它 shell 的初始化集成尚未提供。

## 1. 目标与边界

运行时用 Rust 实现，默认构建不要求 Python、Node 或 CUDA。模型未就绪时普通 shell 仍可使用；输入辅助和补全不依赖模型。

风险分析与审批不是沙箱。普通用户命令直接执行，Agent 工具按权限策略执行；允许执行不代表操作可撤销、原子或已备份。`--offline` 只约束 nosh 的模型下载与探测，不限制 shell 命令或补全脚本联网。

## 2. 默认模型：MiniCPM5-2B

### 2.1 模型与格式

首选默认 ID 为 `minicpm5-2b:q8_0`。未显式指定模型 ID 或路径时，在装配引擎、加载权重之前按可用主机内存和可用单卡 CUDA 显存选择：

| 可用资源 | 默认型号 |
|---|---|
| 内存或单卡显存任一达到 **7 GiB** | `minicpm5-2b:q8_0` |
| 未达到 7 GiB，但内存或单卡显存任一达到 **3.5 GiB** | `minicpm5-2b:q4_k_m` |
| 内存和可用单卡显存都低于 **3.5 GiB** | 不自动选择、安装或加载模型，明确提示资源不足 |

内存与显存不相加，多卡显存也不相加：自动设备设置取可见、可探测 GPU 的最大单卡空闲值，显式 `cuda:N` 只看该逻辑卡。显式 `cpu`、CPU-only 构建及不支持 CUDA 的 f32 KV 不使用显存提升默认档位；没有可用 GPU 时只看主机内存。主机内存足够选择最高档时无需初始化 CUDA 来选择型号。

主机检测使用 Linux / WSL 的 `MemAvailable`；macOS 根据 `vm_stat` 的空闲、非活动和 speculative 页及实际页大小估算可回收内存。检测失败会明确警告，仍可依据另一项成功观测选择达到阈值的档位；无法确定两项都低于 3.5 GiB 时保守使用 2B Q4_K_M，不把未知伪装成零。该选择保留在本次引擎设置中，不因加载后的资源变化而重新选择，也不改写用户配置。这些是资源快照，不是总容量或资源预留，仍需满足所选型号、上下文和实际设备的加载预算。

显式 CLI、环境变量或配置中的模型 ID 和路径不受该策略覆盖；短模型名 `minicpm5-2b` 显式选择该系列的首选 Q8_0。资源不足时普通 shell 仍可使用，但已缓存模型也不会自动加载；用户可用 `--model ID`、`--model-path PATH`、`nosh model pull ID` 或手动导入显式尝试，不保证加载成功。

`nosh model pull`、`verify`、`path` 未带 ID 时使用相同策略，`list` 的 `*` 标记本次自动默认型号；无自动型号时仍列出清单，但不标记默认。它们仍使用各自的显式参数，不继承交互模型配置，设备选择按全局 `--device` 或默认 `auto` 探测。只按原有下载／确认策略缓存被选中的型号，不额外下载其它型号。模型使用 GGUF 权重与 `tokenizer.json`，文件、哈希、下载来源和采样默认值以[内置 registry](../assets/registry.toml)为准，运行时核对 GGUF 架构与 tokenizer 词表。

MiniCPM5 使用 ChatML 和基于 special token 的工具格式。模型特定的模板、结束 token、工具调用解析和采样留在 `nosh-llm`，不进入 Agent 或通用引擎契约。

### 2.2 文件与加载

CLI 通过 hub 解析文件，再把明确的 `ModelSource` 传给本地引擎。推理实现不查找模型库、不下载模型；文件信任与解析规则见 §8。

### 2.3 内存

CPU KV 默认使用 f16，可通过 debug 入口对照 f32。CPU 层矩阵和输出投影的权重预重排与释放只在受支持的内核路径启用：x86 的 Q4K、具备 VNNI 的 x86 的 Q8_0、支持 dotprod 的 ARM 的 Q4K/Q6K；token embedding 保留原始块，不把某个平台的释放效果外推到其他平台。实现与补丁契约见 [Candle 说明](../third_party/candle-core/NOSH_PATCH.md)。

内存需求随模型、上下文、KV 类型和设备变化。加载使用显式上下文配置；除 §2.1 的初始化默认选择外，不随剩余内存变化迁移或替换正在使用的模型。`doctor` 的可用内存提示不构成资源预留。当前没有磁盘 KV 缓存。

## 3. 架构

### 3.1 分层

`nosh-cli` 装配 `nosh-core`、`nosh-shell`、`nosh-permissions`、`nosh-llm` 和 `nosh-hub`；`nosh-engine` 提供引擎契约，`nosh-platform` 提供共享宿主能力。准确的生产依赖方向由[架构说明](ARCHITECTURE.md#crate-职责与依赖)和[架构测试](../tests/architecture.rs)共同维护。

### 3.2 运行方式

每个 nosh 进程按需加载自己的模型。前台 Agent 与后台 CommandAssist 交接同一引擎，不各加载一份。`LocalChatEngine` 可保存多份对话日志，但只有一份活动 KV；切换对话可能重新 prefill。

### 3.3 核心约束

Shell 状态由同一 `EmbeddedShell` 持有。权限在执行工具的宿主判定，模型只提出调用。模板和 token 细节由推理适配器封装；平台层不持有模型、审批或提示符布局逻辑。

### 3.4 核心接口与代码入口

接口及调用方见[架构说明：接口边界](ARCHITECTURE.md#接口边界)。`ChatEngine` 使用 `EngineError` 表达可识别的会话、上下文、配置和 I/O 错误，本地实现错误保留在 `Backend` 中。Local、Mock、trace 与评测代理遵循同一契约。

### 3.5 任务流程

输入分流 → 获取当前会话和项目背景 → 模型步骤 → 工具参数与范围检查 → 风险策略与必要审批 → 执行并返回真实结果 → 继续或结束。

CommandAssist 使用独立短对话和受限查询工具，最终命令只输出或回填，不进入自动执行路径。

### 3.6 故障隔离与降级

模型加载、推理、回退和压缩失败均显式报告，不把错误转换成成功或无限重试。普通 shell 路径不要求模型就绪，但推理仍在同一进程，不能隔离 OOM 杀进程等故障。

`--safe` 关闭 AI 并跳过 rc；`NOSH_DISABLE_AI=1` 关闭 AI 但保留 rc。Shell 严重故障按 CLI 的 `fallback_shell` 恢复路径处理，不自动重放已执行命令。

## 4. Shell 核心

### 4.1 引擎与会话

brush 提供 shell 解析和执行，Reedline 提供行编辑。修补源准备与升级见[源码维护](REEDLINE-MAINTENANCE.md)。cwd、变量、alias、function、venv 等状态在用户与 Agent 之间延续。

### 4.2 AI 触发与输入判定

以下是默认交互行为；脚本、`source`、函数体和 `-c` 不经过 AI 输入分流。

| 输入或结果 | 行为 |
|---|---|
| 键入单独配置前缀 `#` | 增强编辑器自动弹出本地命令补全，不请求模型或修复 |
| 无菜单时提交单独前缀，或其后只有空白 | 显示内置命令帮助 |
| `#subcommand [参数]` | 本地解析内置命令；未知名称或无效参数明确报错，不回退到 Agent |
| `# 任务正文` | 前缀后加空白，正文进入 Agent |
| 普通解析错误 | 在自动路由启用时交给 AI |
| 静态可确认的未知命令 | 先尝试本地纠错，否则按自动路由处理；纠错只回填 |
| 用户命令失败 | 按失败策略提示，默认后台生成 Fix，不自动执行 |
| 用户命令成功 | 默认后台生成 Next，没有合理下一步可不建议 |

不完整的 shell 结构继续显示续行提示符；单词内撇号且没有其他 shell 结构的输入可被识别为自然语言。名称预检考虑执行顺序与可确认作用域，不保证展开所有动态命令、替换或函数体，也不判断所有分支可达性。运行时不确定不能伪装成“命令不存在”。

| 开关 | 关闭的路径 | 仍然有效的路径 |
|---|---|---|
| `trigger_on_error = false` | 普通解析错误、无法纠错的未知命令自动转 AI | 本地纠错、撇号分流、安全网、显式入口 |
| `on_failure = "off"` | 执行后的失败提示与自动求助 | 执行前分流、显式 `#fix` |
| `#assist off` | 普通错误/未知命令自动路由，以及自动 Next/Fix | 本地纠错、撇号分流、安全网、显式入口；配置允许的失败提示 |
| `command_assist = false` | 命令完成后的自动 Next/Fix | F2、适用的 Tab 兜底、`-s`、Agent（包括 `#fix`） |
| `NOSH_DISABLE_AI=1` / `--safe` | AI 输入分流、提交时纠错与自然语言安全网 | 普通命令；独立启用的实时输入辅助 |

`#assist off` 不是全局禁用 AI，也不是审批模式。默认自然语言安全网只在破坏性命令的参数满足本地启发式时介入；带选项、显式路径等可能不触发，不构成危险命令过滤器。

模型加载未就绪或失败时，自动错误路由与 Next/Fix 共用加载暂停；普通输入保留本地纠错和 shell 错误，显式 AI 入口及用户确认的安全网仍可重试。恢复契约见 [CommandAssist](COMMAND-ASSIST.md#调度与用户输入)。

失败求助忽略 130、141、148；名单内的 `grep`、`rg`、`diff`、`test` 等只在退出码 1 时按无结果处理。完整实现入口为 [trigger.rs](../crates/nosh-shell/src/trigger.rs)。

### 4.3 共享状态与输出

Agent 命令改变的 cwd 和环境默认保留，可配置任务结束后恢复 cwd。输入辅助、补全和后台建议使用带会话版本的快照，过期结果不得覆盖当前输入。

最近命令输出只保留有界终端尾部，按命令 ID 用于失败诊断，不自动带入普通任务。完整注入、隐私和状态契约见[输出采集](OUTPUT-CAPTURE.md)。

### 4.4 终端与信号

用户命令直连终端；Agent 命令采集输出并有取消、期限和进程清理。检测到需要终端或明确 sudo 密码诊断时，宿主结束任务并交回整条原命令，不自动重试，也不读取用户密码。

复合命令可能已经部分执行，用户再次运行前必须检查状态。命令替换或 builtin 后的管道阶段可能处在 nosh 进程组内，不能保证均由 SIGTTIN 识别；清空环境并脱离进程树的后代也不能保证被回收。作业控制和用户脚本中断仍受 brush 的实现边界约束。

### 4.5 CLI 模式与非交互约定

`-c` 和脚本不下载或加载模型，不增加装饰输出。`-s` 只输出建议命令。`-a --json` 输出结构化事件，诊断留在 stderr；管道 stdin 作为附件时不开放命令执行，终端提问不读取管道。

`-a` 的管道附件与无显式描述时 `-s` 的 stdin 输入共用 1 MiB 字节上限，空输入不作为附件。读取失败在加载模型前报告并以退出码 2 结束，不把已经读取的部分当作完整输入继续运行；`-s` 已有显式描述时仍不读取 stdin。

没有可用终端时，审批不会假装可交互或接受不可见输入。Agent 状态与退出码以 [`TaskStatus`](../crates/nosh-core/src/agent.rs) 为准。

### 4.6 平台

支持 Linux / WSL 与 macOS。原生 Windows 只覆盖可独立运行的共享契约、源码维护工具和编辑器测试，不代表整个 shell 可运行。

## 5. Agent 与命令辅助

### 5.1 入口与生命周期

`# 任务`、`#fix [补充说明]`、自然语言分流和 `-a` 使用 Agent；`#fix` 不因说明省略而切换能力。`-s`、非管理输入的 F2、适用的 Tab 兜底及完成事件使用 CommandAssist，失败后的自动 Fix 建议与手动 `#fix` 分开。任务开始重新获取适用的项目与文档背景。

### 5.2 对话

Agent 可延续对话，按空闲时间、上下文预算或显式 `#clear` 新建。CommandAssist 每个请求使用独立短对话，不复制主 Agent 历史；前台任务开始时取消、回收后台引擎。

### 5.3 主循环

循环消费结构化模型事件。每个工具调用独立经过解析、权限与审批，再返回真实结果；执行失败不被同批后续成功掩盖。步数耗尽可请求一次总结，并使用 `ToolChoice::None` 禁止本轮工具解码；上下文恢复重试也保留该约束，宿主仍拒绝意外返回的调用。取消、拒绝和终端交接分别结束或限制任务，不伪装成完成。

同一任务内，参数与风险上下文均未变化的被拒调用再次出现时，不重复请求审批，而是进入一次无工具收尾。不同作用域的操作仍重新评估；新用户回答或新任务清空该记录，回答本身不授予执行权限。

工具结果处理后立即记录原生宿主观察，与下一轮模型输入分离。已执行命令、未执行调用和无法确认结果的执行分别表示；用户编辑后的实际命令不冒充模型原始提案。执行结果未知或观察记录失败时停止后续调用，保留已有结果，不自动重试可能产生副作用的操作。

### 5.4 Prompt 与上下文

静态 system 在对话内稳定；动态项目背景独立注入，真实请求使用 User。适用的 `AGENTS.md` 优先，否则加载 README 简介与索引；外部正文按不可信数据处理。读取预算、作用域和刷新规则见[项目上下文](PROJECT-CONTEXT.md)。

### 5.5 工具

Agent 使用 `exec`、`read_file`、`grep`，有交互终端时可用 `ask_user`。CommandAssist 只用 `command_help`、`read_file`、`grep`。工具声明顺序、参数、权限、输出预算和结果契约统一见[LLM tools](LLM-TOOLS.md)。

### 5.6 工具调用解析

外层流状态由 special token ID 驱动，调用正文再按模型格式解析；不通过全文搜索字符串识别协议。字段、类型、未知工具和截断错误显式返回。模型解析器在 [toolcall.rs](../crates/nosh-llm/src/toolcall.rs)。

### 5.7 上下文与思考

默认上下文为 8K，包含前缀、对话和生成。新任务前占用超过 85% 时压缩旧工具结果，仍超过 60% 时新建对话。任务内 `ContextFull` 只回退、压缩并重试一次；编码失败不修改日志，恢复失败不丢弃已执行工具的待回灌结果，也不重跑工具。

工具压缩不是模型摘要；Local 保留跨最近消息边界的完整工具组。显式清空或切换思考会重建对话。思考只支持 `on` / `off`，默认关闭。

CommandAssist 对 Generate / Fix / Next 使用相同终答校验和具体拒绝原因，不能通过 `[None]` 掩盖查询失败。完整协议与预算见[CommandAssist](COMMAND-ASSIST.md)。

## 6. 权限

### 6.1 信任边界

模型、项目文档和命令输出不能授予权限。审批和风险分析在工具执行端；用户普通命令不经过 Agent 审批。当前没有内核沙箱、全量审计或自动回滚保证。

### 6.2 风险分析

使用与执行器匹配的 brush AST，结合当前 cwd、环境、路径保护和命令效果分析。静态分析有边界，动态行为未知不等于安全；当前便利性取舍见[审批模式](APPROVAL-MODES.md)。

### 6.3 审批模式与策略

默认 Auto，支持 Confirm 和 YOLO。用户 deny 优先，有效用户 allow 可免审批并覆盖内置禁止；没有 allow 覆盖的内置禁止在 Confirm 中须强确认，Auto / YOLO 拒绝。YOLO 不绕过工具范围、外部认证或用户 deny。

配置无效时安全策略失败关闭，不退回空规则 Auto。普通构建、测试及部分修改操作可自动执行，这明确接受项目代码风险，不代表操作可恢复。完整矩阵和匹配作用域只在[审批说明](APPROVAL-MODES.md)维护。

### 6.4 数据与显示

审批显示实际执行目录、完整命令、风险和有效选项，隔离提示出现前的预输入。命令预览显式显示隐藏字符；显示清理不改写实际执行文本或原始结构化结果。输出与 trace 不是脱敏功能。

## 7. 本地推理

### 7.1 实现与设备

Candle 加载量化 llama，CPU 进行运行时内核选择；CUDA 为显式构建能力。`auto` 在加载时根据设备和显存选择后端，显式 CUDA 失败不回退。GPU 只开放 f16 KV。构建、设备编号和约束见[本地推理与 CUDA](INFERENCE.md)。

CLI 在启动线程前设置推理线程环境，并向 shell 登记原始值，避免把内部覆盖泄漏给用户子进程。默认 Candle 使用物理核，Rayon 为单线程；显式环境覆盖仍有效。

### 7.2 模板与分词

使用 `tokenizers` 的 fancy-regex 路径。静态模板可编码受信 special token，用户、背景和工具正文按普通文本编码。模板 golden 和分段编码一致性由推理测试维护，不在文档复制整份模板。

### 7.3 采样

默认 temperature / top-p / top-k / min-p 为 1.0 / 0.95 / 40 / 0；top-k 为 0 时禁用该过滤。原生采样先按温度缩放，保留 top-k 候选并重新归一化，再应用 min-p 和 top-p；禁用 top-k 时保留原有随机序列。工具调用温度仍为 0.3。重复惩罚在当前生成的末尾 16-gram 于最近 256 token 至少出现三次后启用，系数 1.05，并持续到本次生成结束。

每步重新创建 sampler；固定 seed 不固定任务时间、进程 ID、工具输出或浮点后端差异。候选和去重缓冲按单次生成复用。启用 top-k 时先选择前 k 个候选，再只排序保留部分，避免完整词表排序；相同概率按 token ID 选择，保持与完整排序实现相同的候选、随机数消费顺序和采样结果。

### 7.4 对话日志与 KV

用最长公共 token 前缀截断、复用 KV，仅计算未缓存部分。完全命中也需重算末位置 logits。Assistant 保留生成 token，不经文本重新编码；追加、回退和压缩先完成所需编码，再原子更新日志。

### 7.5 性能口径

吞吐、TTFT、加载耗时和内存分别测量，注明硬件、模型、dtype、上下文、线程、设备与冷热状态。主机 RSS 不包含 GPU 显存；结构重构和无模型用例不构成推理性能证据。

## 8. 模型管理与离线

### 8.1 解析顺序

显式 CLI / 环境路径优先于配置路径；没有显式模型 ID 或路径时先按 §2.1 确定默认精度，然后针对该型号依次查便携、用户和 Unix 系统模型库，再按下载策略处理。不会仅因另一个精度已经缓存而替换所选型号。显式路径无效或目录内 GGUF 无法明确选择时直接报错，不换默认模型或下载替代品。

显式 GGUF 优先使用同目录 tokenizer，否则尝试对应 registry 模型的已校验安装，再尝试该模型用户目录中的 tokenizer。显式 GGUF、同目录 tokenizer 和最后的用户目录文件回退不做 registry 哈希校验，只应使用可信文件。

### 8.2 下载与校验

支持 HF、hf-mirror、ModelScope；自动选源并行探测、顺序下载与故障换源，不是多源并行下载。下载使用文件锁、磁盘空间检查、Range 分块、断点续传和校验后的原子 rename。

已校验文件通过当前 manifest 格式中的哈希、大小和 `FileStamp` 复用结果。版本不支持或记录不完整时缓存失效，重新 SHA-256 校验后重建；不会仅凭旧记录或 mtime 信任文件。

交互首次下载询问，非交互按既有策略在 stderr 提示；`--no-download` 或 `download.auto = "never"` 禁止自动下载。后台辅助只使用已安装模型，`-c` 和脚本不下载。

### 8.3 离线

`--offline`、`NOSH_OFFLINE=1`、`HF_HUB_OFFLINE=1` 阻止 hub 下载与探测。可在联网机器下载后离线导入，不要求运行时网络服务；这不约束用户或 Agent 命令自身联网。

### 8.4 路径

平台目录由 [`nosh-platform::paths`](../crates/nosh-platform/src/paths.rs) 解析。用户模型位于数据目录的 `nosh/models`，状态位于 `nosh/state`，配置位于配置目录的 `nosh/config.toml`；`NOSH_HOME` 同时覆盖数据与配置根目录。便携模型位于可执行文件旁的 `models`，Unix 系统库为 `/usr/share/nosh/models`。

## 9. 交互

### 9.1 Shell 界面

提示符上方信息条显示环境、状态、有效操作和模式；前台程序、AI、审批及下载接管时不后台重绘提示符。布局、主题、关闭路径与已知宿主限制见[状态行](STATUS-BAR.md)。

交互入口采用 `#subcommand [参数]`，前缀由 `[shell] ai_prefix` 统一配置，默认 `#`。增强编辑器中单独键入前缀立即弹出本地命令菜单，采用候选只回填、不提交；前缀后有空白和正文时是 Agent 任务。没有菜单时提交只有前缀或空白的输入仍显示帮助。命令名完整匹配，参数不做 Shell 展开或命令替换；未知命令、无效值及多余参数明确报错，不交给模型。`ai` 是普通 Shell 命令名，没有 nosh 专用含义。

| 默认输入 | 行为 |
|---|---|
| 键入 `#` | 自动弹出本地命令补全；首项 `# <task>` 只补空格，不请求模型 |
| `#help` | 显示帮助，不请求模型 |
| `#mode [confirm\|auto\|yolo]` | 查看或切换审批模式 |
| `#theme [名称或路径]` | 查看当前主题，或校验并自动保存主题选择；无需重启 |
| `#abbr [list\|set\|remove\|enable\|disable] ...` | 查看或管理可见缩写，持久化规则；不调用模型，详见[缩写说明](ABBREVIATIONS.md) |
| `#think [on\|off]` | 查看或开关思考；改变设置时重置对话 |
| `#assist [on\|off]` | 查看或暂停自动错误路由及自动 Next/Fix；`on` 允许在加载失败后重新检查，管理命令本身不加载模型 |
| `#fix [补充说明]` | 由 Agent 处理上次失败，自动附带失败命令、退出码及匹配输出；省略正文时默认诊断，有正文则遵循本次请求，工具调用按 Agent 审批规则执行 |
| `#clear` | 新建对话，不重置 Shell 环境 |
| `#ctx` | 查看上下文 token 占用 |
| `#status` | 查看自动辅助开关、模型可用性、审批、思考及上下文状态，不等待后台加载 |
| `# 任务正文` | 进入 Agent，按现有审批规则执行任务 |

无空格的 `#任务` 是未知子命令；`#auto`、`#next`、`#history`、`#private`、`#undo`、`#model` 不在命令目录中，旧管理名称也不回退为 AI 任务。除显式 `#fix` 外，帮助和管理操作不发起推理或模型下载。

子命令和固定参数支持本地 Tab 补全，`#mode ` / `#theme ` 自动展示参数候选；采用只修改草稿，不执行命令。前缀菜单随输入和删除更新，不打断完整命令或 `# 任务` 的提交；筛选、取消和恢复规则见[补全说明](COMPLETION.md)。未提交的有效名称前缀只标为待完成，提交仍要求完整命令名。管理输入的 F2 和 Tab 无结果不触发 AI。关闭补全时不自动弹出菜单；关闭 AI 时不拦截前缀；空 `ai_prefix` 关闭显式入口及相应提示。中间的 `#`、脚本、`source`、函数体和 `-c` 保持正常 Shell 语义。

### 9.1.1 实时输入提示

输入辅助只读输入与版本化快照，不执行或改写输入。基本终端与非 TTY 不启动该通道，`NO_COLOR` 关闭样式但保留已启用的文字反馈。判定、进程预算与恢复契约见[输入辅助](INPUT-ASSIST.md)。

### 9.1.2 编辑与补全

Emacs / Vi、作用域改键、搜索、取消和建议采用见[输入编辑](INPUT-EDITING.md)。命令、路径、Git、Make、npm/Yarn 和脚本补全见[补全契约](COMPLETION.md)。明确无匹配与查询失败不同，采用候选不等于提交执行。

### 9.1.3 历史输入

交互 nosh 在可进入历史导航的位置按上下键或 Ctrl+R 唤出同一历史菜单，打开后才接管输入筛选，默认全部，不立即替换草稿。历史菜单使用 ↑/↓ 选择、Tab 回填，Enter 保留兼容但默认不单独提示，Ctrl+R 切当前工作树范围；Shift+Tab 不绑定。回填不执行。命令跨次保存到 SQLite，提问只在本次内存中召回。输入、持久化、手动添加/导出和 history/fc 的边界见[历史说明](HISTORY.md)，不与下面的外部 Bash 集成混为一套历史。

### 9.2 现有 Bash 集成

`nosh init bash` 在模型与终端宿主初始化前分派，只读取现有配置并输出脚本；不联网、准备模型或写 rc。加载后，`bind -x` 回调复用现有 `nosh -s` 的受限查询和完整命令校验，stdout 仅承载命令，诊断写 stderr。共享 rc 在 nosh/brush 中加载时跳过外部绑定。

建议只回填，用户自行回车执行；不接管 Bash 的编辑模式、补全、历史、提示符或 `on_failure`，不增加自动 Fix/Next 或 AI Tab 兜底。Bash 5.1+ 的版本依据、取消恢复、冲突与重载规则统一见[绑定说明](INPUT-EDITING.md#外部-bash-的轻量建议入口)。

每次建议由独立子进程读取模型、下载和安全配置，保留初始化时显式传入的 `--offline`、`--no-download`、`--model-path` 等选项；相对 `--model-path` 在生成脚本时转为绝对路径，不要求模型已存在，之后 `cd` 不会改变所选模型。仍有冷启动成本，不承诺共享模型缓存。只继承 cwd 和导出环境，不采集整份历史或同步非导出变量、函数、alias；手动 `-a` 也不能改变父 Bash 状态。

## 10. 进程与观测

正常运行由本地进程持有模型与 shell，输入分析、查询及补全使用有界辅助进程。评测专用 worker 可串行服务独立 trial，协议与生命周期由 CLI 的 `eval_worker.rs` 管理，不是用户共享引擎服务。

`NOSH_EVAL_TRACE` 显式启用私有 JSONL 观测。当前协议、身份校验、超时和测量规则见[评测说明](../eval/README.md)；缺失或矛盾观测明确报错，不从终端文字猜测模型结果。

## 11. 配置

Linux 默认 `~/.config/nosh/config.toml`，macOS 默认 `~/Library/Application Support/nosh/config.toml`；`NOSH_HOME` 覆盖为其下的 `config.toml`。有对应覆盖项时，优先级为 CLI > 环境 > 用户配置 > 默认值；没有项目或管理员配置层。

缺失配置使用默认值。配置加载、主题刷新和缩写管理共用 UTF-8 普通文件读取与 1 MiB 字节上限；读取可跟随配置符号链接，但不等待 FIFO，超限或非普通文件明确报错。配置不可读、TOML 错误或安全规则非法时报告原因并阻止 AI 工具执行，普通 shell 保持可用。非法项目环境配置也阻断 agent 执行，不默认为 off；终端集成配置错误则告警并关闭标记。其余非安全字段按告警与默认值规则处理；未知 section/key 明确告警，不为未实现能力预留静默接受的键。

`shell.ai_prefix` 可为空字符串来关闭显式前缀入口；非空值不得以空白字符开头（包括 Unicode 空白）。非法值明确告警并使用默认 `#`，不静默禁用命令，也不自动去掉配置中的空白。

### 11.1 当前配置示例

```toml
[shell]
ai_prefix = "#"
trigger_on_error = true
on_failure = "hint"           # hint | auto | off
capture_output = "last"       # last | off
project_env = "off"           # off | direnv | mise；显式启用，互斥
terminal_integration = "auto" # auto | off | on；通用 OSC 7/133
input_assist = true
completion = true
completion_scripts = true
command_assist = true
status_bar = true
theme = "soft-dark"           # soft-dark | soft-light | 原生 Starship TOML 路径
starship = false              # 启用提示符边界的 Starship 环境模块
nerd_font = "auto"            # 默认自动选择 Powerline；true 显式启用，false 关闭
nl_guard = "destructive"      # destructive | off
edit_mode = "auto"            # auto | emacs | vi

[shell.keybindings]
ai_suggest = ["F2"]
undo = ["Ctrl+Z", "Ctrl+_"]
redo = ["Ctrl+Y", "Alt+/"]
literal_space = ["Ctrl+Space"]

[shell.abbreviations]
enabled = true
builtins = true
probe_commands = true        # 默认探测 Git / ls；不可信 PATH 应设为 false
disabled = []

[shell.abbreviations.rules]
gmain = { expansion = "git switch main" }

[agent]
approval = "auto"             # confirm | auto | yolo
max_steps = 10                # 1–50
command_timeout_sec = 60      # 1–600
restore_cwd = false
conversation_idle_minutes = 30 # 1–1440

[model]
# id = "minicpm5-2b:q8_0"     # 显式固定；省略时按可用内存自动选择 Q8 / Q4
# path = "/opt/models/model.gguf"
context_length = 8192         # 1024–32768
device = "auto"               # auto | cpu | cuda | cuda:N
thinking = "off"              # off | on

[download]
auto = "yes"                  # yes | never
source_selection = "auto"     # auto | hf | hf-mirror | modelscope

[safety]
allow = []
deny = [{ command_prefix = "docker system prune", reason = "Keep lab volumes" }]
protected_paths = []
fallback_shell = "/bin/bash"
```

键位列表是显式覆盖示例，不是所有原生别名。安全规则使用 TOML 条目，可改用 `[[safety.allow]]` 表数组，但同一个键只能定义一次；条件、路径 glob 与 workspace 作用域见[用户规则](APPROVAL-MODES.md#4-用户规则与会话授权)。

`nosh theme set <name-or-path>` 自动保存主题，无需重启。自定义配色使用原生 Starship TOML；生效时机、静态样式、可选环境模块、Powerline 和保存失败规则见[主题说明](THEMES.md)。

`nosh model` 子命令使用自己的位置参数与选项，不继承交互模式的模型/下载配置。键、默认值与范围的实现来源为 [config.rs](../crates/nosh-cli/src/config.rs)。

`project_env` 的工具版本、完整输入边界、`--safe`/`--norc` 区别及失败恢复，以及 `terminal_integration` 的自动发现和 OSC 7/133 范围，见[项目环境与终端集成](SHELL-INTEGRATION.md)。

### 11.2 环境变量

| 用途 | 变量 |
|---|---|
| 路径与模型 | `NOSH_HOME`、`NOSH_MODEL`、`NOSH_MODEL_PATH` |
| 推理线程 | `CANDLE_NUM_THREADS`、`NOSH_RAYON_THREADS` |
| 离线与禁用 AI | `NOSH_OFFLINE`、`HF_HUB_OFFLINE`、`NOSH_DISABLE_AI` |
| 下载源 | `NOSH_ENDPOINT`、`HF_ENDPOINT`、`NOSH_REGION=cn\|global`、`HTTPS_PROXY` |
| 终端 | `NO_COLOR`、`CLICOLOR`、`TERM`、locale |

## 12. 工程

### 12.1 代码与依赖

只维护当前八个生产 crate 与一个集成测试 package。模块职责、生产依赖约束和源码位置见[架构与维护](ARCHITECTURE.md)；工具链与依赖由 manifest、lockfile 和 `rust-toolchain.toml` 固定，不在这里复制版本清单。

### 12.2 构建

首次构建先运行 `cargo source prepare`。Reedline / brush 的上游、补丁和派生源码按[源码维护说明](REEDLINE-MAINTENANCE.md)管理；不允许缺失补丁时静默改用原版。

### 12.3 维护与扩展约定

直接修改当前接口并同步全部调用方，不增加旧路径转发或未使用的抽象。测试按职责和执行条件组织，不按历史版本分组。生产算法、错误行为与数值门槛不能因结构整理被弱化。

## 13. 测试与评测

### 13.1 覆盖原则

无模型回归验证接口、权限、字符预算、取消、状态新鲜度和 PTY 行为；真实模型用例单独运行，不把 Mock 成功解释成模型质量通过。文档不维护固定的通过数量或旧构建成绩，状态以当前运行产物为准。

### 13.2 测试矩阵

| 层次 | 入口与要求 |
|---|---|
| 共享契约 | `nosh-engine`、`nosh-platform`；消息格式、取消、文件身份和终端能力 |
| 单元与组件 | 对应 crate；有效输入、失败、权限、恢复和资源边界均覆盖 |
| 跨 crate | `tests/flows/` 与 `tests/architecture.rs`；任务流程和生产依赖方向 |
| 终端 | `nosh-core/tests/terminal/`、Shell / CLI 的 PTY 用例；保持输入、输出、授权及终端恢复语义 |
| 真实模型 | `nosh-llm/tests/real_model.rs`、`nosh-cli/tests/arm64_memory.rs` 的 ignored 用例；保持既有数值与内存断言 |
| 评测器 | `python3 -m unittest eval.tests`；当前场景、观测、评分、来源与报告完整性 |

模型长上下文输入使用固定的 [`tests/fixtures/model_context.txt`](../tests/fixtures/model_context.txt)。它只是测试语料，不是当前架构文档；文档修改不得改变输入 token 或验收条件。

真实测量固定源码、二进制、模型、套件、seed、预算、设备与冷热条件。失败样本不重抽，未观测指标不填零，版本或裁判不同的结果不当作受控比较。完整命令与平台矩阵分别见[架构与维护](ARCHITECTURE.md#开发入口)、[CI](../.github/workflows/ci.yml)和[评测说明](../eval/README.md)。
