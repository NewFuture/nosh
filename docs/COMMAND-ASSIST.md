# CommandAssist：命令辅助

当前 LLM 能力分成两类：**CommandAssist 提出 shell 输入，Agent 分析或执行任务**。Generate、Fix、Next 是命令辅助的三个意图，不是三套 agent；共用查询实现、命令校验和模型实例，均直接返回候选文本或 `[None]`。可交互 Generate 通过独立的 `ask_user` 澄清，不使用 `finish`。

## 入口与职责

| 意图 | 入口 | 输出 |
|---|---|---|
| Generate / CommandGen | `nosh -s "描述"`、非空输入 F2、适用的 Tab 兜底 | 生成或改写一个完整 shell program，必要时澄清 |
| Fix / AutoFix | 用户命令失败后的自动辅助、裸 `ai fix`、失败后的裸 `#` | 修正建议或无建议；不提问、不自动重跑或修改环境 |
| Next / NextSuggest | 用户命令成功后的自动辅助、`ai next` | 有依据的下一条命令；没有合理下一步就不建议 |

`ai fix <question>` 保留 Agent 入口，携带匹配的失败证据，支持解释、日志分析和继续诊断。普通 `# <任务>`、自然语言任务与 `nosh -a` 仍走 Agent。

只有交互层直接执行的用户命令产生自动辅助事件。Agent 内部的工具成功／失败继续由原 Agent 处理；不递归启动 Next/Fix。脚本、`-c`、补全、本地拼写纠错、AI 管理命令不产生新的用户执行事件。

成功退出不构成新任务，非零退出也不必然是错误。失败仍沿用 `failure_is_notable` 排除取消、管道信号和 grep/test 等无结果状态。AutoFix 的“自动”是自动触发，不是执行授权；要实际修复，用户需自行执行候选命令或明确交给 Agent。

## Instructions 与 prompt

每次只发送当前意图的一段简短说明，公共部分保留外部文本边界和终态提交约定。不把三类规则同时发送，也不复制 Agent 的全部行为规则。工具描述说明参数和能力，权限、调用次数、取消和生命周期由代码控制。

```text
System：查询工具协议 + 公共背景边界 + 当前意图 + 对应输出约定
System：本次事实、适用项目文档、可选执行证据
User：真实用户请求（存在才发送）
System：宿主 command_completed 候选生成请求（Fix / Next）
Assistant / Tool：必要查询及结果
Assistant：最终命令文本或 [None]
```

所有命令辅助和普通 Agent 使用同一套官方工具模板，允许自然最终回复；不维护单调用模板模式。初始前缀和工具集合在会话内保持稳定。

Generate/Fix/Next 的查询阶段使用 `ToolChoice::Auto`，终答阶段使用 `ToolChoice::None`，不注册 `finish`，不预填工具调用开头。`None` 在 `LocalChatEngine` 每次采样前把现有 `FUNCTION_OPEN` 控制 token 的 logit 设为负无穷，阻止流式解析器进入工具调用状态；不是仅靠提示要求“不调用工具”。初始工具定义、会话历史与采样参数不变，该选择仅作用于下一步，其他会话保持原行为。普通文本仍可能写出 XML、工具名或说明，必须通过原有完整程序校验，不能据此宣称语义正确。执行事件不伪造 User 请求，也不代表用户授权。

主 instructions 与终答阶段复用同一条输出规则：`Return only the complete shell command as plain text, without explanation or Markdown. If no justified command can be suggested, return exactly [None].` 终答阶段重申当前意图，用 JSON 字符串引用原始请求（存在才添加）及已完成命令（Fix/Next），并按发生顺序引用真实、成功的澄清问答，包括选项和完整回答。不概括或裁剪用户要求，不重贴项目背景或查询结果，也不为自动事件虚构用户请求；完整回显计入现有上下文预算。随后要求 `Answer now without calling tools.` 并复用输出规则，不再使用 `[query_budget]`、含糊的 “final response” 或 “Fulfill this request”。终答阶段同时启用上述解码约束；若其他引擎仍返回工具调用，宿主拒绝且不执行。

`SessionSpec.label` 仅供宿主观测区分 `agent` 与 `command_assist.<intent>.<foreground|background>`，不编码进模型输入。没有新增分类模型调用。

Fix/Next 的末尾宿主消息明确要求根据已记录结果、当前项目状态和适用指引起草供用户审阅的候选，并说明候选不会自动执行；不再用“没有任务执行权限”作为临近生成的最后一句。仍使用 System，不伪造人类 User 请求，也不改变查询权限或授予目标命令执行能力。

## Context

| 意图 | 初始内容 |
|---|---|
| Generate | 原始描述／待改写命令、cwd、平台、项目事实与适用指引 |
| Fix | 原命令、command ID、执行 cwd、退出码、匹配的有界终端证据、当前 cwd 与项目事实 |
| Next | 成功命令、command ID、执行 cwd、当前 cwd 与项目事实；不自动附成功输出 |

