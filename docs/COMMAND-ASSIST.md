# CommandAssist：命令辅助

当前 LLM 能力分成两类：**CommandAssist 提出 shell 输入，Agent 分析或执行任务**。Generate、Fix、Next 是命令辅助的三个意图，不是三套 agent；共用查询循环、工具、结果校验和模型实例。

## 入口与职责

| 意图 | 入口 | 输出 |
|---|---|---|
| Generate / CommandGen | `nosh -s "描述"`、非空输入 Ctrl+G | 生成或改写一个完整 shell program，必要时澄清 |
| Fix / AutoFix | 用户命令失败后的自动辅助、裸 `ai fix`、失败后的裸 `#` | 修正建议或必要澄清，不自动重跑或修改环境 |
| Next / NextSuggest | 用户命令成功后的自动辅助、`ai next` | 有依据的下一条命令；没有合理下一步就不建议 |

`ai fix <question>` 保留 Agent 入口，携带匹配的失败证据，支持解释、日志分析和继续诊断。普通 `# <任务>`、自然语言任务与 `nosh -a` 仍走 Agent。

只有交互层直接执行的用户命令产生自动辅助事件。Agent 内部的工具成功／失败继续由原 Agent 处理；不递归启动 Next/Fix。脚本、`-c`、补全、本地拼写纠错、AI 管理命令不产生新的用户执行事件。

成功退出不构成新任务，非零退出也不必然是错误。失败仍沿用 `failure_is_notable` 排除取消、管道信号和 grep/test 等无结果状态。AutoFix 的“自动”是自动触发，不是执行授权；要实际修复，用户需自行执行候选命令或明确交给 Agent。

## Instructions 与 prompt

每次只发送当前意图的一段简短说明，公共部分保留外部文本边界和终态提交约定。不把三类规则同时发送，也不复制 Agent 的全部行为规则。工具描述说明参数和能力，权限、调用次数、取消和生命周期由代码控制。

```text
System：工具协议 + 公共背景边界 + 当前意图 + finish 约定
System：本次事实、适用项目文档、可选执行证据
User：真实用户请求（存在才发送）
System：宿主 command_completed 事件（Fix / Next）
Assistant / Tool：必要查询及结果
Assistant：一个 finish 调用
```

模型模板包含通用“可以直接回答”的描述，因此不只依赖文字要求稳定输出：CommandAssist 设置单步 `ToolChoice::Required`，解码前填入工具调用开头，在第一个调用结束后停止；最后一步及协议修正使用 `Named("finish")`。生成参数仍须经原有解析与宿主校验，不保证语义正确。Agent 保留 `Auto`，不改变其原生模板或生成方式。执行事件不伪造 User 请求，也不代表用户授权。

`SessionSpec.label` 仅供宿主观测区分 `agent` 与 `command_assist.<intent>.<foreground|background>`，不编码进模型输入。没有新增分类模型调用。

## Context

| 意图 | 初始内容 |
|---|---|
| Generate | 原始描述／待改写命令、cwd、平台、项目事实与适用指引 |
| Fix | 原命令、command ID、执行 cwd、退出码、匹配的有界终端证据、当前 cwd 与项目事实 |
| Next | 成功命令、command ID、执行 cwd、当前 cwd 与项目事实；不自动附成功输出 |

共享项目与文档采集器，沿用 AGENTS 完整加载、README 摘要和路径保护。指引不完整时明确报错，不猜测剩余规则。每次辅助都是独立短对话，不默认携带主 Agent 历史、最近三条命令或完整环境变量。

shell 的有界命令快照保存 PATH、有效 builtin／alias／function 名称、命令 hash 和解析选项，供后台查询与候选校验使用；不会在后台访问或修改活 shell。它不是文件系统快照，查询仍可能观察到随后发生的文件变化；过期输出不能交付。

Fix 只接受匹配 command ID、退出码与执行目录的证据，区分缺失、空、截断、不完整、混流。Next 不根据陈旧聊天猜测用户目标；当前自动入口没有额外的目标推断或长期记忆。

## Tools

| 工具 | 能力与边界 |
|---|---|
| `command_info(name, query, topic?)` | `resolve` 返回当前快照中的命令身份；`list` 返回匹配 name 前缀的候选命令名，不列目录；`help/version` 查询允许执行的外部程序，help 可按 topic 子串筛选 |
| `read_file` | 复用文本读取实现和 400 行上限，只读取当前策略允许的普通文件；FIFO、设备等特殊文件不打开 |
| `grep` | 复用内容搜索及扫描预算，每次辅助搜索最多 2 秒或更短的配置期限 |
| `finish(kind, text?)` | 唯一终态接口；不是执行工具 |

命令辅助不注册 `run_command`，不执行生成的目标操作，即使目标本身只读。查询仍受用户 deny 和保护路径限制；需要审批的查询返回明确错误，不在后台弹审批，不使用 YOLO 绕过命令辅助的能力上限。

