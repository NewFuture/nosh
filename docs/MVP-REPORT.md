# nosh MVP 报告

> **定位：分阶段的实施与实测记录，不是单一构建的功能清单。** 初始范围对应 [MVP 计划](MVP-PLAN.md) T0–T7 和设计 v0.4；当前能力边界与后续规划以 [设计文档](DESIGN.md#03-实现状态) 为准。
>
> 后续记录包括内存优化（§5.3，设计 v0.6）、审批与脱敏调整（§2.2–§2.3，设计 v0.8）、多平台 CI（§2.5，设计 v0.11）、ARM 内存优化（§5.4），以及固定 seed 原生 main 基线（§3.1，设计 v0.13）。各阶段的数据保留原始构建和测量口径，不相互覆盖，也不将历史建议自动当作当前待办。

**导航**：[结论](#1-结论) · [实现与审查记录](#2-实现摘要) · [场景与基线](#3-端到端场景真实模型) · [MVP 验收](#4-验收标准逐条结果) · [性能](#5-性能) · [已知问题](#6-已知问题) · [设计偏差](#7-偏离设计之处与决策) · [后续建议](#8-对-m2-的建议)

## 1. 结论

- MVP 范围内的 9 项能力全部实现，验收标准 7 条全部满足（逐条结果见 §4）。MVP 构建有两项性能指标未达标：常驻内存（3.2–3.8 GB）和冷启动首 token 延迟（6–7 s，目标 ≤ 3 s，依赖 M2 的磁盘前缀缓存）。
- **内存优化（§5.3）之后**：加载时预先重排 Q4K 权重并释放其原始数据（给 candle 打的补丁见 `third_party/candle-core`），KV 改为 f16。8K 上下文的 RSS 峰值从 4,150 MiB 降到 2,737–2,751 MiB（2.69 GiB），达到 v0.6 的目标（约 2.9 GB，≤ 3.0 GB）；场景中（1–3K 上下文）从 3,450–3,650 MiB 降到 2,342–2,520 MiB。速度没有回退：decode 在长上下文变快（7.9K 时 11.9–12.4 → 13.1–13.8 tok/s），prefill 持平或更快；冷启动首 token 从 6–7 s 降到 4.7–5.5 s。
- 10 个真实模型场景用最终构建各跑 3 次（共 30 次）：25 次完全正确，3 次结论正确但回答里有小错（总数算错、先给出错误的中间表格、措辞），2 次失败（模型声称已经切换目录，实际没有执行 `cd`；列出改名计划后反问"是否继续"，没有执行）。temperature 1.0 下模型波动明显：两个较早构建上的三轮结果分别是 27/2/1 和 20/7/3（完全正确/有小错/失败，见 §3）。内存优化后的构建再跑 3 轮：26/4/0。
- 代码审查发现的 11 个缺陷和 4 个小问题已全部修复（§2.1），涉及审批规则、符号链接、agent 命令的超时与中断、隐藏字符和下载取消等；PR #1 上三次 Copilot 代码审查的 5 条、8 条和 5 条意见也已处理（§2.2–§2.4）。
- **平台**（issue #7，§2.5）：CI 增加 Linux aarch64 和 macOS（Apple Silicon）两个 job，三个平台的 clippy 和全部测试都通过。随之修复了 aarch64 Linux 上 debug 构建编译不过（gemm-f16）、macOS 上 agent 命令超时或中止后进程不停止，以及 macOS 上的两处权限缺口。
- **ARM 内存优化**（issue #9，§5.4）：dotprod CPU 在加载时释放层内 Q4K/Q6K 和 output 的原始数据，embedding 不动。Linux ARM 的实际 8K 样本 RSS 峰值从 3.41 GiB 降至 **2.05 GiB**，满足 ≤ 2.5 GiB；数值验收及 Linux ARM/macOS 合成正确性通过。
- 性能（WSL2，Xeon 8370C 8 核，Q4_K_M，release 构建）：decode 19–25 tok/s，prefill 102–147 tok/s，热对话首 token 0.5–1.0 s，无 rc 启动到提示符约 8 ms。
- **后续固定 seed 基线**（§3.1）：精确 main `4f602ab` 的 100 次原生观测原始为 70 通过 / 30 失败 / 0 错误，透明修复判定作用域后为 73 / 27 / 0。50/50 对最终状态一致，但判定加状态仅 45/50；#3 的复现验收未满足，不将动态输入差异直接认定为推理不确定性。

## 2. 实现摘要

| crate | 内容 |
|---|---|
| `nosh-hub` | 内置 registry（2B Q4_K_M/Q8_0、1B Q4_K_M + tokenizer，固定 revision 和 SHA-256）；按地区（locale/时区）+ 并行 HEAD/2 MB 测速选源（HF / hf-mirror / ModelScope）；64 MiB 分块 Range 下载、`.partial` 断点续传、文件锁、磁盘空间检查、失败换源与退避、边下边算 SHA-256、原子 rename；离线开关（`--offline`、`NOSH_OFFLINE`、`HF_HUB_OFFLINE`，全部网络访问经过唯一出口 `net`）；`nosh model pull/list/verify/import/path`。 |
| `nosh-llm` | fork 自 candle `quantized_llama` 的 MiniCPM5 模型（量化 embedding、RoPE 表、自有 KV 与注意力内核；KV 默认 f16；x86 在加载时预重排层内 Q4K 并释放原始数据，ARM + dotprod 扩展到层内 Q6K 及 output，见 §5.3、§5.4）；tokenizer（分段编码，不可信片段不解析 special token）；手写 MiniCPM5 模板（与 HF `apply_chat_template` 逐字节一致的 golden 测试）；采样（temperature/top-p/min-p、复读检测后启用 1.05 惩罚、`<function` 内降温到 0.3、`--seed`）；按 token ID 驱动的流式工具调用解析（CDATA、实体、按 schema 转类型）；`LocalChatEngine`（token 级对话日志 + 最长公共前缀复用 KV、分块 prefill、可取消）和 `MockChatEngine`；`nosh debug gen`（`--kv f16/f32`、`--no-prepack` 用于对比）。 |
| `nosh-permissions` | 基于 brush-parser 的 AST 分析：管道、列表、子 shell、`$(…)`、进程替换、重定向、函数定义、fork 炸弹；展开 `sudo`/`doas`/`env`/`timeout`/`nice`/`xargs`/`find -exec`/`bash -c`/`eval`/`watch` 等包装器和会话里的别名、函数；`$'\x..'` 混淆、动态命令名；规则表（git/docker/kubectl/systemctl/包管理器/网络工具等）；路径分级（受保护路径、工作区、临时目录、系统目录）；confirm/auto/yolo 决策矩阵、用户 allow/deny、"本会话同类放行"；效果未知的命令按 Mutating 处理，本地 shell 脚本读取内容后用同一个分析器分析（§16 #14，见 §2.2）；读取目标用已知的变量值和函数、脚本参数检查受保护路径（§2.3）。433 条表驱动用例，Dangerous 召回率 100%。 |
| `nosh-shell` | 嵌入 brush-core：交互/登录/`-c`/脚本/stdin 模式，rc 与 profile 加载；reedline REPL（brush 历史桥接、补全、续行校验、hinter、PS1 或 `cwd ❯` 提示符 + 右侧审批模式/YOLO 标记、Ctrl-C/Ctrl-D、Ctrl+G）；输入流水线（`#`、解析失败与单词内撇号判定、整行静态命令名检查、本地拼写纠错、破坏性命令安全网、失败提示与含中文时自动交给 AI、`ai` 内建命令）；`run_agent_command`（同一 `Shell`、stdin 为 `/dev/null`、管道采集 10 MB 上限、后台进程组、防卡住环境变量只作用于单次执行、超时与 Ctrl-C（连同脱离了进程树的后台进程一起停止，§2.3）、SIGTTIN 识别、状态差异）。 |
| `nosh-core` | 静态 system prompt + `[task …]`/`[recent]` 任务头（含 NOSH.md）；工具 `run_command`/`read_file`/`list_dir`/`propose_command`；任务循环（错误回灌同类最多 2 次、拒绝理由、步数上限后要求总结、上下文 85% 时压缩旧工具输出、Ctrl-C 取消/中止）；输出截断（头 60% + 尾 40%，6,000 字符，完整输出原样存入 `state/outputs/`，文件权限 0600；脱敏只保留 `Redactor` 扩展接口，§16 #15）；终端审批卡片（y/n/e/a，Dangerous 键入 `yes`，Ctrl-C 拒绝，无 TTY 拒绝并把命令写到 stderr）；终端渲染（`┃` 块、8 行实时输出区、`ai out <n>`）与 JSON Lines；REPL 处理器（懒加载模型、`ai mode/think/clear/ctx/status/out`、Ctrl+G 建议）。 |
| `nosh-cli` | `nosh`、`-c`、脚本、`-a`（管道附件，只读工具）、`-s`、`doctor`、`model`、`debug`；`--auto/--yolo/--offline/--model-path/--model/--no-download/--norc/--safe/--seed/--json`，`-l/-i/-e/-x/-u`；首次启动下载确认（默认 Y，前台下载）；`config.toml`（§11 常用项，未知项警告）；登录 shell 的 REPL panic 时 exec 回退 shell。 |

测试：`cargo test --workspace` 共 187 个测试（macOS 上另有 2 个只在 macOS 上运行；权限的 555 条用例和 AI 触发的 415 条语料按表驱动放在少数几个测试函数里），其中 `tests/agent_flow.rs` 用 MockChatEngine 覆盖多步任务、审批与拒绝理由、Dangerous 强确认与编辑后重新评估、`exec`/`exit` 拦截、错误回灌与放弃、截断与落盘、步数上限、无 TTY、`propose_command`、只读工具与受保护路径（含 `..`、符号链接和 nosh 自己的配置与状态目录）、超时，以及 REPL + agent 联动（`#`、`gti status`、agent `cd` 后用户 `pwd`、中文 not_found）；`crates/nosh-shell/tests/shell.rs` 覆盖快速输出下的超时、`$(…)` 与管道中进程的清理、脱离进程树的后台进程的清理（这两项在 Linux 和 macOS 上都运行，§2.5）、只含 builtin 的循环超时、作用域不泄漏、后台作业存活和提示符下 Ctrl-C；`crates/nosh-llm/tests/prepack.rs` 覆盖预重排后的矩阵乘法与原始路径一致、释放后访问原始数据报错（§5.3），并打印决定内核路径的 CPU 特性（§2.5），`tests/vendored_candle.rs` 防止重新 vendor 时丢失补丁；`crates/nosh-permissions/tests/scripts.rs` 覆盖脚本分析、子 shell 和工作区内运行时写入目标（§2.2），`tests/vars.rs` 覆盖经由变量和参数的受保护路径读取（§2.3）；`crates/nosh-cli/tests/cli.rs` 另外检查推理线程变量不进入 shell 和子进程（§2.4）。另有 6 个 `#[ignore]` 测试：4 个需要真实模型（本地已通过，含 f16 与 f32 KV 的对比），1 个注意力基准，1 个权限诊断输出。CI（ubuntu-latest：fmt、clippy -D warnings、test）每次提交都是绿色；之后增加了 Linux aarch64 和 macOS 两个 job（§2.5）。

加分项：nosh 内 Ctrl+G 就地改写（空行时解释上一条失败的命令）已实现；agent 命令放后台进程组并通过 SIGTTIN 识别需要终端的命令已实现；多源并行分段下载、后台下载未实现。

### 2.1 代码审查后的加固

T7 第一轮场景之后做了一次完整的代码审查，发现的问题全部修复并补了测试：

| # | 问题 | 修复 |
|---|---|---|
| 1 | allow 规则按整行匹配，`ls; rm -rf x` 能借 `ls*` 规则放行 | allow 规则必须匹配行内每一条简单命令，不能覆盖 Forbidden，也不适用于含隐藏字符的命令；deny 规则同时匹配列表中的每条命令和 `sudo`/`env` 等包装之后的命令 |
| 2 | "本会话同类放行"会覆盖受保护路径的读取和新增能力 | 放行记录网络、工作区外写入、修改会话状态三个标志，新命令多出任何一项都重新审批；受保护路径的读取从不被覆盖 |
| 3 | `..` 和符号链接可以绕过受保护路径检查 | `read_file`/`list_dir` 先做词法规范化，再沿符号链接（包括悬空链接）检查真实目标；命令分析也按真实路径分级（`rm` 不跟随最后一级） |
| 4 | 快速输出时超时失效，内存无上限 | 有界通道加 select 循环，持续检查超时和 Ctrl-C；超过 10 MB 采集上限的部分直接丢弃；显示区按行线性处理，未完成的行最多缓存 4 KB |
| 5 | `$(…)` 和 builtin 之后的管道阶段留在 nosh 自己的进程组，超时后仍然存活 | 通过 `/proc` 找出本次命令新增的全部后代进程：其他进程组整组发信号，nosh 进程组内的逐个发信号 |
| 6 | 放弃函数调用后变量作用域泄漏 | 记录执行前的作用域深度，放弃后精确恢复 |
| 7 | 命令返回后，后台作业因 SIGPIPE 退出 | 读取线程在命令返回后继续排空管道 |
| 8 | 只含 builtin 的循环无法超时或中断 | 交互和 agent 会话中，每个 builtin 执行前让出一次调度（约 1 µs；`-c` 和脚本不受影响），超时和 Ctrl-C（包括在提示符下）都能生效 |
| 9 | 审批卡片原样输出控制字符 | 卡片、工具输出和模型回答中的控制字符、双向控制符、零宽字符显示为可见转义；含这些字符的命令评为 Dangerous，`propose_command` 和建议直接拒绝 |
| 10 | agent 命令运行时按 Ctrl-Z 会杀掉命令 | 运行期间关闭终端的 VSUSP |
| 11 | 下载时 Ctrl-C 无法取消 | 下载循环检查取消标志；加载期间按 Ctrl-C 放弃当前任务（退出码 130） |

小问题：被中断的 `git status` 会留下 `index.lock`（任务头改用 `git --no-optional-locks`）；连按两次 Ctrl-C 中止任务改为按单条命令计数；脱敏改为整行扫描，并覆盖更多 `key=value`、`"key": "value"`、`Authorization: Bearer` 形式（脱敏后来按 §16 #15 移除，见 §2.3）；`read_file` 在 `end_line < start_line` 时返回错误，不再下溢。

### 2.2 第二轮审查（PR #1 上的 Copilot 代码审查）

| # | 问题 | 修复 | 提交 |
|---|---|---|---|
| 1 | 校验缓存只比较秒级 mtime，同一秒内（或保留 mtime 的 `cp -p`）换成同样大小的文件时，不重新计算哈希就当作已校验 | manifest 升到版本 2，每个文件记录 `FileStamp`：大小、纳秒级 mtime，Unix 上再加 dev、inode 和 ctime（其他平台用创建时间）；版本 1 的记录一律不信任，旧的模型目录会重新校验一次；校验前先取 stamp，哈希期间文件变了就不记录 | `1931aae` |
| 2 | 不在规则表里的命令、按路径执行的程序、用解释器执行脚本或内联代码的命令，都被当作"工作区内的 Mutating"，auto 模式下不经确认直接执行 | 先按意见改为"效果未知、auto 模式下也要单键确认"（`66d2725`）；随后按用户"方便优先"的决定（§16 #14）撤回（`fe649b5`）：这类命令仍按 Mutating 处理，auto 模式下自动执行，改为分析本地 shell 脚本的内容（`./x.sh`、`/path/x.sh`、`bash x.sh`；单个脚本 256 KiB、每条命令共 1 MiB），只有其中的 Dangerous/Forbidden 才升级；脚本、`bash -c`、子 shell 和 `$(…)` 里的 `exit`/`exec`/`cd`/函数定义只作用于该子 shell。同一提交把工作区内运行时才确定的写入目标（`for f in *.txt; do mv …`、`find . -exec cp {} {}.bak`）从 Dangerous 降为 Mutating；删除和可能离开工作区的目标仍为 Dangerous | `66d2725` → `fe649b5`、`146118a` |
| 3 | `read_file` 先读 8 MiB 前缀再定位 `start_line`，读不到大文件后面的部分，总行数也只是前缀的 | 流式读到请求的行，只保留要返回的行（最多 400 行、每行 2 KiB、共 6,000 字符）；之后在 64 MiB 的预算内继续数行，数不完时标注"≥ N lines (stopped counting)" | `1f20dca` |
| 4 | 等待另一个进程的下载锁时用阻塞的 `File::lock()`，Ctrl-C 无效 | 每 200 ms 轮询一次 `try_lock()`，其间检查取消标志；等待时提示"另一个 nosh 进程正在下载" | `b31e7ae` |
| 5 | 测速没有计入第一次读取的字节；服务器只返回一块就断开时，回退逻辑把它当作完整的 2 MiB，可能把坏掉的源排在第一 | 每次读取都计入，第一个字节只用来开始计时；在达到测速大小（或更小的文件大小）之前结束的响应算作测速失败 | `8f1554a` |

每项都有回归测试。

### 2.3 第三轮审查（PR #1 上的第二次 Copilot 代码审查）

| # | 问题 | 处理 | 提交 |
|---|---|---|---|
| 1 | 脱敏在带引号、含空格的值（`password="a b c"`）处提前停止，后半截留在输出日志里 | 按用户决定（§16 #15）本地 agent 受信任，移除脱敏实现，完整输出原样写入 `state/outputs/`（文件 0600、目录 0700）；只保留 `Redactor` 扩展接口（本地为不复制内容的 `NoRedact`），接入远程 agent 时再实现具体规则，届时把这个例子加入回归测试 | `6c0dab9` |
| 2 | 动态读取目标被忽略：`cat "$KEY_PATH"` 在变量值为 `~/.ssh/id_rsa` 时仍是 Safe；函数体分析时不代入调用参数，受保护路径的确认可以绕过 | 分析时已知的值像字面路径一样检查受保护路径：会话中的标量变量（连同导出标志）；同一行里之前的赋值（`read`、`unset`、循环、算术、`source`、子 shell 等使之失效）；行内定义的函数在每次调用时按参数再分析一遍（只取结论，不重复应用 `cd` 等效果）；会话函数、脚本和 `bash -c` 的 `$1`…；子进程只看到导出的变量和 `NAME=value cmd`。无法确定的值不改变分级、不额外确认（§16 #14）。已知值只用来增加确认：流不敏感的分析在分支或循环之后可能拿到过时的值，所以用变量拼出的写入、删除目标和命令名仍按运行时计算处理。脚本里的受保护路径读取现在也会提出来 | `a5f984b` |
| 3 | `read`、`hash` 等会修改会话的形式被当作只读（`read PATH <<< /tmp`、`hash -p /tmp/evil ls`） | `read`/`mapfile`/`readarray`/`printf -v`/`getopts`/`let` 的赋值按变量赋值规则分级（改 `PATH` 等关键变量需要确认）；`hash -p/-d/-r` 修改会话，查询仍是 Safe；`fc -l` 为 Safe、`fc -s`（重新执行历史命令）为 Dangerous、其他形式为 Mutating；`stty`、`mesg` 修改终端设置 | `fb50268` |
| 4 | 超时和 Ctrl-C 的清理只找 nosh 的后代进程：double-fork 或 `setsid` 之后、父进程已退出的进程被重新挂到 init 下，清理不到 | 每次 agent 命令在环境中设置 `NOSH_AGENT_RUN=<pid>.<序号>`（与防卡住变量一样只作用于这一次）；超时或 Ctrl-C 时，环境里带有本次取值的进程无论挂在哪里都一起停止，连同其后代。之前启动的用户后台作业不带这个值，不受影响。没有采用 subreaper：孤儿进程不会挂到 nosh 下，也就不必额外回收 | `bc7f346` |
| 5 | `--model-path` 指向含多个 `.gguf` 的目录时按文件系统顺序任取一个，`--model` 不起作用 | 目录里只有一个 `.gguf` 时直接使用；有多个时选请求模型（未指定时为默认模型）在 registry 中的文件名，找不到就报错并列出候选 | `96e0c77` |
| 6 | `SharedProgress::finish` 不设置完成标志，复用时 `start` 不清除上一次的完成和失败状态 | `start` 重置状态（保留最后一条说明）；`finish` 记录完成，失败时设置失败标志，成功时进度置满 | `e187c9a` |
| 7 | GGUF 的 attention head 数为 0 时，加载在除法和取模处 panic（`--model-path` 的文件不经过 registry 哈希校验） | `LlamaConfig::from_gguf` 在做除法前检查两个 head 数和维度能否整除，返回加载错误 | `ea4920d` |
| 8 | vendored candle 的 Metal 桩函数 `quantize_onto` 返回 CUDA 的错误（上游问题） | 改为 `NotCompiledWithMetalSupport`，并重新生成 `nosh.patch`、在 `NOSH_PATCH.md` 中记录；新增测试防止重新 vendor 时丢失补丁 | `ebf7cc6` |

每项都有回归测试。

### 2.4 第四轮审查（PR #1 上的第三次 Copilot 代码审查）

| # | 问题 | 处理 | 提交 |
|---|---|---|---|
| 1 | `LocalChatEngine::load` 用 `std::env::set_var` 设置 `CANDLE_NUM_THREADS`/`RAYON_NUM_THREADS`，此时 shell 的 Tokio 运行时等线程已经存在，修改环境变量可能与其他线程的读取竞争（未定义行为） | 改为在 `main` 开头、任何线程启动之前设置（`configure_thread_env` 改为 `unsafe fn` 并写明这个前提，debug 构建用 `/proc/self/task` 检查），随即登记原值，shell 照旧不把它们传给子进程；加载时只读取线程数。candle 的私有 rayon 池和 barrier pool 都只读这两个环境变量，没有可用的 API。代价是每次启动多约 0.4 ms（计算物理核心数；`nosh -c true` 3.2–3.4 ms/次，同时测得 `bash -c true` 4.1–4.2 ms/次）；decode 速度不变（短 prompt 23.5–24.5 tok/s，8 个 barrier 线程） | `a2f4247` |
| 2 | `nosh doctor` 的内存检查写死 3.5 GB 和 2B 的提示，不看 registry 的 `min_memory_mb`：Q8_0（3.8 GB）可能在内存不足时显示正常，1B 模型却收到 2B 的警告 | 按已安装模型（否则按选中的或默认的 registry 条目）的 `min_memory_mb` 加 512 MiB 余量检查，警告里写出该模型 | `d4940b9` |
| 3 | 首次启动时，`--model-path` 缺失或有歧义这类解析错误被当成"没有模型"，会下载 registry 模型，而加载时仍然用那个错误的路径 | 先解析：出错就报告错误；只有确实没有模型、允许下载且没有拒绝过时才提议下载 | `830ebdb` |
| 4 | 下载失败后的指数退避（最长 8 s）是一次整段 sleep；Ctrl-C 只设置取消标志，要等 sleep 结束才生效 | 退避按 200 ms 分段等待，取消后立即返回 `Cancelled`；取消检查一路传下去（等锁、退避、读分块），测试可以不依赖全局标志 | `37740da` |
| 5 | `net::head` 对 5 s 以内的超时一律用共享 agent 的固定 5 s 超时，测速传入的 3 s 预算不起作用，`probe_all` 结束后探测线程还可能继续发请求 | 仍用共享 agent 复用连接，但每个请求设置调用方的超时（整次请求含重定向），长短超时都生效 | `a45e274` |

每项都有回归测试。之后再次请求审查，唯一的新意见是"加载时每个 Q4K 张量开一个线程重排，会同时创建数百个线程"：实际上 `prepack_q4k` 按层调用、每层 7 个矩阵，线程在进入下一层之前全部 join，同时最多 7 个线程，因此只在注释里写明了这一点（`8254d10`）。

第五次 Copilot 审查的 4 条按最小改动修复：`check_dir` 和 `nosh model verify` 遇到哈希期间文件有变化（拿不到初始 stamp，或 `record_verified_as` 返回 `Ok(false)`）时算作校验失败，只读库写不了 manifest 仍然忽略（`8c0f37f`）；识别不出来的 GGUF 显式给了未知的 `--model` 时报错，不再换成默认模型（`b2bbcdf`）；integer 参数的 float 回退只接受有限、整数值且在 i64 范围内的值（`53fa89f`）。第六次（推送后自动触发）的 4 条同样按最小改动修复：配置文件存在但读不了时保留默认值并给出警告，不再当作不存在（`b4de458`）；建议模式从 fenced 代码块取出完整的多行命令（`52e7dcd`）；对已有文件和已下载部分的哈希可以被 Ctrl-C 打断，`.partial` 保留（`a9bc41c`）。

### 2.5 多平台 CI（issue #7）

CI 原来只有 ubuntu-latest（x86_64）。现在有三个 job，都跑 clippy（`-D warnings`）和全部测试，fmt 和单独显示指标的 `ai trigger corpus` 步骤只在 x86_64 上跑；arm64 和 macOS 的 workspace test 同样覆盖这组语料（`82e65fd`，rebase 前为 `d3190e8`）：

| job | runner | 触发 | 测试日志里的 CPU 特性 | candle 的内核路径 |
|---|---|---|---|---|
| `check` | `ubuntu-latest`（Ubuntu 24.04，x86_64，2 vCPU） | 每次 | 随 runner 不同：一次为 avx avx2 fma f16c avx512f avx512bw avx512vl avx512vnni，一次只到 avx2 | VNNI 或 AVX2 tile；Q4K 都预重排并释放原始数据，Q8_0 只在有 VNNI 时释放 |
| `linux-arm64` | `ubuntu-24.04-arm`（2 vCPU） | 每次 | neon dotprod i8mm fp16 bf16 | ARM 重排：m == 1 用 dotprod gemv，m 为 4 的倍数用 i8mm tile |
| `macos` | `macos-latest`（macOS 26，Apple M1，3 vCPU） | PR、main、手动触发（单价约为 Linux x64 的 10 倍） | neon dotprod fp16 | ARM 重排：dotprod gemv 和 dotprod tile（没有 i8mm） |

- 每个 job 的最后一步用 `--nocapture` 重跑 `crates/nosh-llm/tests/prepack.rs`，打印 CPU 特性（`nosh_llm::cpu::features()`，检测方式与 candle 相同）和各个 m 下与原始内核的误差；前面的步骤失败时也运行。`nosh doctor` 改用同一个函数，aarch64 上原来只显示 `neon`（`9f9fa1e`）。
- 每个 job 限时 30 分钟。缓存命中时 x86_64 约 2.5 分钟、arm64 约 5 分钟、macOS 约 2 分钟；macOS 第一次（没有缓存）约 4.5 分钟。
- #7 当时的 `prepack.rs` 在 ARM 上 `released=false`，需要释放的检查按预期跳过；40 行和 48 行在 ARM 上走同一类内核，不能作为真正的 packed/raw 对照。#9 已改为不满足 tile 对齐的奇数行 raw 对照，并强制支持的 ARM CPU 实际释放，最新结果见 §5.4。

发现并修复的问题：

| # | 平台 | 问题 | 处理 | 提交 |
|---|---|---|---|---|
| 1 | Linux aarch64 | `cargo test` 编译失败：gemm-f16 的 aarch64 内核是启用 fp16 的函数，调用的 `#[inline]` 辅助函数里有 fp16 汇编（`fmla v.8h`）。debug 构建不内联，辅助函数单独编译时没有 fp16 特性，而 aarch64 Linux 的基线不含 fp16，汇编器报 "instruction requires: fullfp16"。clippy 不生成代码，所以查不出来；Apple 的基线含 fp16；release 构建会内联 | dev 构建只对 gemm-f16 开 opt-level 3。没有在 RUSTFLAGS 里加 `+fp16`，所以二进制仍能在没有 fp16 的 CPU 上运行（内核按运行时检测分派）。上游问题见 sarah-quinones/gemm#31 | `4ca3910` |
| 2 | macOS | 没有 `/proc`，找不到 agent 命令启动的进程；brush 0.5 丢弃命令时也不杀子进程，所以超时和 Ctrl-C 之后命令继续运行（如 `yes`） | 用 libproc（`proc_listallpids`，`proc_pidinfo` 取 ppid 和 pgid）列出进程，用 `sysctl(KERN_PROCARGS2)` 读 `NOSH_AGENT_RUN`；按平台拆开的函数也消除了非 Linux 上的 `unused_mut` 警告 | `f2a48e0` |
| 3 | macOS | `/etc`、`/tmp`、`/var` 是指向 `/private` 的符号链接：指向 `/etc/hosts` 的链接解析成 `/private/etc/hosts` 后不再受保护；临时目录（`/var/folders/…`）下的受保护路径与解析后的形式对不上 | macOS 上比较路径时，两边（路径、受保护位置、工作区、home、`$TMPDIR`）都去掉 `/private`；`/System`、`/Library`、`/Applications`、`/Volumes`、`/private` 按系统目录处理，`/Users` 本身与 `/home` 相同。代码审查又发现，判断"顶层目录"（递归删除时为 Forbidden）时数的是原样路径的层数，`rm -rf /private/etc`、`rm -rf /private/var` 只算 Dangerous，而它们才是真正的目录；改为按去掉 `/private` 之后的形式计算，与 `rm -rf /etc` 一样为 Forbidden | `8a05ecb`、`4b7391d` |
| 4 | macOS；`NOSH_HOME`、`XDG_CONFIG_HOME` | 受保护列表写死了 `~/.config/nosh` 和 `~/.local/share/nosh/state`，而 macOS 上 nosh 的配置和状态在 `~/Library/Application Support/nosh` | agent 的权限上下文加上 nosh 实际使用的配置和状态目录 | `9e9c163` |
| 5 | macOS | 测试默认了 Linux：清理测试读 `/proc`；临时目录在符号链接之后，而 `cd` 保留给定的路径；BSD `ls` 对不存在的文件退出 1，GNU 是 2 | 清理测试改用 `ps` 列进程，在 Linux 和 macOS 上都运行（`setsid` 那一步只在有 `setsid` 时做）；`tmpdir()` 返回 canonicalize 后的路径；按 `ls` 实际的退出码断言 | `f2a48e0` |
| 6 | x86_64（2 vCPU 的 runner） | `timeout_stops_processes_that_left_the_process_tree` 偶发失败：brush 把 `cmd &` 作为任务运行，agent 命令记录已有的子进程时，用户的后台作业可能还没 fork，于是被当成 agent 命令启动的进程一起停止 | 测试先等后台作业的进程出现（见 §6 #5） | `264adde` |
| 7 | aarch64 | vendored candle 的 `mark_released` 只在 x86_64 上调用，其他架构报 dead_code 警告 | 与 `x86_prepack` 一样限定为 x86_64，同步更新 `nosh.patch` | `336d165` |
| 8 | macOS | `vars.rs` 的 5 个测试并行创建脚本夹具，目录名只用 pid 和墙钟纳秒；macOS 的墙钟分辨率可能让多个测试拿到同一个时间，先结束的测试删掉另一个测试的脚本，于是脚本内的受保护路径读取偶发漏报 | 在时间之外再加进程内原子序号；同样修正 `scripts.rs` 的夹具。20 轮、每轮 8 线程的压力测试通过 | `ffe034a` |

推送前在 WSL 里做了交叉检查：`aarch64-apple-darwin` 和 `aarch64-unknown-linux-gnu` 两个目标的 `cargo clippy --workspace --all-targets -- -D warnings`，以及 aarch64 Linux 的 `cargo test --no-run`。ring 等 C 依赖用一个只生成空文件的编译器和链接器代替，因为这里不需要能运行的产物。第 1 项就是这样在本地复现并验证的：它只在生成代码时出现，只做 clippy 的交叉检查发现不了。

## 3. 端到端场景（真实模型）

后续的固定 seed 评测见 [`eval`](../eval/README.md)。新套件按下列 10 个场景的意图重建夹具，每场景默认 5 个 seed，使用独立进程和自动二元判定；不沿用未入库的旧夹具、跨场景会话或“有小错”分级。因此新的通过率与冷会话耗时不应直接与本节的手工结果比较。原始 main 的有限观测实测和合并后补录的完整正式基线分别标注。

**环境**：Windows 11 + WSL2 Ubuntu 26.04，Intel Xeon Platinum 8370C（8 核 16 线程，AVX-512 VNNI），31 GB 内存；Rust 1.98.1；模型 MiniCPM5-2B Q4_K_M（由 `nosh model pull` 下载，SHA-256 与 registry 一致）；上下文 8K；采样用设计默认值（temperature 1.0）。

**方法**：交互场景用 Python pty 驱动真实的 `nosh --norc`（发送按键、按提示回答审批），CLI 场景直接调用 `nosh -a`/`nosh -s`。夹具在 `~/nosh-e2e`（不同大小的文件、一个多语言小项目、待重命名的 `.txt`、用本仓库 git bundle 克隆的仓库、`python3 -m http.server 8080` 监听端口）。所有场景用最终构建（`56d11d3`；之后的 `cd2525b` 只改了用户 allow 规则的判定，场景中没有配置规则，不影响结果）连续跑 3 轮，每轮重建夹具；`NOSH_STATS=1` 记录每个任务的 token 数、速度、首 token 延迟和 RSS。

| # | 场景 | 输入 | 3 轮结果 | 步数 | 任务耗时 |
|---|---|---|---|---|---|
| 1 | 查找大文件 | `# find the 3 largest files under this directory` | ✔ ✔ ✔ | 3 / 4 / 4 | 16.9 / 23.8 / 23.8 s（首个任务，含冷 prefill） |
| 2 | 端口占用 | `# which process is listening on port 8080?` | ✔ ✔ ✔ | 2 / 2 / 2 | 3.7 / 4.1 / 5.2 s |
| 3 | 统计代码行数 | `# count the lines of code in this project, grouped by language` | ✔ ✔¹ ✔² | 4 / 4 / 3 | 32.3 / 33.1 / 24.1 s |
| 4 | 批量重命名（需审批） | `# rename every .txt file in this directory to .md` | ✔ ✔ ✘ | 4 / 4 / 2 | 16.7 / 16.9 / 14.1 s |
| 5 | 中文提问 | `# 这个项目里有哪些 Python 文件？` | ✔ ✔ ✔³ | 2 / 2 / 2 | 5.2 / 9.7 / 5.2 s |
| 6 | 拼写纠错 | `gti status`、`pyhton3 --version` | ✔ ✔ ✔ | 0（本地） | 4 ms（Linux PATH）/ 79 ms（WSL PATH） |
| 7 | 执行失败后分析原因 | `python3 broken.py`（exit 1）后输入 `#` | ✔ ✔ ✔ | 2 / 2 / 2 | 11.4 / 20.3 / 11.0 s |
| 8 | 管道总结 git log | `git log --stat -8 \| nosh -a "summarize …"` | ✔ ✔ ✔ | 1 / 1 / 1 | 30.6 / 28.0 / 41.0 s（不含 2–3 s 模型加载；2.5K token 冷 prefill 约 18 s） |
| 9 | `nosh -s` 生成命令 | `nosh -s "compress the logs directory into logs.tar.gz"` | ✔ ✔ ✔ | 1 | 9.5–10.1 s（进程总耗时，含加载模型） |
| 10 | agent `cd` 后状态延续 | `# change the current directory to data and list the files there`，然后用户执行 `pwd` | ✘ ✔ ✔ | 2 / 2 / 2 | 4.5 / 6.2 / 6.2 s |

¹ 第 2 轮先给出一张错误的表格（每个文件 1 行），接着说"逐个读取核实"，最后给出正确的表格和总数 29。
² 第 3 轮各语言行数都对，但总结写成"8 个文件，共 36 行"（应为 6 个文件、29 行）。
³ 第 3 轮正确列出 3 个文件，但把行数写成了"第 5 行"（应为"5 行"）。

**逐项说明**：

1. **查找大文件**：三轮都先用 `list_dir` 浏览 5 个目录。第 1 轮直接根据 `list_dir` 的大小得出正确的前三（20.0 MB、11.4 MB、4.8 MB）；第 2 轮再用 `find … -exec ls -la {} \;` 列出全部文件，第 3 轮用 `find … -exec ls -lh {} \; | sort -k5 -rh | head` 核对，结论都正确。
2. **端口占用**：三轮都用 `ss -tlnp | grep 8080` 找到 `python3` 及其 PID（均为 Safe，自动执行）。
3. **统计代码行数**：模型先用 `list_dir` 浏览，再用 `read_file` 读每个文件并计数（第 1 轮还用 `cat … | wc -l` 核对了总数），各语言行数三轮都对，但步数偏多（3–4 步，24–33 s），总数和中间表格偶尔出错。
4. **批量重命名**：成功的两轮生成 `for f in *.txt; do mv "$f" "${f%.txt}.md"; done`，因为写入目标是运行时计算的路径被评为 **DANGEROUS**，审批卡片要求键入 `yes`（`146118a` 之后，工作区内的这类改名评为 Mutating，§2.2）；执行后 4 个文件改名，`readme.md` 保持不变。**失败的第 3 轮**：模型列出目录后给出改名计划，然后反问"Would you like me to proceed?"，没有发起命令（审批卡片本身就是确认环节）；回答开头还模仿任务头写了一行 `[task complete=hash …]`。
5. **中文提问**：三轮都用中文回答，列出正确的 3 个文件。
6. **拼写纠错**：`gti status` → 输入行预填 `git status`，`pyhton3 --version` → `python3 --version`，都不自动执行，也不调用模型。
7. **失败分析**：`python3 broken.py` 失败后提示 `✗ exit 1 · Ctrl+G or # to ask AI`；输入 `#` 后，agent 读取脚本，指出缺少 `config.json`，并给出修复方法。第 2、3 轮沿用了上一轮中文提问的语言，用中文回答。
8. **管道总结**：stdin 截断后作为附件（只读工具），三轮都准确概括了本仓库最近的提交（CLI、core、shell、permissions、llm、hub、CI）。
9. **`nosh -s`**：三轮 stdout 都恰好是一行 `cd logs && tar -czvf ../logs.tar.gz .`（`bash -n` 语法检查通过），说明写到 stderr，退出码 0。
10. **状态延续**：成功的两轮中 agent 执行 `cd …/data && ls -la`，界面提示 `cwd → …/data`，随后用户输入的 `pwd` 输出 `/home/newfuture/nosh-e2e/big/data`。**失败的第 1 轮**：模型只对 `data` 调用了 `list_dir`，却回答"The current directory is now …/data"；`pwd` 显示目录没有变化。

**不同构建的三轮结果**（同一套场景和夹具；完全正确 / 有小错 / 失败）：

| 构建 | 说明 | 结果 |
|---|---|---|
| `3e66f8b` | 代码审查之前 | 27 / 2 / 1 |
| `b60e727` | 审查修复之后；模型可见的变化只有脱敏规则、中文任务头的 `lang=zh` 和规范化的工具路径 | 20 / 7 / 3（场景 1 只对 1 轮，场景 3 没有一轮完全正确） |
| `56d11d3` | `list_dir` 大小改为同一单位（最终构建） | 25 / 3 / 2 |
| `ed81c8d` | 内存优化（§5.3：Q4K 预重排并释放原始数据、KV f16） | 26 / 4 / 0（小错：场景 3 三轮总数都算错，场景 5 有一轮说错文件位置） |

`b60e727` 相对 `3e66f8b` 在这些英文场景里几乎没有改变模型看到的内容（脱敏规则对这些输出不起作用，只有工具路径的写法可能不同），差异主要来自 temperature 1.0 下的采样波动；3 轮样本太少，不能据此比较构建之间的优劣。

**调优记录**：

- 场景 3 首次试跑失败：`list_dir` 把大小显示为 `86B`，模型把它当成了行数。改为 `(86 bytes)`，并在工具描述中写明"只列名称和大小，计数与搜索用 run_command"之后，3 轮都正确。
- 场景 10 最初的措辞是"go into the data directory and list what is there"，有一轮模型直接对 `data` 调用 `list_dir`，没有 `cd`，测不到状态延续。改用明确的"change the current directory to data"。
- 试跑中长 prompt 的 prefill 明显变慢，定位到注意力内核（见 §5.2）。
- 场景 5 有一轮用英文回答中文问题。输入或失败的命令里含中文时，任务头加上 ` lang=zh`。
- 场景 1 的主要失败方式是：`list_dir data` 同时列出 `dump.bin (20.0 MB)` 和 `small.bin (781.2 KB)`，模型把 781.2 KB 排在 11.4 MB 前面。先试了在工具描述里加"计数、排序、搜索用 run_command 配合 wc/du/sort/find/grep"：单独运行场景 1 时，基线与改动都是 5/6 正确；在会话 A 的上下文中，基线 3/5，改动 2/5，没有改善，已撤回。随后改为同一次列表内的大小统一使用最大文件的单位（`small.bin (0.8 MB)`、`m1.py (<0.1 MB)`）：在会话 A 的上下文中跑 6 次，5 次先用 `list_dir` 浏览（3 次直接据此作答，2 次再用 `find` 核对），全部正确；另 1 次一开始就用 `find`，三个文件对了但顺序错了。改动前，没有用命令核对、只根据 `list_dir` 作答的 7 次里只有 1 次正确。

**离线验证**：用 `LD_PRELOAD` 拦截 `socket`/`connect`/`getaddrinfo` 做计数。对照组 `nosh doctor`（联网）记录到 3 次 inet 连接和 6 次 DNS 解析；`nosh --offline -a --auto "how many files are in the data directory?"` 完成任务（退出码 0），期间没有任何 inet socket 和 DNS 解析，只有一次连接本机 `/run/systemd/userdb` 的 Unix socket（glibc NSS 查询用户信息）。另外，在没有网络的 `unshare -rn` 命名空间里执行 `nosh --offline -s …` 也能正常输出命令。以上检查在最终构建上复测，结果相同。

### 3.1 固定 seed 的原生 main 基线（2026-09-25）

被测源码固定为 **`4f602ab8d95d046162adb7d4b202ddf6d3e20bea`**（合并 #13），在任何评测代码修改前从干净 Git 归档隔离构建 release。使用同一 WSL2 Ubuntu 26.04.1、Xeon 8370C（16 逻辑 CPU、约 31 GiB RAM）、Rust 1.98.1，以及哈希核对一致的 MiniCPM5-2B Q4_K_M/tokenizer。原样运行该 main 的标准库评测器，8 推理线程、Rayon 1、C.UTF-8/UTC、独占 `/tmp/nosh-eval-1000`，10 场景 × seeds `[0,1,2,3,4]` × 2 轮，只有一个串行 campaign，无重试或补采样。

原始结果为 **70 通过 / 30 失败 / 0 运行器错误 / 0 缺失**。逐份答案审计确认三次误拒：正确的前三名之后另列较小参考文件；Python 10+5 的分组小计被各自当成完整语言总数；中性目录概览被当成 Python 分类。最小判定修复后对同一观测重评为 **73 / 27 / 0 / 0**，其中真实模型 **63/90** 通过、本地纠错 **10/10** 通过，整体 73% 不等于模型任务通过率。所有 `original_judgment` 和回答原文保留，原始 JSON/Markdown 可逐字节重建；原始输入、工具、指标、审批和最终状态未改。完整[报告](../eval/baselines/main-4f602ab/report.md)、[构建清单](../eval/baselines/main-4f602ab/build-info.json)、[来源与哈希](../eval/baselines/main-4f602ab/provenance.json)、[逐项分析](../eval/baselines/main-4f602ab/analysis.md)已记录。历史 `main-7c57a88` 的 legacy 36/50 通过、14 失败、0 错误记录保持不变；其观测限制和单轮规模不能充当这次原生双跑，也不与本次数字直接作受控回归比较。

| 场景 | 原始通过/10 | 重评通过/10 | 判定+状态一致/5 | TTFT 中位 s | 进程耗时中位 s | RSS 最大 MiB |
|---|---:|---:|---:|---:|---:|---:|
| 大文件 | 9 | 10 | 5 | 5.03 | 15.10 | 2388.21 |
| 端口 | 10 | 10 | 5 | 5.15 | 11.99 | 2362.61 |
| 语言行数 | 0 | 1 | 4 | 5.12 | 38.62 | 2409.88 |
| 改名 | 10 | 10 | 5 | 4.80 | 17.58 | 2410.70 |
| 中文 Python 文件 | 7 | 8 | 5 | 4.81 | 21.84 | 2395.23 |
| 本地纠错 | 10 | 10 | 5 | N/A | 0.06 | 18.96 |
| 失败分析 | 8 | 8 | 3 | 4.97 | 29.95 | 2395.69 |
| git 摘要 | 10 | 10 | 5 | 8.06 | 32.05 | 2423.92 |
| 归档建议 `-s` | 6 | 6 | 3 | 2.12 | 6.97 | 2355.93 |
| cwd 连续性 | 0 | 0 | 5 | 4.73 | 10.18 | 2395.39 |

**复现验收仍未满足**：原始判定+状态一致 44/50，重评后 45/50（模型配对 40/45、本地纠错 5/5），最终状态均为 50/50 一致。剩余差异是语言行数 seed 2（第二轮总数错为 30 行/5 文件）、失败分析 seeds 0/3（某轮请求重跑 Python，被既定审批策略拒绝后任务状态 incomplete）、归档建议 seeds 1/3（某轮目标路径/输入范围错误）。回答原文 43/50 不同、工具 17/45 不同、可观测输入 45/45 不同。所有模型配对的初始接口输入，在仅用于分析地忽略任务时间及最近命令耗时后相同；实际推理输入从未做此处理。后续还可有监听 PID、工具耗时、目录时间和工具分支变化，不能从这些不同输入下的答案推断数值推理不确定性，#3 不自动关闭。

**主要失败方式**：10 次 cwd 任务都只调用 `list_dir(data)`，实际 `pwd` 未变；9 次行数回答混淆文件数/字节数、漏文件、错分语言或总计；4 次归档建议使用错误目标/通配范围或占位路径；2 次将 shell 文件明确列入 Python 分类；2 次失败分析因审批拒绝后的 incomplete 状态未通过。通过只表示满足明确判定事实，不保证回答中所有旁支解释都正确。

**指标口径**：每次独立进程是冷会话/KV，未清空 OS 页缓存；模型哈希检查和先前试验也可能暖页。TTFT 取首步原生 Usage，排除模型加载，90/90 模型试验可测，中位数 4.90 s；本地纠错 10 次均为 0 步、TTFT=N/A。所有 100 次进程总耗时中位数 16.93 s（仅 90 次模型试验为 17.85 s），含加载、交互及退出，排除夹具创建和建议验证；步骤/确认均值分别为 2.18/0.14。RSS 是 Linux wait4 的逐个 nosh 峰值（模型试验范围 2297.21–2423.92 MiB），含内核对已等待后代的统计，不是进程树求和，端口监听器/归档验证器另起进程。失败试验的有效指标同样计入，不能与上文热会话/手工结果混比。

## 4. 验收标准逐条结果

| # | 标准 | 结果 |
|---|---|---|
| 1 | WSL 中 `cargo build --release` 成功；`cargo test` 全部通过；CI 变绿 | ✔ 全新 release 构建（fat LTO）3 分 21 秒，二进制 14 MB；`cargo test --workspace` 全部通过；每次推送的 CI 都是绿色 |
| 2 | `nosh model pull` 下载并校验 Q4_K_M 和 tokenizer；`--offline` 且模型已存在时不发起网络请求 | ✔ 从 huggingface.co 下载用时 1 分 28 秒，SHA-256 一致；离线验证见 §3 |
| 3 | `nosh -c 'echo hi'` 输出恰好是 `hi`；交互模式下普通命令、别名和 rc 正常 | ✔ `od -c` 显示 `h i \n`，stderr 为空（有集成测试）；pty 测试中加载 Ubuntu 默认 `~/.bashrc`，PS1、别名、多行 `for`、Ctrl-C 均正常 |
| 4 | `# 任务`、中文输入、命令不存在都会触发 AI；`gti status` 被纠正为 `git status`，但不会自动执行 | ✔ 单元测试、MockChatEngine 集成测试和场景 5/6 均覆盖 |
| 5 | 在共享会话中完成多步任务：Safe 自动执行，Mutating/Dangerous 先审批；agent `cd` 后状态保留 | ✔ 场景 1–4、7、10：Safe 命令自动执行，重命名经 Dangerous 审批后执行，agent `cd` 后用户的 `pwd` 一致；审批流程和状态延续另有集成测试。最终 3 轮中有 2 次因为模型没有发起命令而失败（场景 4、10 各 1 次，见 §3），机制本身没有出错 |
| 6 | `nosh -s` 只输出一条可执行的命令；`nosh -a --auto` 在无 TTY 环境中完成简单任务 | ✔ 场景 9；无 TTY 的脚本里 `nosh --offline -a --auto …` 退出码 0 |
| 7 | `docs/MVP-REPORT.md` 给出 10 个场景的结果和性能数据 | ✔ 本文 |

## 5. 性能

### 5.1 与设计目标对比

8 个推理线程（物理核心数），Q4_K_M，上下文 8K。decode/prefill 的基准数据来自 release 构建（`nosh debug gen`）；场景中的数据和延迟数据来自最终 3 轮使用的 `fastrel` 构建（继承 release，thin LTO、16 个 codegen unit）。除内存和冷启动两行外，表中是 MVP 构建的数据；内存优化后的构建速度持平或更快，对比见 §5.3：

| 指标 | 目标 | 实测 | |
|---|---|---|---|
| decode | ≥ 12 tok/s | 24.6–25.7 tok/s（短上下文）；19.4–23.1 tok/s（场景中，1.1–3.0K 上下文）；16.4 tok/s（4.4K 上下文） | ✔ |
| prefill | ≥ 100 tok/s | 131–147 tok/s（短增量）；130–140 tok/s（场景首个任务约 1K token）；135–140 tok/s（场景 8 的 2.5K token 冷 prompt）；102 tok/s（2.2K→4.3K 上下文） | ✔ |
| engine 常驻内存（8K） | v0.4：≤ 2.3 GB；v0.6 修正为约 2.9 GB（≤ 3.0 GB） | MVP：3.2–3.8 GB（RSS 峰值，8K 时 4,150 MiB）。内存优化后：8K 时 2,737–2,751 MiB（2.69 GiB），场景中 2,342–2,520 MiB | ✔（v0.6）见 §5.3 |
| 启动到提示符（不含用户 rc） | ≤ 50 ms | 中位数 8 ms（5–11 ms）；加载 Ubuntu 默认 `~/.bashrc` 时约 60 ms | ✔ |
| `nosh -c` 相对 bash 的额外开销 | ≤ 10 ms | `nosh -c true` 3.0 ms/次，`bash -c true` 5.1 ms/次（§2.4 之后另测：3.2–3.4 ms/次，`bash -c true` 4.1–4.2 ms/次） | ✔ |
| 命令不存在 → 本地拼写建议 | ≤ 50 ms | 4 ms；WSL 默认 PATH（42 项中 32 项在 `/mnt/c`，经 9p 访问）下为 79 ms | ✔（WSL 下 ✘） |
| `#` 任务首 token（engine 已加载、同一对话） | ≤ 0.8 s | 0.48–0.95 s，中位数 0.75 s | ✔ |
| `#` 任务首 token（engine 冷启动） | ≤ 3 s（依赖磁盘前缀缓存） | MVP：6.2–7.0 s；内存优化后：4.7–5.5 s（另加模型加载约 2 s） | ✘ 需 M2 |

说明：

- 冷启动的首 token 包括两部分：约 1K token 的静态前缀（system prompt 和工具定义）的 prefill，以及进程内第一次前向时 candle 对权重做的一次性 x86 重排（MVP 中约 2.5 s，短 prompt 首次 TTFT 2.8–3.1 s 主要就是这部分）。内存优化后，Q4K 的重排挪到加载阶段并按层并行（约 0.5–0.9 s），首次前向只剩 Q6K 的 prefill 重排，短 prompt 首次 TTFT 降到 0.72–0.86 s（§5.3）。M2 的磁盘前缀缓存和常驻 engine 可以消除剩下的部分。
- 模型加载 2.2–3.1 s（页缓存已热）。

### 5.2 注意力内核优化（T7 中完成）

试跑场景 8（2.5K token 的 prompt）时，首 token 要 35 s。profile 显示长上下文时时间主要花在注意力上：512 token 的块在 1.5K 位置每层 131 ms，只有 57 GFLOP/s。原因是按"每个 token × 每个 head"逐行计算，每一行都要重新读一遍 K/V。

改为按"KV head × 一块 query token"划分工作单元，同组 8 个 head、8 个 token 共用一次 K/V 读取；QKᵀ 和 PV 用单线程 `gemm` 在 candle 的 barrier pool 上并行执行，softmax 使用可向量化的 exp；decode 时再按 key 区间切分，最后合并局部 softmax。结果：

| 情况 | 优化前 | 优化后 |
|---|---|---|
| 注意力，512 token 块 @1.5K（每层） | 131 ms | 32 ms |
| 注意力，decode @4K（每层） | 0.94 ms | 0.23 ms |
| 2.1K token prompt 的 prefill | 79 tok/s | 124–140 tok/s |
| 4.2K 上下文的 decode | 10.9 tok/s | 16.3 tok/s |

### 5.3 内存优化（设计 v0.6 §2.3、§16 #12）

**做法**：

1. **加载时预先重排 Q4K，释放原始数据**。candle 在 x86 上把量化权重重排成 16 行一组的 tile，缓存在原始数据旁边，原始数据一直保留。MiniCPM5-2B Q4_K_M 的 252 个 Q4K 层矩阵原始 915 MiB、tile 1,221 MiB，而 Q4K 的矩阵乘法在任何 m 下都只读 tile。给锁定的 candle rev 打了一个补丁（`third_party/candle-core`，说明与 diff 见其中的 `NOSH_PATCH.md`、`nosh.patch`），只新增一个公开 API `QTensor::prepack_x86_and_release_storage()`：
   - 只有当 `repack_x86::select` 在所有 m 下都接受该张量时（x86_64、AVX2/VNNI、2D、n % 16 == 0、k % 256 == 0，且 dtype 的 tile 也服务 m == 1）才构建 tile 并释放原始数据，否则什么都不做（非 x86 或老 CPU 保留原始权重）；
   - 有 AMX 时，Q4K 在 m ≥ 32 时用另一种 tile，一并提前构建；
   - 释放后，`dequantize`、`embedding`、`data`、f16 输入的矩阵乘法等需要原始数据的路径都返回明确的错误。
   
   nosh 只对 Q4K 层矩阵调用它；`token_embd`、`output` 和所有 Q6K 权重不动（Q6K 的 decode 用原始数据，prefill tile 仍然懒加载）。每层的 5 个 Q4K 矩阵并行重排，用时 0.5–0.9 s。
2. **KV 改为 f16**（`half` crate），仍按 1024 token 分段增长。注意力内核把 f16 转成 f32 再做 GEMM：decode 时每个工作单元按 256 个 key 一块转换（F16C，每条指令 8 个值），转换结果留在 L2 里；prefill 时所有 token 块读同一段 key，所以每次调用把用到的范围整体转换一次，放进可复用的 scratch（8K 时 16 MiB）。

**为什么没有自己实现 PackedQ4K**：tile 的 AVX-512 VNNI / AVX-VNNI / AVX2 / AMX 内核和 candle 的线程池、激活量化都在 candle 内部（crate 私有），自己实现等于复制约 600 行 SIMD 内核；补丁约 80 行（含注释），内核仍然跟随上游。代价是要维护一个 vendored 的 candle-core，升级 rev 时要重新打补丁。

**内存**（RSS，单位 MiB，`/proc/self/status`；MVP 与优化后都是 release 构建，同一台机器、同一个脚本）：

| 情况 | MVP | 优化后 |
|---|---|---|
| 加载完成（首次前向之前；MVP 列用 `--no-prepack --kv f32` 测） | 1,606 | 1,910 |
| 短 prompt（上下文约 40 token） | 3,219–3,221 | 2,291–2,292 |
| 2.1K prompt + 128 token | 3,544–3,545 | 2,351–2,436（峰值 2,379–2,441） |
| 4.4K 上下文 | 3,805–3,806（峰值 3,849–3,850） | 2,494–2,622（峰值 2,553–2,622） |
| **8K 上下文**（7,884 token prompt + 81–85 token，ctx 7,966–7,970） | 4,149（峰值 4,150） | 2,692–2,706（**峰值 2,737–2,751，即 2.69 GiB / 2.88 GB**） |
| 场景中（10 个场景各 3 轮，1–3K 上下文，`fastrel`） | 3,450–3,590（峰值 3,650） | 2,342–2,423（峰值 2,520） |

8K 时的构成与 v0.6 的公式吻合：保留的原始权重 567 MiB（token_embd 143 + output 209 + Q6K 215）+ Q4K tile 1,221 MiB + Q6K prefill tile 328 MiB + KV（f16，8 段）336 MiB + prefill scratch 16 MiB + 其余（tokenizer、激活、分配器）约 280 MiB ≈ 2,750 MiB。

**速度**（release，tok/s；MVP 3 次、优化后 2 次，这台 WSL 主机本身有 ±5% 的波动）：

| 情况 | MVP | 优化后 |
|---|---|---|
| decode，短上下文 | 22.6–26.6 | 23.6–25.6 |
| decode，约 2.1K | 19.0–19.8 | 19.5–19.9 |
| decode，约 4.4K | 15.5–16.7 | 17.0–17.7 |
| decode，约 7.9K | 11.9–12.4（80–86 ms/token） | 13.1–13.8（74–82 ms/token） |
| prefill，2.1K 冷 prompt | 125–130 | 136–156 |
| prefill，2.2K→4.3K | 94–99 | 95–104 |
| prefill，7.9K 冷 prompt | 85–89 | 89–92 |
| 场景中 decode / 首个任务 prefill（`fastrel`） | 19.4–23.1 / 130–140 | 19.0–22.5 / 151–170 |

长上下文 decode 变快，是因为 KV 的读取量减半。冷 prompt 的 prefill 变快，是因为 MVP 的首次前向里包含 Q4K 重排。

**冷启动**（页缓存已热，短 prompt，进程启动到第一个 token，两个二进制交替各跑 4 次）：

| | MVP | 优化后 |
|---|---|---|
| 模型加载 | 1.55–1.62 s（第一次 3.02 s） | 1.83–2.10 s（含并行重排 0.5–0.9 s） |
| 首个 token（加载之后） | 2.79–3.09 s | 0.72–0.86 s |
| 合计（wall） | 4.53–4.70 s（第一次 6.29 s） | 2.84–2.96 s |
| 场景中 `#` 首个任务的首 token（约 1K token prompt，`fastrel`） | 6.2–7.0 s | 4.7–5.5 s |

**正确性**：

- tile 路径：`crates/nosh-llm/tests/prepack.rs` 在随机权重上比较 m = 1、2、3、4、5、17、32、33、64：预重排并释放后的输出与懒加载 tile 的输出逐位相同；与 candle 读原始权重的内核（行数不是 16 的倍数时 candle 走这条路）相比，Q4K 的最大相对误差 2.0e-7–3.7e-7。另外检查了释放后 `dequantize`/`data`/`embedding`/f16 输入都报错，Q6K 和不满足条件的形状不释放。
- KV f16 与 f32：真实模型、3,329 token 的 prompt 加 48 步 decode（teacher forcing，用文档本身的后续 token），共 49 个位置：

  | 对比 | logits 余弦（prompt 末尾 / 中位数 / 最小） | 平均 KL（nats） | 真实后续 token 的 NLL | top-5 集合一致 | 高置信（p > 0.5）top-1 一致 |
  |---|---|---|---|---|---|
  | f16 KV | 0.99774 / 0.99824 / 0.99606 | 0.0106 | 2.7680 → 2.7502 | 38/49（顺序一致 20/49） | 30/30 |
  | 对照：f32 KV 取整到 22 位尾数（1 ULP） | 0.99822 / 0.99818 / 0.99623 | 0.0110 | → 2.7514 | 39/49 | 30/30 |
  | 对照：取整到 14 位尾数 | 0.99819 / 0.99829 / 0.99636 | 0.0076 | → 2.7568 | 35/49 | 30/30 |
  | 对照：取整到 10 位尾数（与 f16 相同的精度，没有指数范围限制） | 0.99811 / 0.99833 / 0.99600 | 0.0094 | → 2.7596 | 39/49 | 30/30 |

  原计划的验收条件是"top-5 一致、余弦 > 0.999"，这在本流水线里对任何改变 KV 数值的做法都达不到：每个量化矩阵乘法都会把激活重新量化到 8 位，哪怕 1 ULP 的差异也会翻转一部分舍入并逐层扩散，余弦就稳定在 0.998 左右（对照组与 f16 没有区别；只改 prefill 分块则逐位相同，说明计算本身是确定的）。对照组用临时加入的取整代码测得，没有提交。因此 `f16_kv_matches_f32_kv` 改为检查 f16 不应改变的东西：高置信的 top-1 全部一致、平均 KL < 0.03、NLL 变化 < 0.05、余弦中位数和 prompt 末尾 > 0.995、top-5 集合一致的比例 ≥ 60%。
- 场景：内存优化后的构建再跑 3 轮 10 个场景，26 次完全正确、4 次小错、0 次失败（§3），回答质量与之前的构建在同一波动范围内。

**调优记录**：第一版 f16 decode 用 `half` 的切片转换（每次调用 4 个值），并把整个 key 区间（8K 时 1 MB）一次转换出来，超出 L2，注意力内核慢 40–68%，7.9K 上下文的 decode 从 85 变成 88–90 ms/token；改为 F16C 转换、按 256 个 key 分块后，内核与 f32 持平，端到端反而更快。

### 5.4 ARM 重排后释放（issue #9）

**实现**：通用 API `QTensor::prepack_and_release_storage()` 在 aarch64 + dotprod 上支持二维 Q4K/Q6K、n%8==0、k%256==0。完整四行块继续走原有内核，尾部 1–3 行逐行 GEMV；预建好同一份缓存再释放原始数据。nosh 对层矩阵和 output 调用，token_embd 不动；x86 的释放条件、Q6K decode、output 和 AMX 策略不变。旧 x86 API 保留兼容入口。`--no-prepack` 保留原始数据，但**仍允许懒重排**，不是禁用重排内核。

**合成正确性**：[CI run 36101297488](https://github.com/NewFuture/nosh/actions/runs/36101297488)（`2ce9ddd`）三平台均通过。Q4K/Q6K 各覆盖 204 个形状/批大小组合，包括每个 m=1..64、m=0、511/512/513，k=256/512，以及 ARM 合格而 x86 不合格的行数；对照采用 47/39 行，确保真正走原始内核。Linux ARM（dotprod + i8mm）最大相对误差分别为 2.3e-7、0；macOS（dotprod、无 i8mm）为 2.9e-7、8.5e-8。还验证了 f32/bf16、非零输入偏移、融合 GEMV、释放后原始访问报错、加载器 output/embedding 边界及无独立 output 的情况。

**真实模型**：[CI run 36102190771](https://github.com/NewFuture/nosh/actions/runs/36102190771)，源码 `9737774`，Rust 1.98.1 release，MiniCPM5-2B Q4_K_M（SHA-256 `ec2d5801640099e97d8d7e8003ad4d81f336e757811f03a26173dddf386602fd`）。runner 是 `ubuntu-24.04-arm` / Neoverse-N2，报告 4 个 CPU，但推理固定 `CANDLE_NUM_THREADS=2`、`RAYON_NUM_THREADS=1`；KV f16、seed 42、temperature 0。

8,065 token prompt（尾块 385 行）+ 64 token 生成，最终上下文 8,131/8,192。开/关预重排在两个独立进程中串行运行，GNU time 测完整进程生命周期，而不是只设置 `--ctx 8192`：

| 指标 | `--no-prepack` | 默认预重排 |
|---|---|---|
| GNU time 峰值 RSS | 3,576,060 KiB（3.41 GiB） | **2,153,388 KiB（2.05 GiB）** |
| CLI VmHWM | 3,492 MiB | 2,103 MiB |
| 释放矩阵/原始字节 | 0 / 0 | 295 / 1,405,071,360（约 1,340 MiB） |
| 生成输出 | 两份文本完全相同 | 两份文本完全相同 |

峰值减少约 39.8%，通过 2.5 GiB（2,621,440 KiB）门槛。CLI 历史日志的 `MB` 标签实际按 MiB 计算；此处以 GNU time 的 KiB 为精确口径。不以这个受限线程数的 runner 作 ARM 速度验收，也不把 Linux RSS 数字当成 macOS 测量。

**数值验收**：相同 f16 KV 下比较预重排开/关，两份模型顺序加载；使用已有真实文档样本，并增加长上下文。通过线不变，使用设计 §13.2 的全部指标，而非仅对比生成文字：

| prompt + teacher forcing | 高置信 top-1 一致 | 平均 KL | 平均 NLL（保留 / 释放） | 余弦中位数 | top-5 集合一致 |
|---|---|---|---|---|---|
| 3,329 + 48 token | 30/30 | 0 | 2.736771 / 2.736771 | 1 | 49/49 |
| 8,065 + 48 token | 48/48 | 0 | 0.527555 / 0.527555 | 1 | 49/49 |

prompt 末位置的余弦分别为 1 和 0.9999999999999998，全部指标通过。`arm64-memory-*` artifact 包含输入、两份 CLI 输出、内存/数值 JSON、CPU 特性和二进制校验值；独立 `.github/workflows/arm64-memory.yml` 只在手动触发时下载并缓存模型，普通 PR 不跑真实模型。首次验证借用了已注册 CI 的手动入口；临时调用接线已移除，最终仅保留独立工作流。

**x86 回归对照**：基线 `cd7a106` 与修改后 `d9c255a`，同一 WSL Ubuntu / Xeon Platinum 8370C（8 核 16 线程），Rust 1.98.1 release，8 推理线程、rayon 1 线程，同一个校验通过的模型，KV f16、seed 42、temperature 0。输入为 `MVP-PLAN.md` 前 7,000 个字符加固定摘要请求，实际 3,384 token prompt + 64 token 生成。在确认其他模型评测退出后串行交替跑三对，窗口为 2026-09-25 07:26:12–07:29:33 UTC（含预热）；先前可能受同机评测影响的样本不计入结果：

| 中位数（每个构建三次） | 基线 | 修改后 |
|---|---|---|
| 加载 | 2.49 s | 2.31 s |
| prefill | 130.9 tok/s | 133.0 tok/s |
| decode | 17.9 tok/s | 18.3 tok/s |
| 峰值 RSS | 2,563,496 KiB | 2,559,520 KiB |

三对生成文本全部相同，仍仅释放 252 个层内 Q4K、约 915 MiB；此组对照未见性能回退，微小速度差异不作为加速结论。x86 原有内核和释放条件未改变，ARM CI 则没有速度门禁。

## 6. 已知问题

1. **内存**：§5.3 的 x86 8K 实测仍为 2.69 GiB，达到 v0.6 的目标（≤ 3.0 GB），但达不到 v0.4 的 2.3 GB。ARM + dotprod 已通过 #9 释放 Q4K/Q6K 原始数据，Linux ARM 实测 2.05 GiB（§5.4）；macOS 已验证正确性但未测 RSS。无 dotprod/其他架构没有这次实测，不能外推。AMX 未在真机验证；Q8_0 模型也未启用 nosh 层面的预重排释放。
2. **冷启动首 token 4.7–5.5 s**（MVP 6–7 s）：没有磁盘前缀缓存，每个新进程都要重新 prefill 约 1K token 的静态前缀；Q4K 重排已挪到加载阶段（加载多 0.3–0.5 s），首次前向仍有 Q6K prefill 的懒加载重排。
3. **f16 KV 的 logits 与 f32 KV 余弦约 0.998**：低于原计划的 0.999，但 1 ULP 的扰动也是这个水平（§5.3），NLL、KL 与高置信预测不受影响。
4. **2B 模型的可靠性**：偶尔写错命令（排序字段、`sort -h -n` 混用）、算错总数、先给出错误的中间结果、自相矛盾；偶尔声称做了实际没做的事（场景 10 说已切换目录），或者在该发起命令时反问用户（场景 4）；偶尔模仿任务头的格式输出 `[task …]` 之类的行。倾向于用 `list_dir`/`read_file` 逐个查看，而不是一条 `find`/`wc` 命令，因此步数偏多。temperature 1.0（官方推荐的设计默认值）放大了结果的波动：四个构建各跑 3 轮，完全正确的次数分别是 27、20、25、26。
5. **brush 后台作业**：`cmd &` 显示 `[1]+ <pid unknown>`，`kill %1` 失败。brush-core 0.5 把异步命令当作任务运行，没有 pid；需要向上游修复。同样因为是任务，进程可能在命令返回之后才创建：用 `&` 启动作业后几毫秒内就开始的 agent 命令，会把这个作业当作自己启动的进程（交互中不会出现这种时序，§2.5 #6）。
6. **中断 agent 命令时丢弃 brush 的 future**：超时或 Ctrl-C 时向本次命令新增的进程发送 SIGTERM/SIGINT（2 s 后 SIGKILL），包括 double-fork 或 `setsid` 后脱离了进程树、但环境里仍带有本次 `NOSH_AGENT_RUN` 的进程（Linux 从 `/proc` 读取，macOS 用 libproc 和 `sysctl(KERN_PROCARGS2)`，§2.5；其他 Unix 平台不做这种清理）；清空了环境又脱离进程树的进程（如 `env -i setsid …`）找不到。放弃整条命令行并恢复变量作用域深度，行为与 bash 中按 Ctrl-C 一致。如果当时正处在 shell 函数内部，brush 的调用栈帧（`FUNCNAME` 等）可能残留（很少见，需要上游提供取消接口）。
7. **nosh 进程组内的子进程**：brush 在 nosh 自己的进程组中运行 `$(…)` 和 builtin 之后的管道阶段。agent 命令超时或中断时这些进程会被逐个清理，但它们可以直接读写终端，不会触发 SIGTTIN 识别（例如 `$(ssh host …)` 会直接在终端上询问密码，而不是被识别为需要终端的命令并交给 `propose_command`）。
8. **提示符下 Ctrl-C 的覆盖范围**：循环体里运行外部命令时（`while true; do sleep 1; done`），Ctrl-C 只结束当前子进程，brush 会继续循环；只含 `[[ ]]`/`(( ))` 而没有其他命令的循环无法中断；`read` 内建命令等待输入时不响应 Ctrl-C。这些都需要 brush 上游支持中断。agent 命令不受影响（超时和 Ctrl-C 会放弃整条命令行）。
9. **WSL 特有**：PATH 中的 `/mnt/c` 目录经 9p 访问很慢，"命令不存在"的判定约 79 ms；首次列 PATH 约 0.4 s（在后台预热）。
10. **Ctrl+G 建议使用独立会话**：`LocalChatEngine` 只维护一份 KV，切换会话时从最长公共前缀开始重算，所以 Ctrl+G 之后的下一个 `#` 任务要重新 prefill 主对话。
11. **风险分级的取舍**（方便优先，§16 #14）：工作区内运行时才确定的写入目标（`for f in *.txt; do mv …`、`find . -exec cp {} {}.bak`）评为 Mutating，删除和可能离开工作区的目标仍为 Dangerous。效果未知的命令（规则表之外的命令、本地程序、其他解释器执行的脚本或内联代码）和分析不了的脚本（二进制、超过 256 KiB、无法解析）按 Mutating 处理，auto 模式下在工作区内自动执行；`make`、`cargo build/run`、`npm run` 这类构建和运行命令同样会执行项目中的代码。受保护路径的读取只在路径是字面量或分析时已知的变量值、参数时才要求确认，运行时才能确定的值（`$(…)`、glob、未知变量）不要求。反过来，用变量拼出的写入目标（`OUT=build/x; echo > "$OUT"`）仍按运行时计算的路径评为 Dangerous。
12. **审批卡片出现时会丢弃预输入**：为防止之前缓冲的按键误答审批，出现卡片时清空终端输入缓冲；设计文档里写的是"agent 运行期间用户的输入先缓冲"。
13. **`list_dir` 只检查起始目录**：`list_dir ~/.ssh` 需要审批，但 `list_dir ~`（depth ≥ 2）会列出 `~/.ssh` 里的文件名和大小（不含内容）。
14. **尚未实现（MVP 范围外或加分项）**：后台下载、多源并行下载、`ai history/private/undo/model`、`thinking = "auto"`、`engine.*` 配置（MVP 在进程内推理，这些键会被接受但忽略）。
15. **vendored candle-core**：`third_party/candle-core` 是锁定 rev 的副本加补丁；升级 candle 时要按 `NOSH_PATCH.md` 重新打补丁、重新生成 manifest。上游提供释放原始数据的选项后即可删除。
16. **`nosh model import` 的理论竞态**：先对源文件算哈希，再硬链接或复制进模型库并记录 stamp，中间不复查；如果源文件恰好在这段时间内被改写，库里的文件会被当作已校验。目前只是理论问题，暂不处理（补上需要复制后再算一遍哈希）。
17. **平台差异**（§2.5、§5.4）：aarch64 的权重释放已覆盖 dotprod CPU；无 dotprod 的策略不变。macOS 上 `nosh doctor` 读不到内存（读的是 `/proc/meminfo`），显示为 unknown；CPU 型号在 macOS 和 aarch64 Linux 上只显示架构名（`/proc/cpuinfo` 里没有 `model name`）；`nosh debug gen` 的 RSS（读 `/proc/self/status`）在 macOS 上没有；`configure_thread_env` 检查"还没有其他线程"只在 Linux 的 debug 构建里做。这些不影响功能，暂不处理。

## 7. 偏离设计之处与决策

| 事项 | 设计 | 实现 | 原因 |
|---|---|---|---|
| candle 版本 | `candle-core` 0.11 | 固定 git rev `9b1be4a`（main）；candle-core 以 vendored 副本 + 补丁的形式放在 `third_party/candle-core`，通过 `[patch]` 替换（candle-nn 仍来自同一 rev） | 0.11.0 只有编译期 AVX2，并且依赖带 onig（C 库）的 tokenizers 0.22；main 支持运行时 AVX2/AVX-512 VNNI 分派和 x86 重排内核，Q4K GEMV 快 2.3 倍。补丁见 §5.3：上游没有在重排后释放原始数据的方法 |
| 推理线程 | — | `RAYON_NUM_THREADS=1`、`CANDLE_NUM_THREADS=物理核心数`，在 `main` 开头、任何线程启动之前设置，只作用于 nosh 进程，在 shell 子进程中还原 | rayon 线程池与 candle 的 barrier pool 争抢核心，decode 只有 6.5 tok/s；调整后达到 20 tok/s。candle 只从环境变量读取这两个值，而修改环境变量必须在单线程时进行（§2.4） |
| 注意力 | candle 算子 | 自有分块 GQA 内核（直接调用 candle 已依赖的 `gemm` crate），decode 按 key 切分 | 见 §5.2 |
| KV | 预先分配 8K（v0.6：默认 f16） | f16（`half`），按 1024 token 增长；decode 按 256 个 key 分块转成 f32，prefill 每次调用整段转换到可复用的 scratch；`LoadOptions`/`nosh debug gen --kv f32` 可切回 f32 | 降低短对话的内存占用；转换方式见 §5.3 |
| 预重排权重 | v0.6：加载时提前重排 Q4K 并释放原始数据 | x86 保持层内 Q4K；ARM + dotprod 增加层内 Q6K 及 output。按层并行，只在 tile 服务所有 m 时释放，embedding 不动；`--no-prepack` 可关闭 | 把重排移到加载阶段，按平台释放原始副本；内存结果见 §5.3、§5.4 |
| f16 KV 的验收 | top-5 一致、logits 余弦 > 0.999 | 检查高置信 top-1 一致、KL、NLL、余弦中位数 > 0.995、top-5 集合重合 | 任何 KV 改动（包括 1 ULP）都使余弦停在约 0.998（§5.3）。验收标准的调整已经用户确认（设计 v0.6 §13.2、决策 §16 #13，见 PR #2），保留 KV f16 |
| 下载 | 单连接流式 | 64 MiB 分块 Range 请求，每块有超时 | 便于检测卡顿、从断点换源 |
| 文件锁 | fs4 | std `File::try_lock`（fs4 只用来查询磁盘空间） | MSRV 1.89 |
| 选源 | 地区 + 测速 | 地区只根据 locale/时区推断；吞吐从收到第一个字节开始计时 | 排除 TLS 握手和重定向的影响 |
| 命令历史 | reedline | reedline 接 brush 的历史；`HISTFILE` 默认为 `<data>/state/shell_history` | `history` 内建命令与编辑器共用同一份历史 |
| `-c`/脚本 | 纯 bash | 非登录时不加载任何 rc | brush 在非交互模式下遇到 `BASH_ENV` 会报"未实现"，会产生额外输出 |
| 会话状态保护 | 禁止 exit/logout/exec | 权限分析里把它们定为 Forbidden；修改 PATH、`trap`、别名、函数等属于 Mutating 且 `changes_session` | 与审批矩阵统一处理 |
| agent 超时 | 中断 | 中断整条命令行（超时发 SIGTERM，Ctrl-C 发 SIGINT，2 s 后 SIGKILL），不继续执行后续命令；按环境变量 `NOSH_AGENT_RUN` 同时找到脱离了进程树的进程 | brush 没有取消接口；与 bash 按 Ctrl-C 的行为一致 |
| `ai` 内建 | 内建命令 | 用户没有同名命令（别名/函数/PATH）时才生效 | 避免遮蔽用户自己的 `ai` |
| `nosh -a` 输出 | — | 回答写 stdout，工具卡片和统计写 stderr；stdout 不是 TTY 时不加 `┃` 前缀 | 便于重定向和管道 |
| 引擎 | 默认共享 engine 进程 | 进程内推理（相当于 `engine.shared = false`） | MVP 范围 |
| 统计输出 | `xtask bench` | `NOSH_STATS=1`、`nosh -a --json` 的 `done` 事件、`nosh debug gen` | 满足 T7 测量需要 |
| 任务头 | `[task trigger=… cwd=… venv=… git=… time=…]` | 输入或失败的命令含中文时追加 ` lang=zh` | 2B 模型偶尔用英文回答中文问题；只影响任务消息，system 保持不变 |
| `list_dir` 输出 | 遵循 .gitignore，深度 ≤ 3 | 树形列表；同一次列表中所有文件大小使用最大文件的单位 | 模型会把 "781.2 KB" 排在 "11.4 MB" 前面（§3 调优记录） |
| 用户 allow/deny 规则 | glob 规则；allow 不能覆盖 Forbidden | 逐条匹配简单命令：allow 要求行内每一条简单命令都匹配（可以放行 Dangerous），deny 匹配任一简单命令或包装之后的命令；含隐藏字符的命令不适用 allow | 设计没有规定匹配的粒度；按整行匹配时 `ls; rm -rf x` 能借 `ls*` 规则放行 |
| 隐藏字符 | — | 含控制字符、双向控制符、零宽字符的命令评为 Dangerous，并在卡片上显示为转义 | 防止审批卡片显示的内容与实际执行的不一致 |
| 受保护路径的读取 | 读取需要确认 | 字面路径，以及分析时已知的变量值、函数和脚本参数；无法确定的值不要求确认；已知值不用于降低写入目标的分级 | 方便优先（§16 #14）；流不敏感的分析在分支或循环之后可能拿到过时的值，只能用来增加确认，不能用来放行 |
| builtin 中断 | — | 交互和 agent 会话中每个 builtin 执行前让出一次调度 | brush 在只含 builtin 的循环里从不让出，超时和 Ctrl-C 无法生效；代价约 1 µs/次，`-c` 和脚本不受影响 |

## 8. 对 M2 的建议

1. **常驻 engine + 磁盘前缀缓存**：把静态前缀的 KV 落盘（f16 约 50 MB），冷启动首 token 可以从 4.7–5.5 s 降到 1 s 以内；常驻进程还能省掉每次约 2 s 的加载（含 Q4K 重排）和首次前向的 Q6K 重排。
2. **内存的后续工作**：Q4K 原始数据释放和 KV f16 已完成（§5.3，8K 时 2.69 GiB）；向 candle 上游提议"重排后释放原始数据"的选项，以便删除 vendored 副本；`q8_0` 模型的 Q8_0 矩阵也可以同样释放（约 2 GB）；若要再低，需要更紧凑的重排格式或不重排的低内存档（v0.6 §16 #12 暂不做）。
3. **提高 2B 模型的可靠性**：约束解码（工具名、参数名）；工具输出按小模型的特点设计（`list_dir` 统一单位对场景 1 有明显效果，而在工具描述里加提示没有效果）；agent 模式试用较低的 temperature（例如 0.6–0.7），与官方推荐值 1.0 对比成功率；建立评测集（大文件、端口、行数、改名、`cd` 等）并固定 seed 做回归，每个场景至少跑 10 次，3 次的样本不足以比较不同构建。
4. **多会话 KV**：按会话保留 KV（或至少为建议模式单独保留一份），避免 Ctrl+G 冲掉主对话的缓存。
5. **brush 上游**：异步作业的 pid 与 `kill %n`；可取消的执行接口（agent 超时时不必丢弃 future）；子进程统一放入独立进程组（`$(…)`、builtin 之后的管道阶段）；子进程因 SIGINT 退出时中止循环（与 bash 一致）；`read` 等内建命令响应中断；进程创建钩子（为 Landlock/seccomp 沙箱做准备）。
6. **体验**：模型后台下载与提示符上的进度；审批卡片显示风险原因的中文说明；`ai out` 折叠视图；WSL 下对 `/mnt/*` PATH 做缓存以加快命令存在性判断。