共享项目与文档采集器，沿用 AGENTS 完整加载、README 摘要和路径保护。指引不完整时明确报错，不猜测剩余规则。每次辅助都是独立短对话，不默认携带主 Agent 历史、最近三条命令或完整环境变量。

`[execution]` 是本次命令身份的唯一完整表示，保留 command ID、命令、退出码与执行目录，并用 `status: succeeded/failed` 明确表示该命令的退出结果，不代表项目干净或整个工作流完成。同一当前目录的 `execution_cwd` 写为 `.`；实际执行目录不同时保留原路径，不能因命令内的 `cd` 丢失起始目录。

Fix 的 `[user_output]` 用 command ID 关联该执行记录，不重复命令、执行 cwd 和退出码；输出来源、耗时、字节计数、截断／不完整／混流等质量信息照常保留。真实命令／cwd 和采集快照不被修改，Agent 诊断入口仍保留原完整证据格式。

shell 的有界命令快照保存 PATH、有效 builtin／alias／function 名称、命令 hash 和解析选项，供后台查询与候选校验使用；不会在后台访问或修改活 shell。它不是文件系统快照，查询仍可能观察到随后发生的文件变化；过期输出不能交付。

Fix 只接受匹配 command ID、命令正文及截断标记、退出码与执行目录的证据，区分缺失、空、截断、不完整、混流。Next 不根据陈旧聊天猜测用户目标；当前自动入口没有额外的目标推断或长期记忆。

## Tools

| 工具 | 能力与边界 |
|---|---|
| `command_help(name, query?)` | Generate/Fix/Next 共用；name 指定命令及可选子命令（如 `tar`、`git commit`），query 是对应帮助中的搜索内容（如 `gzip`、`--amend`），省略时给概览。不提供命令发现、版本或目录查询 |
| `read_file` | 复用文本读取实现和 400 行上限，只读取当前策略允许的普通文件；FIFO、设备等特殊文件不打开 |
| `grep` | 复用内容搜索及扫描预算，每次辅助搜索最多 2 秒或更短的配置期限 |
| `ask_user(question, choices?)` | 仅有交互输入能力的前台 Generate 注册；可选单选建议始终允许自由输入，回答回填原会话继续生成，不是终态或执行审批 |

命令辅助不注册 `exec`，不执行生成的目标操作，即使目标本身只读。查询仍受用户 deny 和保护路径限制；需要审批的查询返回明确错误，不在后台弹审批，不使用 YOLO 绕过命令辅助的能力上限。

`name` 接受以空白分隔的命令路径与子命令名，例如 `git commit`、`git remote add`；不接受命令选项、重定向、管道、变量展开、引号或其他 shell 代码。宿主从快照解析第一个名称，把子命令作为独立 argv 传入，不经 shell 执行。普通程序追加固定 `--help`；Git 子命令使用已知的 `-h` usage 形式，避免 `--help` 启动 man／文档查看器。这不是失败后的盲试，不自动换程序或旗标重试。保留调用路径的 argv[0]，兼容多调用名程序。

权限分析和用户规则检查的是**实际完整调用**。Git 的已知内建子命令及部分多级子命令增加精确的短帮助识别；未知 Git 扩展／alias、其他不能判为 Safe 的调用仍明确拒绝。不会因为最后加了帮助旗标，就将任意程序、脚本或子命令当成只读。shell alias、function、本地／不透明程序和受保护路径的限制不变；builtin／keyword 不替换成同名外部命令。旧 `command_info` 不再注册或分派，`topic` 不保留为参数别名；`query="version"` 只搜索帮助里的 version，不执行 `--version`。

`git stash create` 不属于可查询的短帮助形式：它把 `-h` 当作消息并创建 Git 对象。因此该调用按修改仓库处理，命令辅助即使在 YOLO 模式也拒绝，不执行后再根据退出码猜测是不是帮助。

子进程使用快照中的导出环境与非交互覆盖项，stdin 关闭，独立进程组，有取消、最长 3 秒和 stdout＋stderr 合计 64 KiB 采集上限。帮助正文最多 1,800 字符（含流标签和省略标记），保留原始措辞，去除 ANSI／CRLF 等终端格式。`query` 是忽略大小写的字面文本筛选，不是操作枚举、正则或语义问答，永远不传给程序：关键词／短语按子串匹配（`gz` 可命中 `gzip`）；选项保留名称边界（`-c` 可同时检索 `-c`、`-C`，但不匹配 `--create`）。选项的原始大小写和各自含义完整保留，搜索命中不表示两种选项等价。可选否定写法 `--[no-]amend` 可由 `--amend`／`--no-amend` 命中，返回仍保留原文。返回匹配项及缩进续行；匹配章节标题时保留该章节下的原文块，选项查询优先选项定义而非其他位置的提及。没有 query 时优先 Usage/Synopsis，再从前部选取概览，不拼接任意页尾。长块围绕命中位置截取并保留有界的选项定义。

