# Project context 设计

原则：**用户请求决定任务，context 补充事实，harness 管执行边界。** 下文区分当前代码契约与待验证优化；实现不等于效果已获验证，结果见[实测](../eval/README.md)。

## 消息契约

初始 System 与工具 schema 在对话内保持稳定。动态背景使用独立 System 消息；只有角色边界采用特殊 token，正文按普通文本编码，不能注入角色或工具调用 token。消息角色不改变“参考资料不是任务”的规则或执行权限。

agent、`-s` 和 Ctrl+G 统一发送 **System 背景 + User 原始请求**。背景包含 context、最近命令、项目文档、附件和真实失败事实；请求不改写、不拼接背景。内部 `Trigger` 只负责路由；只有失败后直接求助但没有请求文本时，生成默认的失败解释请求。

会话支持追加 System，不修改或重复初始规则。追加的 System 参与消息计数、回退及缓存前缀复用；步数上限的 System 收尾要求只作用于前一条用户请求，不限制后续任务。真实工具结果继续使用原有工具协议。

## 最小事实

| 保留 | 约定 |
|---|---|
| cwd、项目类型／名称、脚本名、包管理器 | cwd 只出现一次；相同根省略，祖先根用相对路径；推断的 `manager_hint` 不证明已安装 |
| 执行约束 | Rust edition／继承、有效 workspace、Python 要求、Node 模块类型；不展开普通包版本或完整依赖表 |
| Git、语言、venv、真实失败 | head 为分支或短 SHA；dirty 仅指已跟踪变化；失败保留命令与退出码 |
| 缺失与告警 | 未识别 manifest、无 Git、未知、受保护、不可读分别表达；未识别不等于目录为空或输入不存在 |

模型可见 context 使用标签行，不再输出 JSON 对象；内部事实仍是结构化数据。多项目重复 `project:` 行；特殊字符值加引号并转义，不能伪造新字段。不输出空告警、重复 manifest 或默认时钟。

```text
<|im_start|>system
[context]
cwd: /work/app
project: rust; name=app; edition=2021; workspace=true
git: none detected
lang: zh
<|im_end|>
<|im_start|>user
编译<|im_end|>
```

## 发现与文档

从 cwd 向上查找，到 Git/HOME 边界或 32 层停止。最近 manifest 确定项目；解析 Rust、Node、Python 的少量字段，也识别 Go、Maven、Gradle、CMake、Make，同层类型可并存。项目根不改变权限工作区。

**先检查整个作用域的 AGENTS.md，再决定是否使用 README；不加载 NOSH.md。**

| 条件 | 行为 |
|---|---|
| 存在适用 AGENTS.md | 根到子目录加载，标明来源；更具体的文件只管其子树，不能覆盖用户请求或安全规则；不加载 README |
| AGENTS 过大、受保护或不可读 | 明示未加载来源，不截掉规则冒充完整，也不回退 README；agent 收到补读要求，建议模式在调用模型前报错 |
| 确认没有 AGENTS | 最近 README 的首段简介与带行号章节索引，仅作参考；不展开后续操作段落，不另用 LLM 摘要 |

文档附类型、来源及 `<untrusted_text>…</untrusted_text>` 外部输入边界，不因位于 System 消息中就成为系统指令。AGENTS 约定仍只在其目录作用域内、低于用户请求和安全规则生效；README 仅作参考。文件中与边界相同的字面标签转义展示，原文件与缓存不改写。

来源相对于同一背景消息的 cwd：同目录显示 `[README reference "README.md"]` 或 `[AGENTS.md "AGENTS.md"]`，父目录显示 `../README.md` 或 `../AGENTS.md`。仅在相对路径解析后仍指向原文件时缩短；跨符号链接或无法确认时保留绝对路径。实际读取、权限检查和文件缓存始终保留原始路径。

不反复解释文档政策，也不无条件要求补读 README。AGENTS 原文不擅自精简；真正未加载时仍保留补读要求，缺失／保护等 harness 诊断留在外部文本边界之外。旧文档作用域确实需要替换时才发送清除标记。

读取沿用保护及符号链接检查，不执行项目脚本。单文件上限 64 KiB；AGENTS 自动正文合计 4,000 字符，只纳入完整文件；README 正文最多约 1,000 字符，裁切显式标注，可选参考不可用不阻断建议。字符上限不等于 token 数。

## 刷新与缓存

| 层 | 当前边界 |
|---|---|
| 项目事实 | 每任务作为 System 重新发送；一轮工具执行后 cwd 改变，在工具结果之后追加新 System 事实与适用文档。同 cwd 不代表 manifest、Git、环境或保护设置没变 |
| 文档内容缓存 | 按路径与 `FileStamp` 复用；作用域和当前保护仍重新检查 |
| 文档投递去重 | 身份包含 cwd 与展示路径，避免复用失效的相对引用；替换时立即废弃旧标记，完整指引成功送达后才确认。不完整状态恢复后重发，错误／取消／新会话重新确认 |
| LLM KV 缓存 | 复用相同 token 前缀；在历史末尾重复追加相同 context 仍有新开销，不能省掉新会话应有的上下文 |

指引在任务开始及一轮工具执行后 cwd 改变时刷新，下一模型步收到正确作用域；不在复合命令中途或同一批工具调用之间拦截执行。agent 的“补读指引”是提示约束，不是新增执行硬门禁。

## 待验证优化

- **先去重，再做复杂缓存**：状态照算，完整快照未变则不重发，变化则整份替换；请求、附件和真实失败事实不去重。

标签格式、System 背景、独立请求和文档精简已进入代码，行为效果尚未测量。后续比较应覆盖完整输入成本（含文档、schema、消息模板）及任务正确性；历史 JSON 渲染转换的 token 报告不代表当前收益。

实现入口：[project.rs](../crates/nosh-core/src/project.rs)、[guidance.rs](../crates/nosh-core/src/guidance.rs)、[prompt.rs](../crates/nosh-core/src/prompt.rs)、[agent.rs](../crates/nosh-core/src/agent.rs)。
