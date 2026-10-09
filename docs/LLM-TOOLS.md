# LLM tools 设计

原则：**Agent 的操作工具与用户输入能力分开；CommandAssist 是 nosh 发起的内部任务服务，查询后直接交付候选。** 操作工具的 schema 与执行准入共用 `BuiltinTool`／`ToolSet`，Agent 的 `ask_user` 由输入能力决定；CommandAssist 不开放终端对话。上下文选择见 [Project context](PROJECT-CONTEXT.md)。

## 工具与模式

| 工具 | 参数 | 契约 |
|---|---|---|
| `exec` | `command`，`timeout_sec?` | 从会话当前 cwd 执行 Bash 兼容代码，cwd／变量持续存在；返回 stdout／stderr 和退出状态。省略超时使用配置值（配置默认 60 秒），显式值限制为 1–600 秒 |
| `read_file` | `path`，`start_line?`，`end_line?` | 读取带行号的文本；行号从 1 开始，起止均包含，省略 end_line 最多读 400 行。大输出及长行会截断；不列目录 |
| `grep` | `pattern`，`path?`，`glob?` | 递归搜索**文件内容**，pattern 是默认区分大小写的正则，可用 `(?i)`；glob 筛文件，不搜索文件名。返回 `path:line:text`，最多 200 行，标明不完整扫描 |
| `command_help` | `name`，`query?` | 仅 CommandAssist 使用。name 可包含子命令（如 `git commit`），受限查询帮助并返回相关原文及状态；query 忽略大小写，关键词／短语按子串匹配、选项保留名称边界，返回原文不改大小写；不是正则或程序参数 |
| `ask_user` | `question`，`choices?` | 仅 Agent 有交互输入能力时提供；可选单选建议始终允许自由输入，返回原文回答并在原会话继续，不授予执行权限 |

普通 Agent 的操作工具是 `exec`、`read_file`、`grep`；管道附件仅保留 `read_file`、`grep`。两种 Agent 模式有可用控制终端时均额外提供 `ask_user`，提问不读取 stdin，也不改变只读边界。CommandAssist Generate/Fix/Next 固定注册 `command_help`、`read_file`、`grep`，即使来自交互终端也不增加工具。三种辅助均直接向 nosh 返回完整候选命令或精确 `[None]`，不注册 `ask_user`、`command_info`、`exec` 或 `finish`。帮助查询是真实且受限的执行，详情见 [CommandAssist](COMMAND-ASSIST.md)。

模型可见描述只说明用途和必要的返回形式；参数说明保留含义、必要默认值和简短例子。grep 正则与 command_help 字面搜索明确区分，不提前堆叠实现细节、异常情况或重复告诫。权限和限额仍由宿主强制，截断、无匹配等状态在实际结果中说明。Agent 的 `ask_user` 说明询问缺失信息或选择、回答可更新需求，以及等待回答后再调用其他工具；背景规则不把真实用户回答当作外部工具数据。工具定义直接进入实际模板，未另设模型专用名称或隐含参数别名。

共享工具区先给出简短调用格式，再列 `<tools>` 中的完整 JSON schema：

```text
Tool calls:
<function name="function-name"><param name="param-name">param-value</param></function>
Wrap values containing <, & or newlines in <![CDATA[...]]>.
```

这部分只描述语法，不包含“正常回答”、`[None]`、只读或执行策略，因此不会把 CommandAssist 的限制带入 Agent。工具名称、参数转换、CDATA／实体解析和调用数量约束不随文案顺序改变。角色与工具调用格式仍遵循 MiniCPM5；共享说明的文字和位置是明确的 nosh 定制，不再声称完整 prompt 与官方模板逐字相同。

