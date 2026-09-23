# nosh MVP 实施计划

> 对应设计文档 `docs/DESIGN.md`（v0.4）中的 M0 + M1，范围有所精简。目标是在 **Linux** 上跑通端到端的 MVP：可用的 AI shell、本地推理、自动下载、权限审批。
> 工作方式：在功能分支上开发，小步提交，完成后开 PR 到 `main`。遇到需要人工决策的问题，先采用设计文档的默认值，并记录在 PR 描述里，不阻塞开发。

## 1. MVP 范围

**包含**

| # | 能力 | 设计文档章节 |
|---|---|---|
| 1 | 交互式 nosh shell：嵌入 brush-core，普通命令照常执行，加载 `~/.bashrc`（`--norc` 可跳过） | §4.1、§4.3 |
| 2 | AI 触发：`#` 前缀、解析失败（含撇号判定）、命令不存在（整行静态检查）、执行失败时提示；本地拼写纠错；破坏性命令安全网 | §4.2 |
| 3 | 共享会话：agent 命令在同一个 brush `Shell` 中执行，状态延续；采集 stdout/stderr；超时；状态差异；会话状态保护 | §4.3 |
| 4 | Harness：任务循环、system prompt 加任务头、工具调度、输出截断、错误回灌、拒绝理由、步数上限 | §5.1–§5.7 |
| 5 | 工具：`run_command`、`read_file`、`list_dir`、`propose_command` | §5.5 |
| 6 | 权限 v1：基于 brush-parser 的四级风险分级；confirm / auto / yolo 三种模式；终端审批卡片 | §6.1–§6.3 |
| 7 | 推理：candle，fork `quantized_llama`（改造 #1–#3）；tokenizer；MiniCPM5 模板；采样；按 token ID 流式解析工具调用；对话内前缀复用；**进程内推理** | §7.1–§7.4 |
| 8 | 模型管理：registry、按地区和测速选源（HF / hf-mirror / ModelScope）、断点续传、SHA-256 校验、离线开关，以及 `nosh model pull/list/verify/import` | §8 |
| 9 | CLI：`nosh`、`nosh -c`、`nosh script.sh`、`nosh -a`（支持管道附件）、`nosh -s`、`nosh doctor`；退出码和无 TTY 时的约定 | §4.5 |

**不包含**（留到 M2/M3）：远程版；共享 engine 进程；磁盘前缀缓存；在其他 shell 中集成 Ctrl+G；Windows 托管 pwsh；中转 PTY 采集用户命令输出；约束解码；PLD；融合 GEMV；沙箱；MCP 与自定义工具；`write_file`/`search`；LLM 摘要式的上下文压缩（MVP 只截断旧的工具输出）。

**加分项**（时间允许时再做）：nosh 内按 Ctrl+G 就地改写；agent 命令放后台进程组，并通过 SIGTTIN 识别需要终端的命令；多源并行分段下载。

## 2. 环境

- **开发机**：Windows 11 + **WSL2 Ubuntu 26.04**（16 核 AVX-512，31 GB 内存）。Linux 是核心平台，MVP 的构建和验证都在 WSL 中进行。
- **工具链**：WSL 中已经装好 `build-essential`、`pkg-config` 和 Rust stable（rustup，含 rustfmt、clippy）。brush-core 需要 Rust ≥ 1.88。
- **构建目录**：源码在 Windows 工作树中，从 WSL 通过 `/mnt/c/...` 访问。为了避开跨文件系统的 I/O 开销，设置 `CARGO_TARGET_DIR=$HOME/.cache/nosh-target`。
- **执行方式**：在 PowerShell 中调用 WSL 时，建议把命令写进脚本文件再执行（`wsl -d Ubuntu -- bash -lc '<script>'`），直接内联容易被 PowerShell 和 wsl.exe 的引号规则破坏。
- **模型**：由 nosh 自己下载到 WSL 的 `~/.local/share/nosh/models/`，首次约 1.56 GB。
- **CI**：GitHub Actions `ubuntu-latest`，跑 `cargo fmt --check`、`clippy -D warnings`、`cargo test`。需要真实模型的测试标记为 `#[ignore]`，在本地用 `cargo test -- --ignored` 运行。

## 3. 工程结构

