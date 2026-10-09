# 代码架构与维护

本文是**当前代码组织、依赖方向和扩展入口**的维护来源。产品行为和配置见 [DESIGN](DESIGN.md)，功能契约见[文档导航](README.md)。只维护当前接口；这里的 crate 边界不是进程隔离或 Windows shell 支持。

## Crate 职责与依赖

CLI 是装配入口；业务核心依赖引擎契约，不依赖具体推理或下载实现。下表只列生产依赖，包含目标平台特有依赖和构建依赖，不包含 `dev-dependencies`。

| Crate | 职责 | 允许依赖的项目 crate |
|---|---|---|
| `nosh-engine` | `ChatEngine`、消息、事件、取消、通用错误、脚本化 Mock | 无 |
| `nosh-platform` | 语言、终端能力、应用路径、文件指纹、进程统计 | 无 |
| `nosh-permissions` | AST 风险分析、路径分类、审批策略、会话放行 | 无 |
| `nosh-hub` | 模型清单、选源、下载、校验、安装与离线导入 | `nosh-platform` |
| `nosh-llm` | Candle 模型、分词、模板、采样、对话 token 日志、KV、设备选择 | `nosh-engine` |
| `nosh-shell` | brush 会话、REPL、输入编辑、补全、终端与进程管理 | `nosh-platform` |
| `nosh-core` | Agent、CommandAssist、上下文、工具、审批 UI、REPL AI 适配 | `nosh-engine`、`nosh-permissions`、`nosh-platform`、`nosh-shell` |
| `nosh-cli` | 参数与配置、模型装配、子命令、评测观测包装器 | 上述七个 crate |

[`tests/architecture.rs`](../tests/architecture.rs) 持续约束这些方向。增加生产依赖或新 crate 时，先明确职责，再同步约束和本表，不通过转成可选依赖规避边界。

`nosh-core` 的模板互操作与下载进度测试仍以开发依赖使用 `nosh-llm`、`nosh-hub`；`nosh-llm` 的真实模型测试也使用 hub 寻找已安装模型。这些依赖不会传递给下游：`nosh-tests` 的 Agent 流程只使用 `nosh-engine::MockChatEngine`，不需要链接 Candle 或下载组件。

## 接口边界

| 边界 | 契约与入口 |
|---|---|
| 引擎 | [`nosh-engine`](../crates/nosh-engine/src/lib.rs) 定义 open / step / rewind / compact / close 和结构化事件；不含 tokenizer、special token 或 Candle 类型 |
| 本地推理适配 | [`LocalChatEngine`](../crates/nosh-llm/src/local.rs) 实现引擎契约；`ModelSource` 只接收模型 ID、预期架构、权重与 tokenizer 路径、结束 token 和采样默认值 |
| 模型装配 | CLI 的 [`engine.rs`](../crates/nosh-cli/src/engine.rs) 查找或下载模型，并集中把 `ResolvedModel` 转成 `ModelSource`；普通加载和 debug 共用该转换，doctor 的设备检查只接收权重路径 |
| Shell | [`EmbeddedShell`](../crates/nosh-shell/src/backend.rs) 持有共享 brush 会话；用户执行直连终端，Agent 执行采集输出；snapshot / resolve / parse 提供状态与解析 |
| 风险与审批 | [`nosh-permissions`](../crates/nosh-permissions/src/lib.rs) 分析风险并计算 Allow / Ask / Deny；[`ApprovalChannel`](../crates/nosh-core/src/approval.rs) 负责审批交互，不由模型决定放行 |
| 对话日志 | [`conversation.rs`](../crates/nosh-llm/src/conversation.rs) 管理编码、连续工具结果分组、回退与原子压缩；模型和 KV 留在 `local.rs` |
| 输出出口 | [`Redactor`](../crates/nosh-core/src/tools.rs) 在完整采集输出落盘前处理文本；终端显示仍由 Shell / core 的显示层处理，不放进平台层 |

引擎通用错误 `EngineError` 保留上下文已满、未知会话、配置和 I/O 分类；模型与分词失败作为带原始错误的 `Backend` 传递。`nosh-llm::LlmError` 只描述本地实现，不再成为 Agent、Mock、trace 或评测代理的接口要求。消息序列化、取消、会话生命周期与失败恢复语义保持一致。

`nosh-platform` 只承载已有的共享宿主能力。模型校验规则留在 hub，提示符布局和 ANSI 内容清理留在 shell，审批与工具权限留在 core / permissions；不要把业务逻辑堆入公共 crate。

