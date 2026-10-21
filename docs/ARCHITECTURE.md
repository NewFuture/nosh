# 代码架构与维护

本文是**当前代码组织、依赖方向和扩展入口**的维护来源。产品行为和配置见 [DESIGN](DESIGN.md)，功能契约见[文档导航](README.md)。只维护当前接口；crate 边界不是进程隔离，平台范围见 [DESIGN §4.6](DESIGN.md#46-平台)。

## Crate 职责与依赖

CLI 是装配入口；业务核心依赖引擎契约，不依赖具体推理或下载实现。下表只列生产依赖，包含目标平台特有依赖和构建依赖，不包含 `dev-dependencies`。

| Crate | 职责 | 允许依赖的项目 crate |
|---|---|---|
| `nosh-engine` | `ChatEngine`、消息、事件、取消、通用错误、脚本化 Mock | 无 |
| `nosh-platform` | 语言、终端能力、应用路径、文件身份、进程与可用内存统计及 Windows 进程组原语 | 无 |
| `nosh-permissions` | AST 风险分析、路径分类、审批策略、会话放行 | 无 |
| `nosh-hub` | 模型清单、选源、下载、校验、安装与离线导入 | `nosh-platform` |
| `nosh-llm` | Candle 模型、分词、模板、采样、对话 token 日志、KV、设备选择 | `nosh-engine` |
| `nosh-shell` | brush 会话、REPL、输入编辑/历史、补全、终端与进程管理 | `nosh-platform` |
| `nosh-core` | Agent、CommandAssist、上下文、工具、审批 UI、REPL AI 适配 | `nosh-engine`、`nosh-permissions`、`nosh-platform`、`nosh-shell` |
| `nosh-cli` | 参数与配置、模型装配、子命令、评测观测包装器 | 上述七个 crate |

[`tests/architecture.rs`](../tests/architecture.rs) 持续约束这些方向。增加生产依赖或新 crate 时，先明确职责，再同步约束和本表，不通过转成可选依赖规避边界。

`nosh-core` 的模板互操作与下载进度测试仍以开发依赖使用 `nosh-llm`、`nosh-hub`；`nosh-llm` 的真实模型测试也使用 hub 寻找已安装模型。这些依赖不会传递给下游：`nosh-tests` 的 Agent 流程只使用 `nosh-engine::MockChatEngine`，不需要链接 Candle 或下载组件。

## 接口边界

| 边界 | 契约与入口 |
|---|---|
| 引擎 | [`nosh-engine`](../crates/nosh-engine/src/lib.rs) 定义 open / step / rewind / compact / close 和结构化事件；不含 tokenizer、special token 或 Candle 类型 |
| 本地推理适配 | [`LocalChatEngine`](../crates/nosh-llm/src/local.rs) 实现引擎契约；`ModelSource` 接收模型 ID、预期架构、聊天格式、权重与 tokenizer 路径、结束 token 和采样默认值 |
| 模型装配 | CLI 的 [`engine.rs`](../crates/nosh-cli/src/engine.rs) 查找或下载模型，并集中把 `ResolvedModel` 转成 `ModelSource`；普通加载和 debug 共用该转换，doctor 的设备检查只接收权重路径 |
| Shell | [`EmbeddedShell`](../crates/nosh-shell/src/backend.rs) 持有共享 brush 会话；用户执行直连终端，Agent 执行采集输出；snapshot / resolve / parse 提供状态与解析 |
| 风险与审批 | [`nosh-permissions`](../crates/nosh-permissions/src/lib.rs) 分析风险并计算 Allow / Ask / Deny；[`ApprovalChannel`](../crates/nosh-core/src/approval.rs) 负责审批交互，不由模型决定放行 |
| 对话日志 | [`conversation.rs`](../crates/nosh-llm/src/conversation.rs) 管理编码、连续工具结果分组、回退与原子压缩；模型和 KV 留在 `local.rs` |
| 输出出口 | [`Redactor`](../crates/nosh-core/src/tools.rs) 在完整采集输出落盘前处理文本；终端显示仍由 Shell / core 的显示层处理，不放进平台层 |

引擎通用错误 `EngineError` 保留上下文已满、未知会话、配置和 I/O 分类；模型与分词失败作为带原始错误的 `Backend` 传递。`nosh-llm::LlmError` 只描述本地实现，不再成为 Agent、Mock、trace 或评测代理的接口要求。消息序列化、取消、会话生命周期与失败恢复语义保持一致。

REPL 装配使用 core 的 `EngineLoadError` 区分加载未就绪、取消与失败，`ModelAvailability` 保存前后台共享的可用性快照；模型查找与具体错误映射仍归 CLI。shell 的 `SuggestionOutcome` 只表达回填结果与诊断，不依赖引擎或下载实现。加载恢复及自动请求暂停契约见 [CommandAssist](COMMAND-ASSIST.md#调度与用户输入)。

Agent 与 CommandAssist 使用 `Usage::add_step` 汇总成功返回的步骤：token 与 prefill/decode 耗时累加，TTFT 保留首步实际值（包括零），context 保留末步快照。不把零当作尚未记录，也不累加 context 或把后续首 token 延迟替换首步测量。

`nosh-platform` 只承载已有的共享宿主能力。模型校验规则留在 hub，提示符布局和 ANSI 内容清理留在 shell，审批与工具权限留在 core / permissions；不要把业务逻辑堆入公共 crate。

Harness 直接使用 `EmbeddedShell`，REPL AI 适配在 core；不为尚未存在的后端保留转发 trait 或兼容接口。Windows 与 Unix 复用同一 brush 会话和状态契约，`nosh-shell/src/host.rs` 隔离输入句柄、命令查找、中断和执行生命周期。Windows Job Object、文件身份和安全替换属于平台原语；审批路径策略仍归 permissions，不移入平台层。

`nosh-llm` 的内部模型枚举分派 Llama 与 Qwen35；协议分派集中在 `protocol.rs`，模板和调用解析按模型隔离。共享的仍是 `ChatEngine`，不是 tokenizer 或模型状态类型。Qwen 的局部回退使用完整重算，不能借 Llama 的 KV 截短接口假装保留循环状态；usage 记录实际保留长度。宿主可用/总物理内存属于 platform，默认型号与失败暂停策略仍归 CLI。

Tokenizer 的逐 ID 解码字节使用连续存储和偏移表，保留空洞 ID 与 added token 的原始内容；完整词表副本只在加载校验时临时取得，不额外常驻。存储布局不改变控制 token ID、可信分段编码或增量 UTF-8 解码规则。

Agent 的 brush 执行参数可注入宿主进程启动器。Windows 在子进程恢复运行前加入本次执行独占的 Job Object，作用域结束后拒绝迟到的启动，并回收仍属于该次执行的后代，不按进程名或全局进程树回收。Unix 保持既有进程组及标记跟踪。

## 代码与测试组织

模块根文件放实现；较大的同模块测试放在同名目录中，保留原测试模块名称、私有成员访问和 Cargo 测试入口。

| 位置 | 维护内容 |
|---|---|
| `nosh-core/src/agent.rs`、`agent/` | Agent 生命周期、恢复、工具执行；`tests.rs` 与 `permission_tests.rs` 分别维护恢复和审批回归 |
| `nosh-core/src/tools.rs`、`tools/tests.rs` | 内置工具目录、文件读取、grep、输出预算及其测试 |
| `nosh-core/src/command_assist.rs`、`command_assist/tests.rs` | Generate / Fix / Next 的固定查询集合和直接终态 |
| `nosh-core/src/command_help/`、`user_input/` | 命令帮助与提问能力的实现辅助及测试 |
| `nosh-shell/src/completion/`、`input_assist/` | 编辑器适配、后台服务、分析和有界查询 |
| `nosh-shell/src/history.rs`、`history/` | 历史适配器负责查询与行提示，Store 统一 SQLite、内存提问、brush 视图和结果聚合；Reedline 只负责筛选、选择、预览和回填，真实 CLI 回归在 `nosh-cli/tests/history.rs` |
| `nosh-shell/src/abbreviations.rs`、`abbreviations/` | 内置/用户规则、能力检查、版本化目录及交互展开策略；不改写执行层文本 |
| `nosh-cli/src/config/document.rs` | 配置加载、主题与缩写共用的有界读取，以及 TOML 原子更新、配置锁及并发变更检查 |
| `tests/agent_flow.rs`、`tests/flows/` | 跨 crate 的 Agent、CommandAssist、context、permissions 流程；共用 `support.rs` 的夹具与进程内锁 |
| `nosh-core/tests/terminal.rs`、`nosh-core/tests/terminal/` | 同一测试目标和三个精确命名的子进程 probe；按显示、审批、输入、命令辅助分文件，共用 PTY 驱动 |
| `tests/fixtures/model_context.txt` | 推理与 ARM 内存测试共用的固定语料；不是设计文档，不随文档改写 |

CLI 的编辑、历史、主题、缩写、项目环境和 Bash 集成测试共用 `nosh-cli/tests/support/terminal.rs` 的控制终端、非阻塞读取、光标握手和有界排空回收，不各自维护 PTY 实现。

新增工具沿 `tools.rs` 的目录和分发接线；Agent 的提问能力在 `user_input.rs`，CommandAssist 复用读取能力但不开放终端交互。工具声明顺序影响 prompt，不随文件整理改变。

修改 `ChatEngine` 时同步 Local、Mock、CLI trace、评测代理和全部调用方。模板与 token 细节不移入 harness，压缩内容的共用规则在 `nosh-engine::shorten_tool_result`；模型相关分组和重新编码仍归 `Conversation`。

`nosh-tests` 只有实际的集成测试目标，不提供空 library。`eval/scenarios/` 是当前场景单一来源，`eval/suites/` 声明运行计划；评测分层、报告和来源哈希按 [eval/README](../eval/README.md) 维护。结构调整不改变固定语料、seed、预算或数值门槛，不为旧报告维护独立裁判和归档测试。

Linux CI 为评测器设置 `NOSH_TEST_BINARY`，把当前真实 CLI/PTY 与脚本化 worker 的无模型用例纳入正常回归，避免可选测试长期跳过后与接口或终端行为脱节。

## 开发入口

使用 `rust-toolchain.toml` 固定的工具链，在 Linux / macOS / WSL 下准备依赖并构建：

```bash
cargo source prepare
cargo build --release --locked
./target/release/nosh
```

原生 Windows x64 使用 MSVC Rust 工具链、C++ Build Tools 和 Windows SDK，在 PowerShell 运行：

```powershell
cargo source prepare
cargo build --release --locked -p nosh-cli
.\target\release\nosh.exe --safe -c 'printf "native Windows\n"'
cargo test -p nosh-cli -p nosh-permissions --test windows --locked
cargo test -p nosh-permissions --lib windows_tests --locked
cargo clippy --workspace --lib --bins --locked -- -D warnings
```

本地 `cargo build --release` 与发布流水线共用根 `Cargo.toml` 的构建配置：默认保持 `opt-level = 3`；brush 解释器、解析器、内建命令和 SQLite 使用包级 `"s"`，其余交互/UI、JSON/TOML 序列化、printf 格式化依赖及非推理业务 crate 使用 `"z"`。不全局降低依赖的优化级别：CLI 最终链接、`nosh-llm`、Candle、GEMM、half、tokenizers、正则匹配和 TLS 加密实现保留级别 3。继续使用 fat LTO、单 codegen unit 和符号剥离，不删减内建命令、模型或 Unicode 支持，也不使用依赖本机 CPU 的构建参数。

`panic = "unwind"` 必须保留：AI 异常隔离与登录 shell 的故障恢复依赖 `catch_unwind`，不能为缩小文件而切换为 abort。`fastrel` 继承包级优化，但仍使用 thin LTO、多 codegen unit 与增量编译供本地迭代。

Windows 工作树用 Windows Git/Rust 准备源码；需要 Unix 回归时可在 WSL 构建，但使用独立的 `CARGO_TARGET_DIR`，不用 Linux Git 操作 Windows `.git` 文件。离线准备、联调和补丁导出见[源码维护说明](REEDLINE-MAINTENANCE.md)。普通应用构建不要求 Python、Node 或 npm；完整评测工具链独立于运行时。

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

## 版本与发布

在 **Actions → Release → Run workflow** 选择分支，并填写必填的完整 `version`，例如 `0.2.0` 或 `0.2.0-rc.1`，不带 `v`。同一个[工作流](../.github/workflows/release.yml) 自动更新根 `Cargo.toml` 的 `[workspace.package].version`，用 `cargo update --workspace` 同步 `Cargo.lock`，提交并推送这两个文件；各 crate 继承版本，`nosh --version` 显示同一版本。已是指定版本时复用当前提交，不创建空提交。

后续构建和发布都固定到该版本提交，不依赖机器人 push 再触发另一个工作流。版本 tag 冲突会在推送提交前拒绝。需要仓库允许 Actions 使用 `contents: write` 推送版本提交；分支保护拒绝推送时会明确失败，不绕过保护，也不会继续发布。

| 平台 | Rust target | 产物 |
|---|---|---|
| Linux x64 | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `.tar.gz` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x64 | `x86_64-pc-windows-msvc` | `.zip` |

五个平台并行构建 CPU 版，Linux 使用 Ubuntu 22.04 构建环境。归档命名为 `nosh-<版本>-<target>`，包含可执行文件和许可证，不打包模型或 CUDA。Windows ZIP 使用最高级别的标准 DEFLATE 压缩，不压壳或改变解压方式。打包前执行产物的 `--version`，汇总时要求全部平台齐全并验证 SHA-256，随后生成统一的 `SHA256SUMS`。

只有全部构建成功才自动创建指向版本提交的 `v<版本>` tag 和 Release，说明自动生成，`-rc.1` 等版本自动标为 prerelease。`gh release create` 先创建草稿、上传所有归档和校验文件，最后发布，兼容需在仓库设置中启用的 Immutable releases；不移动已有 tag，不覆盖已发布 Release。

构建失败后优先使用 **Re-run failed jobs**，或以同一版本在当前分支重新运行，避免重跑旧的版本提交步骤造成非快进推送。若上传失败留下草稿，先检查并删除未发布草稿，再重试发布；已发布的不可变版本必须使用新版本号。

## 文档维护

项目 README 介绍功能与使用方式，AGENTS.md 提供简短日常开发指引，[文档导航](README.md) 管理分类。本文件维护代码边界和构建开发入口，DESIGN 维护当前系统行为和配置，功能文档维护详细契约。文档引用对应来源，不复制完整规则或接口清单。

历史计划、报告、版本迁移和决策过程交给 Git 历史，不放在当前维护面。变更直接更新现行接口和测试，同步仓库内引用；不保留旧路径转发、废弃配置或未使用的抽象。仍有效的终端降级、错误拒绝和安全边界属于当前功能，不能以清理历史为由删除。