```text
nosh/
├─ Cargo.toml                 # workspace；统一依赖版本
├─ crates/
│  ├─ nosh-cli/               # bin "nosh"：参数分派（交互 / -c / 脚本 / -a / -s / model / doctor）
│  ├─ nosh-core/              # harness（任务循环、prompt、上下文）、tools、终端审批 UI
│  ├─ nosh-shell/             # ShellBackend（brush-core）、REPL（reedline）、AI 触发判定、拼写纠错、安全网
│  ├─ nosh-permissions/       # 风险分级（brush-parser）、规则表、策略决策
│  ├─ nosh-llm/               # candle 模型、tokenizer、MiniCPM5 模板、采样、流式工具调用解析、ChatEngine
│  └─ nosh-hub/               # registry、选源下载、校验、导入
├─ assets/registry.toml       # 内置模型清单（设计文档附录 B + 1B 条目）
├─ tests/                     # 集成测试（使用 MockChatEngine，不依赖模型）
└─ docs/                      # DESIGN.md、MVP-PLAN.md、MVP-REPORT.md
```

**依赖方向**：cli → core → {shell, permissions, llm}；hub 被 cli 和 llm 使用。

**核心接口**：按设计文档 §3.4 定义 `ShellBackend`、`PermissionEngine`、`ApprovalChannel`、`ChatEngine`，另外提供一个 `MockChatEngine`（按预设脚本输出文本和工具调用），供不依赖模型的测试使用。

**已核实的 API 线索**：
- **brush-core**：
  - `Shell::builder().build().await`；
  - `shell.run_string(cmd, &SourceInfo::default(), &params).await`；
  - `let mut params = shell.default_exec_params(); params.set_fd(OpenFiles::STDOUT_FD, file)` 可以按次重定向 stdin/stdout/stderr。`OpenFile` 支持 `File`、`PipeReader`/`PipeWriter`（来自 `std::io::pipe()`），也可以用 `openfiles::null()`；
  - `shell.env()`、`aliases()`、`funcs()`、`working_dir()`、`last_exit_status()` 可以用来计算状态差异和解析命令名；
  - 参考示例 `brush-core/examples/call-func.rs`、`custom-builtin.rs`。
- **candle**：参考 `candle-transformers/src/models/quantized_llama.rs` 做 fork；`candle_core::quantized::{gguf_file, QMatMul, QTensor}`。CPU 量化点积会在运行时检测 AVX2/FMA。
- **tokenizers 0.23**：`default-features = false, features = ["fancy-regex"]`；用 `set_encode_special_tokens(true)` 编码不可信片段。

## 4. 任务分解

每个任务结束时都要通过：`cargo fmt`、`clippy -D warnings`，以及该任务的测试。任务完成后在本节勾选。

### T0 基础设施（约 0.5 天）
- [x] 建立 workspace 骨架和各个 crate，统一 lint 配置，加上 `.gitattributes`（LF）和 CI 工作流。
- [x] 验证：WSL 中 `cargo build` 和 `cargo test` 通过；CI 变绿。

### T1 nosh-hub：模型管理（约 1–1.5 天）
- [x] 内置 `registry.toml`，记录 2B Q4_K_M/Q8_0、1B Q4_K_M 和 tokenizer，含 SHA-256、大小和 revision。
- [x] 下载器（`ureq` + `rustls`）：
  - 选源：只用 locale/时区推断地区；并行发送 HEAD，再下载 2 MB 测速；
  - 可靠性：`Range` 续传（`*.partial`）；流式计算 SHA-256；原子 rename；`fs4` 文件锁；`indicatif` 进度条；
  - 切换与重试：某个源失败时切换到下一个源，从已下载的偏移处继续；
  - 下载前检查磁盘空间。
- [x] 离线开关：`--offline`、`NOSH_OFFLINE`、`HF_HUB_OFFLINE`。开启后，任何代码路径都不访问网络。
- [x] CLI：`nosh model pull [id]`、`list`、`verify`、`import <gguf> [--tokenizer]`、`path`。
- [x] 验证：单元测试（校验、续传偏移、选源逻辑用 mock HTTP）；在 WSL 中真实下载 Q4_K_M 和 tokenizer，SHA-256 与 registry 一致。

### T2 nosh-llm：推理（约 2–3 天）
- [x] `model/llama.rs`（fork 自 quantized_llama）：
  - RoPE 表按 `context_length` 计算（默认 8K）；
  - embedding 保持量化，按行反量化；
  - 预先分配 KV，支持 `truncate`；
  - 分块 prefill（512 token/块，块与块之间检查取消标志）；
  - 只计算最后一个位置的 logits；
  - 线程数等于物理核心数。
