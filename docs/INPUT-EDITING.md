# 交互输入编辑与键位配置

范围是 nosh 提示符中**尚未提交的草稿**：命令、自然语言、多行输入及 AI 回填。不接管前台程序、审批控件或文件编辑；`-c`、脚本和管道执行不受影响。不承诺完整 Readline/Vim，也不提供任意脚本快捷键、宏、外部编辑器、完整 kill-ring 或系统剪贴板。

## 模式

`[shell] edit_mode = "auto" | "emacs" | "vi"`，默认 `auto`。启动环境中 `VISUAL` 或 `EDITOR` 任一值含大小写敏感的 `vi` 子串时选 Vi，否则 Emacs；显式配置优先。这里采用 Zsh 风格的产品策略，不表示文件编辑器名称普遍等于 shell 偏好。

主模式仅启动时选择，不接通运行时 `set -o vi/emacs`。Vi 从插入态开始，沿用原生 Esc、i/a 等切换及编辑流程。`[I]`、`[N]`、`[V]` 标记分别表示插入、普通、原生选区态；保留用户 PS1/PS2，不与 AI 审批模式混用。关闭状态行仍有模式标记。

## 默认覆盖与保留行为

| 键／场景 | 行为 |
|---|---|
| Tab，普通编辑 | 优先补全或适用的菜单操作；确认最终无结果、没有其它适用操作时请求 AI。查询中、临时候选或错误不冒充无结果 |
| F2，普通非空白草稿 | 直接请求 CommandAssist Generate，校验后只替换整份草稿，不执行 |
| F2，空白草稿 | 只采用当前有效、command ID 匹配的已有后台候选；没有候选就提示，不新增模型请求 |
| F2，搜索／菜单／Vi 选区／待完成操作 | 提示先退出或取消，不调用模型 |
| Ctrl+Z、Ctrl+_ | 撤销输入编辑，不是再次执行命令 |
| Ctrl+Y、Alt+/ | 重做输入编辑；不重新调用 AI |
| Vi `p/P` | 保留内部剪切缓冲的粘回；Emacs 粘回无默认键，可配置现有动作 |
| Enter，补全菜单 | 只接受候选，不提交草稿；空菜单／临时候选关闭时也不提交 |
| Enter，历史搜索有匹配 | 经同一验证与提交路由提交有效结果；不完整输入继续编辑 |
| Esc，历史搜索有匹配 | 只回填并返回编辑，不执行 |
| Ctrl+G，历史搜索 | 取消搜索，恢复原文、字节光标、选区及发起模式 |
| 无匹配的历史搜索 | Enter 不提交、保持搜索；Esc 或取消恢复原草稿 |
| Ctrl+C | 取消整份输入的固定保底路径 |

**其它基础键保留 Reedline 0.52 原行为，而不是混合的 Bash/Fish 兼容承诺。** 普通 Emacs 的 Ctrl+G 仍是重做；Vi 普通态 Ctrl+R 仍是历史搜索，不改为 Vim 重做。原生 `?` 进入搜索时会转到插入模式；取消恢复发起模式，接受结果保留入口的模式行为。

Emacs Ctrl+U 剪切到**整个输入缓冲区**开头，可能跨前面的行；Ctrl+W 使用原生 Unicode 词边界，不是空白分词或 Fish 的路径分段。Alt+Backspace 是词级删除，不新增 kill-ring 语义。未配置的 Ctrl+O 外部编辑器动作明确提示不可用，不列为支持能力。

AI 成功替换是一笔可撤销编辑：保留触发模式，移到新文本末尾（Vi 普通态位于末尾字素），清除旧选区。失败、取消、无建议或校验失败保留原文、光标与编辑状态。撤销本身仍遵循原生选区规则。已经收进同一输入批次、尚未显示建议前的 Enter 不提交新草稿；缓冲的文字仍保留，检查后再明确提交。

空白输入的 context 复杂推荐／Agent 继续仅保留后续设计。本期不实现该动作，也不将其混入建议入口。显式 `ai fix`、`ai next` 与已有自动 CommandAssist 调度保持原边界。

## 配置

动作映射到按键列表；列表替换该作用域的绑定，空列表解绑可配置入口。默认绑定仍有原生别名；显式列表可移除它们。没有旧 `shell.suggest_key` 兼容层。

```toml
[shell]
edit_mode = "auto"

[shell.keybindings]
ai_suggest = ["F2", "F3"]
undo = ["Ctrl+Z", "Ctrl+_"]
redo = ["Ctrl+Y", "Alt+/"]

[shell.keybindings.modes.vi_normal]
undo = ["u", "F4"]

[shell.keybindings.contexts.history_search]
accept = ["Enter"]
accept_search = ["Esc"]
cancel = ["Ctrl+G"]
```

作用域从低到高为：预设 → 公共动作 → `modes` → 公共 `contexts` → 模式下的 `contexts`。模式名为 `emacs`、`vi_insert`、`vi_normal`、`vi_visual`；上下文为 `editing`、`history_search`、`menu`。更具体的列表只替换同名动作，不按 TOML 排列顺序选“最后一条”。