帮助结果以 `[command_help]` 和一行 JSON 元数据开头，记录请求名称、解析路径、实际 executable、subcommands 数组、实际帮助旗标 argument、真实 exit_code／signal、capture_complete、两路采集字节数、query、matched_blocks 和 excerpt_truncated；正文用 `[stdout]`／`[stderr]` 标明来源，不声称跨流时序。GNU、BSD、BusyBox 风格文本共用同一摘录逻辑，不根据程序名硬编码答案。非零退出的 usage（包括 Git 短帮助的 129）可作为证据，但不伪装成零退出；空输出明确报错。达到采集上限仍做相关性筛选，结束自有进程组，并将退出码记为未知；`capture_complete=false` 与摘录截断分开。未匹配只表示已采集文本中没有匹配，不证明参数不受支持。保留原 locale，不假设所有程序都支持同一种帮助形式。

**帮助查询仍是真实执行，不是沙箱。** 不能仅凭 `--help` 字样把未知项目程序视为安全。文件系统查询也是协作式检查，不能保证强制中断一次阻塞 I/O；现阶段没有跨任务帮助缓存。

## 结果与预算

### Generate / Fix / Next：直接最终回复

正常 `end_of_turn`、没有工具调用且没有解析错误的回复才进入终态校验：

| 最终文本 | 宿主处理 |
|---|---|
| 一个完整 shell program | 校验后显示／预填，仍不自动执行 |
| 精确 `[None]`（允许首尾空白） | 无建议，不显示标记、不伪造命令 |
| 空回复、说明、Markdown、错误格式 | 剩余预算允许时仅转入一次终答纠正；仍无效则明确失败，不推断为澄清或无建议，也不从散文中提取代码 |

查询回合可以带说明文字或多个受限查询；说明只保留在会话／trace，不展示为候选或执行。每个查询仍单独检查权限、取消和期限。合法早期终答立即返回，不增加模型调用。早期正常结束、没有工具调用但正文校验失败时，将拒绝原因作为 System 反馈保留在同一会话，在原预算内仅用下一步进入终答阶段；这是对“无效正文立即失败”行为的调整，不是无限重试。最后预算步也进入相同终答阶段，不允许再执行查询或提问。终答仍无效、解析错误、截断、取消、超时和预算耗尽均明确失败；宿主不改写命令或把错误替换为 `[None]`。模型在纠正步骤自行返回 `[None]` 时仍受最后一次查询失败的防掩盖检查。

### Generate：可交互澄清

`ask_user(question, choices?)` 与 Agent 共用输入能力。question 必需，choices 为可选字符串数组，省略或空数组表示开放式问题。终端用上下方向键选择，直接打字可以输入任意答案，数字文本也不会被误当选项编号；没有默认选中项。工具返回选项原文或输入文本，通过 `Message::UserAnswer` 回填同一会话；模型侧仍是官方工具回复格式，不伪造额外 User 轮次，但不会按旧工具输出压缩丢失用户约束。原生 trace 保留 `role: tool` 和 `source: user`。后续回答可以修改或取消原需求，不能授予执行审批。候选及回答不会作为 shell 命令执行。

仅在 stderr 是终端且存在可用控制终端时注册该工具；使用控制终端读取，不消费 stdin 的管道内容。没有输入能力的 `nosh -s` 不注册该工具，缺少必要用户选择时返回 `[None]`。Fix/Next（包括显式前台入口）及后台辅助均不注册它。Ctrl-C、Esc、Ctrl-D 取消本次任务；输入通道故障明确失败，不退化为无建议。空回答重新提示，不代选选项。

提问必须是该回合唯一的工具调用；不能预先把依赖回答的操作放进同批调用。宿主验证参数和回答：question／回答最多 4,096 bytes，choices 最多 20 项，每项最多 512 bytes、非空单行、互不重复，拒绝未知字段和隐藏控制字符。XML 参数转换本身不是完整 JSON Schema 校验，数组元素等约束仍由宿主执行。用户回答只补充需求，不授予执行权限。

默认关闭思考，每步最多 512 个新 token；Generate/Fix 最多 4 个模型步，Next 最多 2 步，并受更小的 `agent.max_steps` 限制。最后一步保留给最终文本，此步提出的查询或提问不执行。提问占用正常模型步，不增加预算；等待用户的时间从辅助任务执行期限中扣除，仍可取消。任务期限在步骤边界及查询前检查。采样沿用配置，不为特定测例覆盖 seed 或提高预算。