- [x] tokenizer 加载；EOG 为 {1, 130073}；分段编码（模板骨架允许 special token，不可信内容开启 `encode_special_tokens`）；增量 UTF-8 解码。
- [x] MiniCPM5 渲染器：
  - system 通过 `<tool_def_sep>` 插入工具定义，工具 JSON 使用 Python `json.dumps` 风格的分隔符；
  - user；
  - assistant 直接拼接原始 token ids；
  - 连续的 tool 结果合并进一个 user 轮；
  - generation prompt 预填空的 think 块。
- [x] 采样：temperature、top-p、min-p；检测到复读时启用 repetition penalty 1.05；在 `<function` 区间把温度降到 0.3；支持 `--seed`。
- [x] 流式状态机：TEXT/THINK/CALL 由 token ID 8/9/18/19 驱动，DONE 由 1/130073 驱动。解析 `<function name=..><param name=..>..</param></function>`，支持 CDATA 和实体反转义，按 schema 转换类型。
- [x] `LocalChatEngine`：token 级对话日志，基于最长公共前缀复用 KV，支持取消。
- [x] 调试命令：`nosh debug gen "<prompt>"`，输出生成结果和 prefill/decode 的 tok/s。
- [x] 验证：
  - 单元测试：模板渲染与官方模板逐字节对比（期望字符串按 `chat_template.jinja` 手工推导并提交为 fixture）、工具调用解析（含 CDATA、截断）、采样；
  - `#[ignore]` 测试：真实模型能输出连贯的中英文，并在工具场景中产生可以解析的调用。

### T3 nosh-permissions：权限（约 1 天）
- [x] 用 brush-parser 遍历 AST，拆出所有简单命令：管道、列表、子 shell、`$(…)`、重定向，并展开 `sudo`/`env`/`xargs`/`nohup`/`timeout`/`bash -c`/`eval` 这类包装器；别名和函数用会话表展开（由调用方传入）。
- [x] 规则表：
  - 四个等级；
  - 网络命令至少按 Mutating 处理；
  - `find -delete/-exec`、`rm` 的参数、`sed -i`、`chmod/chown -R`、git 子命令；
  - 受保护路径、工作区外写入；
  - 解码后执行和变量拼接出的命令名；
  - 把 sudo 改写成 `sudo -n`；
  - agent 执行 `exit`/`exec` 判为 Forbidden。
- [x] 决策矩阵（confirm/auto/yolo）；用户的 allow/deny 规则；"本会话放行"。
- [x] 验证：≥ 200 条表驱动用例（覆盖四个等级、混淆写法和包装器）；Dangerous 的召回率为 100%。

### T4 nosh-shell：shell 核心（约 2–3 天）
- [x] 嵌入 brush-core：交互模式，加载 rc；用 reedline 写 REPL，包括历史文件、显示 cwd 和审批模式的提示符、Ctrl-C/Ctrl-D。
- [x] 输入流水线：
  - `#` 前缀 → AI；
  - 解析失败 → 判断是否为不完整输入（单词内撇号 → AI；否则显示续行提示）；
  - 整行静态解析命令名 → 有不存在的就先做本地模糊匹配（编辑距离 ≤ 2），匹配不上再交给 AI；
  - 安全网：破坏性命令带自然语言样式的参数时拦截；
  - 其余情况交给 brush 执行；非零退出时给出提示，这一行含中文时直接交给 AI。
- [x] `run_agent_command`：
  - 在同一个 `Shell` 中执行，stdin 接 `null()`，stdout/stderr 接 `std::io::pipe()`；
  - 用线程读取管道，实时显示并采集（有上限）；
  - 超时后中断；
  - 执行前后对比 cwd、PATH 和变量，计算状态差异；
  - 防卡住的环境变量只作用于单次执行。
- [x] `nosh -c` 和脚本：纯 brush 执行，不加载模型，也不输出额外内容。
- [x] 验证：
  - 集成测试（MockChatEngine）：`#` 能触发 AI；`gti status` 能纠正为 `git status`；agent 执行 `cd /tmp` 后，用户命令里的 `pwd` 输出为 `/tmp`；状态保护能拦下 `exec`；
  - `nosh -c 'echo hi'` 的输出恰好是 `hi\n`。