Harness 直接使用 `EmbeddedShell`，REPL AI 适配在 core；不为尚未存在的后端保留转发 trait 或兼容接口。推理依赖解耦不意味着 Shell 后端、Windows 或 IPC 已实现。

## 代码与测试组织

模块根文件放实现；较大的同模块测试放在同名目录中，保留原测试模块名称、私有成员访问和 Cargo 测试入口。

| 位置 | 维护内容 |
|---|---|
| `nosh-core/src/agent.rs`、`agent/` | Agent 生命周期、恢复、工具执行；`tests.rs` 与 `permission_tests.rs` 分别维护恢复和审批回归 |
| `nosh-core/src/tools.rs`、`tools/tests.rs` | 内置工具目录、文件读取、grep、输出预算及其测试 |
| `nosh-core/src/command_assist.rs`、`command_assist/tests.rs` | Generate / Fix / Next 的固定查询集合和直接终态 |
| `nosh-core/src/command_help/`、`user_input/` | 命令帮助与提问能力的实现辅助及测试 |
| `nosh-shell/src/completion/`、`input_assist/` | 编辑器适配、后台服务、分析和有界查询 |
| `tests/agent_flow.rs`、`tests/flows/` | 跨 crate 的 Agent、CommandAssist、context、permissions 流程；共用 `support.rs` 的夹具与进程内锁 |
| `nosh-core/tests/terminal.rs`、`nosh-core/tests/terminal/` | 同一测试目标和三个精确命名的子进程 probe；按显示、审批、输入、命令辅助分文件，共用 PTY 驱动 |
| `tests/fixtures/model_context.txt` | 推理与 ARM 内存测试共用的固定语料；不是设计文档，不随文档改写 |

新增工具沿 `tools.rs` 的目录和分发接线；Agent 的提问能力在 `user_input.rs`，CommandAssist 复用读取能力但不开放终端交互。工具声明顺序影响 prompt，不随文件整理改变。

修改 `ChatEngine` 时同步 Local、Mock、CLI trace、评测代理和全部调用方。模板与 token 细节不移入 harness，压缩内容的共用规则在 `nosh-engine::shorten_tool_result`；模型相关分组和重新编码仍归 `Conversation`。

`nosh-tests` 只有实际的集成测试目标，不提供空 library。`eval/scenarios/` 是当前场景单一来源，`eval/suites/` 声明运行计划；评测分层、报告和来源哈希按 [eval/README](../eval/README.md) 维护。结构调整不改变固定语料、seed、预算或数值门槛，不为旧报告维护独立裁判和归档测试。

Linux CI 为评测器设置 `NOSH_TEST_BINARY`，把当前真实 CLI/PTY 与脚本化 worker 的无模型用例纳入正常回归，避免可选测试长期跳过后与接口或终端行为脱节。

## 开发入口

首先按[源码维护说明](REEDLINE-MAINTENANCE.md)运行 `cargo source prepare`。普通应用构建不要求 Python、Node 或 npm；完整评测工具链独立于运行时。

Linux / macOS / WSL 下，按变更选择最小覆盖范围：

```bash
# 轻量公共契约；也支持原生 Windows
cargo test -p nosh-engine -p nosh-platform --locked
# 不链接模型运行时的跨 crate 流程与架构约束
cargo test -p nosh-tests --locked
# 核心、推理适配与 CLI 的无模型回归；真实模型用例保持 ignored
cargo test -p nosh-core -p nosh-llm -p nosh-cli --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

完整平台矩阵以 [CI](../.github/workflows/ci.yml) 为准。模型吞吐、首 token 延迟和内存必须按 [DESIGN §13.2](DESIGN.md#132-测试矩阵) 固定构建、硬件、模型和冷热条件单独测量；代码整理不构成性能改善证据。

## 文档维护

README 保留产品与上手入口，[文档导航](README.md) 管理分类，本文件维护代码边界，DESIGN 维护当前系统行为和配置，功能文档维护详细契约。文档引用对应来源，不复制完整规则或接口清单。

历史计划、报告、版本迁移和决策过程交给 Git 历史，不放在当前维护面。变更直接更新现行接口和测试，同步仓库内引用；不保留旧路径转发、废弃配置或未使用的抽象。仍有效的终端降级、错误拒绝和安全边界属于当前功能，不能以清理历史为由删除。