例如 `contexts.menu.accept = []` 会停用菜单中的 Enter 接受；改成 `["F3"]` 则只有新列表接受候选，不保留旧 Enter，也不改变普通编辑的提交键。Vi 视觉态的 `cancel` 改键同样退出视觉态、恢复普通态，而不是仅清除一次选区；原生 Esc 保底仍然可用。

| 动作 | 能力 |
|---|---|
| `complete_or_ai` | 默认 Tab 的补全优先／AI 兜底 |
| `complete` | 仅补全，不请求 AI |
| `ai_suggest` | F2 的直接建议／已有候选采用 |
| `history_search` | 开始或继续反向历史搜索 |
| `accept` | 编辑／搜索中验证提交；菜单中仅采用 |
| `accept_search` | 仅搜索中回填，不执行 |
| `cancel` | 当前辅助交互取消；搜索中恢复原状态 |
| `insert_newline` | 插入换行 |
| `undo`、`redo` | 原生编辑历史 |
| `cut_to_start`、`cut_word_left`、`kill_line`、`delete_word_left` | 对应真实原生编辑原语，不另改词／多行边界 |
| `paste_before`、`paste_after` | 已有内部剪切缓冲，不是系统剪贴板 |

例如恢复传统 Tab：将 `complete_or_ai = []`，再设 `complete = ["Tab"]`。`ai_suggest = []` 只解绑直接入口，不关闭 `complete_or_ai` 的兜底能力；全局 AI 开关仍由 `--safe`／`NOSH_DISABLE_AI` 控制。自定义按键不提供任意 Vi 宏或计数语法；公布的原生 `u/p/P/?` 解绑同时处理其计数形式。

自定义可以显式占用默认键，并提示被替换的动作；有效上下文中自定义动作互相冲突、未知名称、重复／等价键、无效键格式和不可用上下文报错。无效编辑配置整组回退默认，继续启动，不丢掉其它配置。Ctrl+C 及 Vi 的 Esc 保底不可改成 AI／提交。搜索的 Enter/Esc/取消可在自己的上下文配置。

## 终端与资源边界

键名接受 Ctrl/Control、Alt、Shift 与一个字符或 Tab、Enter、Esc、Backspace、方向键、Home/End、PageUp/PageDown、F1–F24；加号字符写 `Plus`。不支持组合键序列。

传统编码中 Tab/Ctrl+I、Enter/Ctrl+M、Esc/Ctrl+[ 等可等价；`0x1f` 经 Crossterm 解析为 Ctrl+7，按 Ctrl+_ 的传统入口处理，不影响普通数字 7。冲突判断与实际事件使用同一归一化。AltGr 的 Ctrl+Alt 文本组合不作为可配置字符快捷键，以免破坏输入。

大小写快捷键查表与模式解析使用一致身份，`X`／`Alt+G` 等绑定不能绕过菜单或搜索的 AI 焦点限制；未绑定文字和 AltGr 保留原字符。增强协议的 `Ctrl+_` 既接受字符形式，也接受实际移位减号事件 `CSI 45;6u`；`Ctrl+_` 与 `Ctrl+Shift+-` 作为等价绑定进行冲突诊断，不仅依赖传统 `0x1f`。

配置确实需要可区分的增强事件时，在启动时查询并使用已有键盘协议；不支持时明确诊断并回退。Ctrl+Shift+Z、Shift+Enter、Ctrl+Tab 不能宣称跨终端通用；终端应用、输入法或系统仍可能截获按键，shell 无法检测全部宿主绑定。

基本终端明确使用简化输入：保留安全输入、退格、清空、换行及可用的 AI 入口；模式、历史搜索、撤销/重做及其它高级操作不可用会提示。审批和独立文本编辑仍用自己的输入规则。

基本终端的 `accept`、`cancel`、`cut_to_start` 也遵循列表替换与解绑；旧 Enter/Esc/Ctrl+U 不会从未命中的键表兜底重新启用，Ctrl+C 和空输入 Ctrl+D 的保底仍保留。搜索提示独立显示仍绑定的回填/取消动作；拒绝过期或无效后台候选时清除其活动显示，不反复推荐同一个失效候选。

Vi 待完成序列最多 64 个字符，一次有效操作最多 1,024 次重复；乘积和缓存 `.` 的展开同样在分配事件前检查。超限可见拒绝，不截断后执行；过长序列的尾部操作不会悄悄作用于草稿，Esc/Ctrl+C 始终可取消。每次读取批次有界，普通键不加载模型、不重新读取配置、不扫描目录、不新增线程或重复运行提示符钩子。主动补全和 AI 查询仍保留各自真实成本。

补全生成及慢任务由独立的 `completion` 模块负责。菜单打开后闲置刷新可以调用已加载脚本，但导航／重绘不重复调用；当前部分结果可明确选择，不能用于最终唯一或 AI 无结果判断。只有仍有效的显式请求可回填最终唯一候选，后台刷新不会自动替换草稿。来源、资源、隔离和实际 Git／Make 覆盖见[补全说明](COMPLETION.md)。

实现：`editing.rs` 的纯编译与模式包装器、`repl.rs` 的宿主动作/提示符、既有 InputAssist/CommandAssist；必要的原生输入上下文、搜索状态和草稿事务扩展由同仓 Reedline 补丁维护。它不修复既有 tmux 草稿历史残留，也不增加原生 Windows shell 支持。