`nosh -s` 成功时 stdout 仅含命令；问题和交互 UI 仅写 stderr。无建议退出 1，错误退出 2，取消退出 130。交互候选只有接受后进入输入行，用户仍需回车。

查询／提问参数错误以工具结果返回，允许在预算内修正；如果最后的查询仍失败，`[None]` 不能把它隐藏为正常无建议，宿主报告查询失败。

## 调度与用户输入

默认 `[shell] command_assist = true`。每条用户命令完成后，成功排入 Next，值得诊断的失败排入 Fix；`on_failure = "off"` 关闭自动失败辅助，`ai auto off` 暂停自动辅助。`command_assist = false` 保留显式 Generate/Fix/Next；`--safe`／`NOSH_DISABLE_AI` 关闭 AI。

一个后台 worker 临时持有既有 Agent 及其推理引擎，只有一个最新任务槽，不加载第二份模型。加载器、Agent 和模型描述作为同一个 EngineState 在前后台移动，不逐项交接独立状态。前台普通命令和编辑不等待推理；显式 AI 请求取消后台工作并取回同一引擎，因此可等待当前推理取消或模型加载完成。主 Agent 的对话日志不因辅助任务而清空，单份活动 KV 在切换对话后可能重新 prefill。

前后台共用一个带 `LoadMode` 的加载入口，后台明确传入 Background，不回退到交互式加载。后台只使用已安装模型，不下载、不读终端、不打印加载进度；仅首次创建 Agent 时在 worker 中探测环境，不在每次命令结束的前台路径重复查询 PATH。错误作为辅助状态显示。输入编辑、新执行和退出使旧版本失效；结果、版本和取消句柄由同一个锁保护，取消回调在锁外执行。

正常 reedline 终端在提示符上方显示候选，F2 接受；接受时再次核对 command ID 和活 shell 的静态校验。基本终端只在输入仍为空时用新行显示结果并重画提示符，同样使用实际配置的建议键；用户开始输入后取消旧任务，不把后台正文插入已有输入。Tab 的补全成功/菜单操作也会使旧草稿候选失效，但明确无补全的兜底可以采用仍有效的现有候选。

启用四区信息条时，后台候选/说明合并进状态区，实际可用的 F2（或改键）采用动作进入操作提示区，不叠加另一行。非空白草稿走 Generate；空白且没有可用候选只提示，不请求 Fix/Next 或 Agent。修复保留显式 `ai fix`。布局只读取既有 `AssistDisplay` 结果，不新增模型请求；关闭信息条保留原提示路径。焦点、改键、回填撤销和资源边界见[输入编辑](INPUT-EDITING.md)。

## 评测

Agent 回归与命令辅助分别使用 `eval/suites/regression.json` 和 `eval/suites/command-assist.json`，从 `eval/scenarios/` 引用唯一场景定义。`smoke.json` 仅选择既有场景的 seed 0 小集合，不替代正式基线。Agent 回归显式关闭自动辅助以隔离任务统计；CommandAssist 专项覆盖生成、帮助查询、非交互缺参不猜测、自动失败修复和成功后无建议，自动场景保持真实完成事件。交互提问的选择／自由输入、取消及同会话续接另有无模型和真实 PTY 回归覆盖。

真实 trace 区分模型工具调用和宿主接受结果。三种意图均必须记录 `observation.response_format = "command_or_none"`；接受结果只允许 command／none，必须匹配会话、command ID、执行状态和完整正常最终回复。没有旧终态的兼容读取或缺失字段推断；历史归档不改写、不用当前评分器重评。`ask_user` 的调用与原文回答保留在原生输入／输出 trace，不算目标命令执行。CLI 还要核对实际 stdout，不能拿 trace 中的正确命令替代错误或缺失的输出。帮助查询要求实际成功退出。记录新 prompt、缓存和生成 token（含 schema／模板），以及步数、确认、延迟、结果类型、文件变化与重复结果一致性。协议稳定不等于任务正确；当前效果以实测报告为准，不宣称接口简化已提高准确率。

后台结果先在显示状态的锁内核对版本并发布，再记录 completed；发布前已经过期的结果记录 cancelled，不带可评分的 kind/text。成功发布后用户继续输入可以正常清除候选；completed 只表示当时已发布，不代表用户接受或执行。

`generate`／`run` 入口显式接收 `UserInput`：前台传终端输入通道，后台及无交互调用传 `NoUserInput`，不另设隐式降级的兼容入口。后台的结果交付回调仍负责过期版本校验。

实现入口：[command_assist.rs](../crates/nosh-core/src/command_assist.rs)、[command_help.rs](../crates/nosh-core/src/command_help.rs)、[assist_worker.rs](../crates/nosh-core/src/assist_worker.rs)、[assist_display.rs](../crates/nosh-shell/src/assist_display.rs)。评测入口见 [eval](../eval/README.md)。
