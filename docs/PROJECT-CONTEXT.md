# Project context 设计

原则：**用户请求决定任务，context 补充事实，harness 管执行边界。** 下文区分当前代码契约与待验证优化；实现不等于效果已获验证，结果见[实测](../eval/README.md)。

## 消息契约

system 与工具 schema 在对话内保持稳定。项目事实、文档和附件不提升为可信 system 内容；不能从背景推导额外任务。

当前仍是**一个 User 消息**：紧凑 `[context]` JSON → 可选最近命令／项目文档／附件／真实失败说明 → 请求。不改写请求内容，沿用首尾空白裁剪并置于最后；内部 `Trigger` 只负责路由，不暴露给模型。

## 最小事实

| 保留 | 约定 |
|---|---|
| cwd、项目类型／名称、脚本名、包管理器 | cwd 只出现一次；相同根省略，祖先根用相对路径；推断的 `manager_hint` 不证明已安装 |
| 执行约束 | Rust edition／继承、有效 workspace、Python 要求、Node 模块类型；不展开普通包版本或完整依赖表 |
| Git、语言、venv、真实失败 | head 为分支或短 SHA；dirty 仅指已跟踪变化；失败保留命令与退出码 |
| 缺失与告警 | 未识别 manifest、无 Git、未知、受保护、不可读分别表达；未识别不等于目录为空或输入不存在 |

不输出空告警、重复 manifest 结构或默认时钟；时间敏感任务获取实际时间证据。字符串保持 JSON 转义，不用模糊省略换 token。

```text
[context] {"cwd":"/work/app","project":{"type":"rust","name":"app","edition":"2021","workspace":true},"git":"none detected","lang":"zh"}
编译
```

## 发现与文档

从 cwd 向上查找，到 Git/HOME 边界或 32 层停止。最近 manifest 确定项目；解析 Rust、Node、Python 的少量字段，也识别 Go、Maven、Gradle、CMake、Make，同层类型可并存。项目根不改变权限工作区。

**先检查整个作用域的 AGENTS.md，再决定是否使用 README；不加载 NOSH.md。**

| 条件 | 行为 |
|---|---|
| 存在适用 AGENTS.md | 根到子目录加载，标明来源；更具体的文件只管其子树，不能覆盖用户请求或安全规则；不加载 README |
| AGENTS 过大、受保护或不可读 | 明示未加载来源，不截掉规则冒充完整，也不回退 README；agent 收到补读要求，建议模式在调用模型前报错 |
| 确认没有 AGENTS | 最近 README 的精简原文简介与带行号章节索引，仅作参考；跳过代码围栏／徽章，不另用 LLM 摘要，不执行示例 |

读取沿用保护及符号链接检查，不执行项目脚本。单文件上限 64 KiB；AGENTS 自动正文合计 4,000 字符，只纳入完整文件；README 摘录约 1,000 字符，可选参考不可用不阻断建议。字符上限不等于 token 数。

## 刷新与缓存

| 层 | 当前边界 |
|---|---|
| 项目事实 | 每任务重新发现、发送；命令实际改变 cwd 后补充新事实。同 cwd 不代表 manifest、Git、环境或保护设置没变 |
| 文档内容缓存 | 按路径与 `FileStamp` 复用；作用域和当前保护仍重新检查 |
| 文档投递去重 | 成功送达后才去重，不完整指引不记为完成；内容／作用域变化重新发送，消失显式清除，错误／取消／新会话重新确认 |
| LLM KV 缓存 | 复用相同 token 前缀；在历史末尾重复追加相同 context 仍有新开销，不能省掉新会话应有的上下文 |

当前指引只在任务开始时按 cwd 选取；命令内换目录不会即时重载。agent 的“补读指引”是提示约束，不是新增执行硬门禁。

## 待验证优化

- **先补刷新一致性**：已完成命令改变 cwd 后，下一模型步前更新文档作用域；不承诺在复合命令中途拦截或重写执行。
- **先去重，再做复杂缓存**：状态照算，完整快照未变则不重发，变化则整份替换；请求、附件和真实失败事实不去重。
- **分离意图与压缩格式分别比较**：背景和原始请求分消息；同一组事实分别用 JSON／短标签键值文本渲染。二者都不是当前实现，不预设更省 token 或更准确。

比较应覆盖完整输入成本（含文档、schema、消息模板）及任务正确性；仅转换历史 context 头的 token 报告不能代表全部收益。

实现入口：[project.rs](../crates/nosh-core/src/project.rs)、[guidance.rs](../crates/nosh-core/src/guidance.rs)、[prompt.rs](../crates/nosh-core/src/prompt.rs)、[agent.rs](../crates/nosh-core/src/agent.rs)。