`help/version` 固定使用 `--help`／`--version`，直接启动解析到的程序，不接受任意 shell 字符串、子命令或额外参数。拒绝 alias、function、本地／不透明程序，以及权限分析无法判为 Safe 的调用。子进程使用快照中的导出环境与非交互覆盖项，stdin 关闭，独立进程组，有取消、最长 3 秒和 64 KiB 采集上限；返回正文最多 1,800 字符，明确标记截断。需要具体参数说明时用 topic 筛选，不默认灌入整份帮助文本。

**帮助查询仍是真实执行，不是沙箱。** 不能仅凭 `--help` 字样把未知项目程序视为安全。文件系统查询也是协作式检查，不能保证强制中断一次阻塞 I/O；现阶段没有跨任务帮助缓存。

## 结果与预算

| kind | text | 宿主处理 |
|---|---|---|
| `command` | 必需，非空完整 program | 静态校验后显示／预填；不自动执行 |
| `clarify` | 必需，非空问题 | 显示必要问题，不写进命令 stdout |
| `none` | 必须省略 | 正常无建议，不伪造命令 |

每步只接受一个工具调用，不允许伴随自由文本；查询与 `finish` 走同一协议，不保留另一套批量分发。单一 finish 的字段／program 校验失败时，在剩余步数内最多反馈一次工具错误让模型修正，下一步只能提交修正后的 finish；仍无有效结果则失败。不从 Markdown 或说明文字中猜测命令，也不把解析失败降级成 `none`。命令语法和可确认名称检查复用 shell 的建议校验；动态行为无法确认不等于安全或执行成功。

默认关闭思考，每步最多 512 个新 token；Generate/Fix 最多 4 个模型步，Next 最多 2 步，并受更小的 `agent.max_steps` 限制。最后一步保留给 finish，宿主发送查询预算耗尽消息并选择 Named finish，避免所有步数都花在探测上。原生解码每步一个调用；任务期限在步骤边界检查，模型生成期间可取消。采样沿用配置，不为特定测例覆盖 seed 或提高预算。宿主预填的调用开头计入 prompt token，不伪装成模型生成。

`nosh -s` 成功时 stdout 仅含命令；澄清／无建议退出 1，错误退出 2，取消退出 130。交互候选只有接受后进入输入行，用户仍需回车。

查询错误以工具结果返回，允许在预算内采用其他查询；如果最后的查询仍失败，`finish(none)` 不能把它隐藏为正常无建议，宿主报告查询失败。

## 调度与用户输入

默认 `[shell] command_assist = true`。每条用户命令完成后，成功排入 Next，值得诊断的失败排入 Fix；`on_failure = "off"` 关闭自动失败辅助，`ai auto off` 暂停自动辅助。`command_assist = false` 保留显式 Generate/Fix/Next；`--safe`／`NOSH_DISABLE_AI` 关闭 AI。

一个后台 worker 临时持有既有 Agent 及其推理引擎，只有一个最新任务槽，不加载第二份模型。前台普通命令和编辑不等待推理；显式 AI 请求取消后台工作并取回同一引擎，因此可等待当前推理取消或模型加载完成。主 Agent 的对话日志不因辅助任务而清空，单份活动 KV 在切换对话后可能重新 prefill。

前后台共用一个带 `LoadMode` 的加载入口，后台明确传入 Background，不回退到交互式加载。后台只使用已安装模型，不下载、不读终端、不打印加载进度；仅首次创建 Agent 时在 worker 中探测环境，不在每次命令结束的前台路径重复查询 PATH。错误作为辅助状态显示。输入编辑、新执行和退出使旧版本失效；结果、版本和取消句柄由同一个锁保护，取消回调在锁外执行。

正常 reedline 终端在提示符上方显示候选，Ctrl+G 接受；接受时再次核对 command ID 和活 shell 的静态校验。基本终端只在输入仍为空时用新行显示结果并重画提示符，Ctrl+G 接受已校验的候选；用户开始输入后取消旧任务，不把后台正文插入已有输入。

## 评测

Agent 回归与命令辅助分别使用 `eval/suites/regression.json` 和 `eval/suites/command-assist.json`，从 `eval/scenarios/` 引用唯一场景定义。`smoke.json` 仅选择既有场景的 seed 0 小集合，不替代正式基线。Agent 回归显式关闭自动辅助以隔离任务统计；CommandAssist 专项分别覆盖生成、帮助查询、澄清、自动失败修复和成功后无建议，自动场景保持真实完成事件。

真实 trace 区分模型工具调用和宿主接受结果。只有与对应会话、command ID、执行状态和实际 finish 内容一致的 `observation` 才证明命令被接受；`finish` 不计为执行。CLI 还要核对实际 stdout，不能拿 trace 中的正确命令替代错误或缺失的输出。帮助查询要求实际成功退出。记录新 prompt、缓存和生成 token（含 schema／模板），以及步数、确认、延迟、结果类型、文件变化与重复结果一致性。协议稳定不等于任务正确；当前效果以实测报告为准，不宣称短 prompt 已提高准确率。

实现入口：[command_assist.rs](../crates/nosh-core/src/command_assist.rs)、[command_info.rs](../crates/nosh-core/src/command_info.rs)、[assist_worker.rs](../crates/nosh-core/src/assist_worker.rs)、[assist_display.rs](../crates/nosh-shell/src/assist_display.rs)。评测入口见 [eval](../eval/README.md)。