### T5 nosh-core：harness 与工具（约 1.5 天）
- [x] system prompt（静态）和任务头 `[task trigger=… cwd=… …]`、`[recent]`。
- [x] 工具注册与 schema；`run_command`、`read_file`（带行号，最多 400 行，识别二进制文件）、`list_dir`（遵循 .gitignore，深度 ≤ 3）、`propose_command`（把命令预填进下一次输入；reedline 不支持时，就打印出来并写入历史）。
- [x] 输出截断（头 60% + 尾 40%，最多 6,000 字符，完整输出落盘）；错误回灌（同一种错误最多重试 2 次）；拒绝理由；`max_steps`；达到上限时要求模型总结。
- [x] 终端审批卡片：`y`/`n`/`e`/`a`，Dangerous 需要键入 `yes`，Ctrl-C 视为拒绝；没有 TTY 时拒绝（除非传入 `--auto`/`--yolo`）。
- [x] 验证：用 MockChatEngine 跑完整的任务流测试（多步、拒绝、错误回灌、截断）。

### T6 CLI 与收尾（约 0.5 天）
- [x] `nosh`、`-c`、脚本、`-a`（stdin 作为附件）、`-s`（stdout 只输出命令）；`--auto`、`--yolo`、`--offline`、`--model-path`、`--no-download`、`--norc`、`--safe`；设计文档中的退出码约定。
- [x] 首次启动时的下载确认（默认 Y）；MVP 可以在前台下载，改成后台下载是加分项。
- [x] `nosh doctor`：检查 CPU 特性、内存、模型状态、下载源连通性和离线状态。
- [x] 配置文件 `~/.config/nosh/config.toml`：只支持 §11 中的常用项，未知项给出警告。

### T7 端到端验证与报告（约 1 天）
- [x] 在 WSL 中用真实模型跑 10 个脚本化场景，至少覆盖：查找大文件、端口占用、统计代码行数、批量重命名（需要审批）、用中文提问、拼写纠错、执行失败后分析原因、管道总结 `git log`、`nosh -s` 生成命令、agent 执行 `cd` 后状态延续。
- [x] 测量 prefill/decode 的 tok/s、首 token 延迟和 RSS，并与设计目标对比。
- [x] 撰写 `docs/MVP-REPORT.md`：场景结果（成功或失败、步数）、性能数据、已知问题和对 M2 的建议。

## 5. 验收标准

1. WSL 中 `cargo build --release` 成功；`cargo test` 全部通过；CI 变绿。
2. `nosh model pull` 下载并校验 MiniCPM5-2B Q4_K_M 和 tokenizer；使用 `--offline` 且模型已经存在时，不发起任何网络请求。
3. `nosh -c 'echo hi'` 的输出恰好是 `hi`；在交互模式下，普通命令、别名和 rc 都能正常工作。
4. `# 任务`、中文输入和命令不存在时都会触发 AI；`gti status` 会被纠正为 `git status`，但不会自动执行。
5. agent 能在共享会话中完成多步任务：Safe 命令自动执行，Mutating/Dangerous 命令先审批；agent 执行 `cd` 后状态保留。
6. `nosh -s "…"` 只输出一条可执行的命令；`nosh -a --auto "…"` 可以在无 TTY 的环境中完成简单任务。
7. `docs/MVP-REPORT.md` 给出 10 个场景的结果和性能数据。

## 6. 风险与应对

| 风险 | 应对 |
|---|---|
| brush-core 的 API 不足以支持采集、中断或状态读取 | 优先使用 `ExecutionParameters::set_fd` 加管道；做不到时，退回到用临时文件重定向；必要时 vendor 或 patch brush，并记录下来 |
| candle 0.11 与主干的 API 有差异 | 以 crates.io 版本为准；需要主干的优化时锁定 git rev；性能不达标就如实写进报告 |
| 2B 模型的工具调用不稳定 | 分区降温、错误回灌、精简工具集；把评测结果写进报告，作为 M2 约束解码的依据 |
| 在 WSL 中编译 `/mnt/c` 下的源码很慢 | 把 `CARGO_TARGET_DIR` 放在 WSL 的本地盘 |
| 模型下载慢或失败 | 测速选源并自动切换；也可以手动下载后用 `nosh model import` 导入 |