CommandAssist 共用固定 System：建议而不执行、平台、查询协议、数据边界和输出约定。协议 User 是 nosh，三个意图的本次请求与上下文都放进 User 任务包；终答要求和拒绝理由也是 nosh 的 User 反馈，不重复命令或原始需求。终端用户不直接参与此子会话，Agent 的交互与权限独立保留。具体文案见 [CommandAssist](COMMAND-ASSIST.md#instructions-与-prompt)。

`exec` 描述使用 “current shell session”，具体 shell 在会话环境中标为 `nosh (bash-compatible)`，不暗示调用系统 Bash。`command_help` 的简短描述不使用 external 术语，但仍只支持通过宿主检查的外部程序，未扩大执行能力。

执行工具统一命名为 `exec`，不保留 `run_command` 别名；审批卡片、权限规则和评测观测使用同一名称。工具 `exec` 不等于 shell builtin `exec`：前者运行受审批约束的命令，后者替换 shell 会话，仍被禁止。主 instructions 明确通过 `exec` 执行，并禁止结束或替换会话，避免同时出现“使用 exec 工具”和“不要使用 exec”的歧义。

`command_help(name="git commit", query="--amend")` 查询提交命令的帮助，不是筛选 Git 顶层概览。name 中的子命令独立传入 argv，不接受额外选项或 shell 代码；普通程序使用 `--help`，Git 子命令使用已知短帮助 `-h`，保留真实的 129 usage 退出码。完整调用仍须通过权限分析与用户规则，未知插件不会仅因追加帮助旗标而获得授权。

`ask_user` 必须是回合中唯一的工具调用，避免回答前就执行依赖它的操作；明确属于 `ask_user` 的解析错误也计入此检查，畸形提问与其他调用混合时整轮不执行。问题／回答最多 4,096 bytes，可选 choices 最多 20 个互异的非空单行字符串，每项最多 512 bytes；没有默认选项。终端用上下方向键选择，直接输入任意答案（包括数字）不受选项限制；问题及交互只写 stderr。回答以 `Message::UserAnswer` 回填，正文是包含 `question`、`choices` 和原文 `answer` 的 JSON，仍按普通文本渲染为工具回复，不参与旧工具输出压缩。这样模型步骤失败后，即使自动重置会话并重放待交付回答，`yes`／`second` 等答案仍保留对应的问题与选项；不新增恢复状态机。取消／EOF 中止任务，通道故障明确失败，不伪造答案或授权。Agent 输入能力变化时重开会话，保持同一会话工具前缀稳定。

`list_dir` 已移除，不保留执行别名。目录／文件名查询用 `exec` 调用 `ls` 等命令；受限模式不因此开放执行。

失败诊断通过明确入口注入匹配的已采集输出，不为拿到已有报错而重跑命令；当前不提供额外的采集槽读取工具，见[输出采集设计](OUTPUT-CAPTURE.md)。

**Available 是发现时的 shell 命令摘要，不是模型工具表、完整实时清单或权限白名单。** 项目声明的包管理器也不证明当前可以调用。

完整 Agent 的主 system 按 `files`、`dev`、`containers`、`network`、`system`、`data` 分组显示已探测命令，省略空组；额外名称归 `other`。Rules 合并为六条通用约束：请求、会话状态、非交互／交接、破坏性操作、背景边界、证据与收尾，不复制场景禁令。

管道只读模式按实际 `ToolSet` 明确仅有 `read_file`／`grep`，不注入 shell 命令清单和执行／终端交接规则；读取保护仍由宿主执行。误把目录传给 `read_file` 时，提示使用各模式共有的 `grep` 搜索文件内容，不再建议当前模式可能没有的 `exec`。

## grep

使用 `grep-regex`、`grep-searcher` 和 `ignore`，不依赖系统 `rg`。目录扫描遵循忽略规则，跳过隐藏文件及递归遇到的链接；不跟随目录链接。显式指定普通文件不应用目录扫描的忽略／隐藏过滤，文件链接仍按真实路径审批；两种入口都跳过二进制文件。返回路径相对于搜索目录，单文件则相对于其父目录。

忽略规则发现与加载属于筛选元数据，不发起审批，直接复用 `ignore` 库的默认行为和缓存；这些内容不作为搜索结果发送给模型。进入搜索目录和读取实际文件内容仍需授权，目录授权只覆盖本次调用内的实际子树。若显式搜索 `.gitignore` 等文件的内容，它仍按普通内容读取检查权限。

最多 200 个匹配行，反馈受 6,000 字符预算限制；正向 glob 不覆盖忽略和隐藏规则。每次调用累计最多枚举 10,000 个目录条目、读取 64 MiB 文件内容，复用 `agent.command_timeout_sec` 超时，不新增配置。目录条目在收集时计数，内容经流式读取，每次底层读取最多 64 KiB；零匹配也受扫描预算约束。

目录枚举和内容读取之间检查超时与取消。达到扫描／输出上限时保留已得到的匹配，标注 `truncated=yes` 和原因；截断的零匹配只表示已扫描部分未匹配。Ctrl-C 则直接取消任务、跳过同批后续调用，不算作命令执行。检查是协作式的，不强制中断单次阻塞的文件系统操作或库内忽略元数据加载。

零匹配、截断、非法 regex/glob、路径与读取错误分别报告；“未匹配”只针对本次搜索范围，不证明整个项目不存在该内容。

## Harness 与证据

- **执行准入**：命令经过风险分析和必要审批；未知／禁用工具不执行。读取保护根或普通目录中的受保护文件均需授权，目录搜索授权仅限该次调用。
- **真实结果**：保留退出码、stdout、stderr、超时、中断和截断；不把元数据当文件内容、部分输出当完整结果、失败当成功。
- **输出预算**：命令 stdout/stderr 正文共享 6,000 字符，状态与省略标记另计；可另存已采集输出，不能恢复采集阶段已经丢弃的字节。
- 终端／密码交接由 harness 直接结束任务并交回原命令，说明可能已有部分执行；不重试、不读取密码。
- 用户拒绝审批时，模型同时收到“未执行”、审批请求的风险／触发原因，以及用户明确提供的理由（若有）。审批触发原因不冒充用户拒绝理由，不暗示换写法即可绕过拒绝。

权限、超时、执行计数与交接由代码保证，不依赖 prompt 放行。模型负责理解目标、选择工具、根据证据收尾；明确任务完成即停止，缺失必要目标才澄清。

## 建议模式

建议仅接受一个完整 shell program，可为单行或完整多行结构。由 brush 检查语法与可确认的命令名；递归覆盖替换和已知函数作用域。动态无法确认不等于成功或安全，建议从不自动执行。

CommandAssist 从调用方 `AgentConfig` 构造明确权限 context，复用用户规则，不构造默认策略替代调用方配置。

## 后续设计，不代表已实现

| 方向 | 约束 |
|---|---|
| 按需 help | 先使用现有命令工具查询；有重复查询／用法错误证据后，再评估独立封装，不能自动给每个命令查询帮助或盲拼 `-h` |

未来帮助缓存须绑定真实命令解析身份、文件版本、查询参数及相关环境，保持当前权限、超时与输出预算；PATH 名称缓存不等于帮助正文缓存。当前不新增 `help` 工具。

工具变化需验证接口、权限、错误、过滤／截断与固定 seed 全量回归。正确率、步数、确认次数和输入成本分别报告；裁判缺陷独立处理，不用工具或 prompt 特例绕过评分。

主提示、建议提示和 `exec` 说明均以当前目录、持久会话为准；保留合法 `cd`，执行器不改写命令。已有定向无模型验证，真实模型效果仍待测量，不把展示变短当作行为改善。

实现入口：[tools.rs](../crates/nosh-core/src/tools.rs)、[agent.rs](../crates/nosh-core/src/agent.rs)、[command_assist.rs](../crates/nosh-core/src/command_assist.rs)、[suggestion.rs](../crates/nosh-shell/src/suggestion.rs)。
