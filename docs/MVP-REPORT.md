# nosh MVP 报告

> 对应 `docs/MVP-PLAN.md`（T0–T7）与 `docs/DESIGN.md` v0.4。本报告记录实现范围、偏离设计之处、10 个端到端场景的结果、性能数据、已知问题和对 M2 的建议。

## 1. 结论

- MVP 范围内的 9 项能力全部实现，验收标准 7 条全部满足（逐条结果见 §4），只有两项性能指标未达标：常驻内存（3.5 GB，目标 ≤ 2.3 GB）和冷启动首 token 延迟（6–7 s，目标 ≤ 3 s，依赖 M2 的磁盘前缀缓存）。
- 10 个真实模型场景用最终构建各跑 3 次（共 30 次）：25 次完全正确，3 次结论正确但回答里有小错（总数算错、先给出错误的中间表格、措辞），2 次失败（模型声称已经切换目录，实际没有执行 `cd`；列出改名计划后反问"是否继续"，没有执行）。temperature 1.0 下模型波动明显：两个较早构建上的三轮结果分别是 27/2/1 和 20/7/3（完全正确/有小错/失败，见 §3）。
- 代码审查发现的 11 个缺陷和 4 个小问题已全部修复（§2.1），涉及审批规则、符号链接、agent 命令的超时与中断、隐藏字符和下载取消等。
- 性能（WSL2，Xeon 8370C 8 核，Q4_K_M，release 构建）：decode 19–25 tok/s，prefill 102–147 tok/s，热对话首 token 0.5–1.0 s，无 rc 启动到提示符约 8 ms。

## 2. 实现摘要

| crate | 内容 |
|---|---|
| `nosh-hub` | 内置 registry（2B Q4_K_M/Q8_0、1B Q4_K_M + tokenizer，固定 revision 和 SHA-256）；按地区（locale/时区）+ 并行 HEAD/2 MB 测速选源（HF / hf-mirror / ModelScope）；64 MiB 分块 Range 下载、`.partial` 断点续传、文件锁、磁盘空间检查、失败换源与退避、边下边算 SHA-256、原子 rename；离线开关（`--offline`、`NOSH_OFFLINE`、`HF_HUB_OFFLINE`，全部网络访问经过唯一出口 `net`）；`nosh model pull/list/verify/import/path`。 |
| `nosh-llm` | fork 自 candle `quantized_llama` 的 MiniCPM5 模型（量化 embedding、RoPE 表、自有 KV 与注意力内核）；tokenizer（分段编码，不可信片段不解析 special token）；手写 MiniCPM5 模板（与 HF `apply_chat_template` 逐字节一致的 golden 测试）；采样（temperature/top-p/min-p、复读检测后启用 1.05 惩罚、`<function` 内降温到 0.3、`--seed`）；按 token ID 驱动的流式工具调用解析（CDATA、实体、按 schema 转类型）；`LocalChatEngine`（token 级对话日志 + 最长公共前缀复用 KV、分块 prefill、可取消）和 `MockChatEngine`；`nosh debug gen`。 |
| `nosh-permissions` | 基于 brush-parser 的 AST 分析：管道、列表、子 shell、`$(…)`、进程替换、重定向、函数定义、fork 炸弹；展开 `sudo`/`doas`/`env`/`timeout`/`nice`/`xargs`/`find -exec`/`bash -c`/`eval`/`watch` 等包装器和会话里的别名、函数；`$'\x..'` 混淆、动态命令名；规则表（git/docker/kubectl/systemctl/包管理器/网络工具等）；路径分级（受保护路径、工作区、临时目录、系统目录）；confirm/auto/yolo 决策矩阵、用户 allow/deny、"本会话同类放行"。247 条表驱动用例，Dangerous 召回率 100%。 |
| `nosh-shell` | 嵌入 brush-core：交互/登录/`-c`/脚本/stdin 模式，rc 与 profile 加载；reedline REPL（brush 历史桥接、补全、续行校验、hinter、PS1 或 `cwd ❯` 提示符 + 右侧审批模式/YOLO 标记、Ctrl-C/Ctrl-D、Ctrl+G）；输入流水线（`#`、解析失败与单词内撇号判定、整行静态命令名检查、本地拼写纠错、破坏性命令安全网、失败提示与含中文时自动交给 AI、`ai` 内建命令）；`run_agent_command`（同一 `Shell`、stdin 为 `/dev/null`、管道采集 10 MB 上限、后台进程组、防卡住环境变量只作用于单次执行、超时与 Ctrl-C、SIGTTIN 识别、状态差异）。 |
| `nosh-core` | 静态 system prompt + `[task …]`/`[recent]` 任务头（含 NOSH.md）；工具 `run_command`/`read_file`/`list_dir`/`propose_command`；任务循环（错误回灌同类最多 2 次、拒绝理由、步数上限后要求总结、上下文 85% 时压缩旧工具输出、Ctrl-C 取消/中止）；输出截断（头 60% + 尾 40%，6,000 字符，完整输出脱敏后存入 `state/outputs/`）；终端审批卡片（y/n/e/a，Dangerous 键入 `yes`，Ctrl-C 拒绝，无 TTY 拒绝并把命令写到 stderr）；终端渲染（`┃` 块、8 行实时输出区、`ai out <n>`）与 JSON Lines；REPL 处理器（懒加载模型、`ai mode/think/clear/ctx/status/out`、Ctrl+G 建议）。 |
| `nosh-cli` | `nosh`、`-c`、脚本、`-a`（管道附件，只读工具）、`-s`、`doctor`、`model`、`debug`；`--auto/--yolo/--offline/--model-path/--model/--no-download/--norc/--safe/--seed/--json`，`-l/-i/-e/-x/-u`；首次启动下载确认（默认 Y，前台下载）；`config.toml`（§11 常用项，未知项警告）；登录 shell 的 REPL panic 时 exec 回退 shell。 |

测试：`cargo test --workspace` 共 134 个测试（权限的 247 条用例按表驱动放在少数几个测试函数里），其中 `tests/agent_flow.rs` 用 MockChatEngine 覆盖多步任务、审批与拒绝理由、Dangerous 强确认与编辑后重新评估、`exec`/`exit` 拦截、错误回灌与放弃、截断与落盘、步数上限、无 TTY、`propose_command`、只读工具与受保护路径（含 `..` 和符号链接）、超时，以及 REPL + agent 联动（`#`、`gti status`、agent `cd` 后用户 `pwd`、中文 not_found）；`crates/nosh-shell/tests/shell.rs` 覆盖快速输出下的超时、`$(…)` 与管道中进程的清理、只含 builtin 的循环超时、作用域不泄漏、后台作业存活和提示符下 Ctrl-C。另有 5 个 `#[ignore]` 测试：3 个需要真实模型（本地已通过），1 个注意力基准，1 个权限诊断输出。CI（ubuntu-latest：fmt、clippy -D warnings、test）每次提交都是绿色。

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

小问题：被中断的 `git status` 会留下 `index.lock`（任务头改用 `git --no-optional-locks`）；连按两次 Ctrl-C 中止任务改为按单条命令计数；脱敏改为整行扫描，并覆盖更多 `key=value`、`"key": "value"`、`Authorization: Bearer` 形式；`read_file` 在 `end_line < start_line` 时返回错误，不再下溢。

## 3. 端到端场景（真实模型）

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
4. **批量重命名**：成功的两轮生成 `for f in *.txt; do mv "$f" "${f%.txt}.md"; done`，因为写入目标是运行时计算的路径被评为 **DANGEROUS**，审批卡片要求键入 `yes`；执行后 4 个文件改名，`readme.md` 保持不变。**失败的第 3 轮**：模型列出目录后给出改名计划，然后反问"Would you like me to proceed?"，没有发起命令（审批卡片本身就是确认环节）；回答开头还模仿任务头写了一行 `[task complete=hash …]`。
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

`b60e727` 相对 `3e66f8b` 在这些英文场景里几乎没有改变模型看到的内容（脱敏规则对这些输出不起作用，只有工具路径的写法可能不同），差异主要来自 temperature 1.0 下的采样波动；3 轮样本太少，不能据此比较构建之间的优劣。

**调优记录**：

- 场景 3 首次试跑失败：`list_dir` 把大小显示为 `86B`，模型把它当成了行数。改为 `(86 bytes)`，并在工具描述中写明"只列名称和大小，计数与搜索用 run_command"之后，3 轮都正确。
- 场景 10 最初的措辞是"go into the data directory and list what is there"，有一轮模型直接对 `data` 调用 `list_dir`，没有 `cd`，测不到状态延续。改用明确的"change the current directory to data"。
- 试跑中长 prompt 的 prefill 明显变慢，定位到注意力内核（见 §5.2）。
- 场景 5 有一轮用英文回答中文问题。输入或失败的命令里含中文时，任务头加上 ` lang=zh`。
- 场景 1 的主要失败方式是：`list_dir data` 同时列出 `dump.bin (20.0 MB)` 和 `small.bin (781.2 KB)`，模型把 781.2 KB 排在 11.4 MB 前面。先试了在工具描述里加"计数、排序、搜索用 run_command 配合 wc/du/sort/find/grep"：单独运行场景 1 时，基线与改动都是 5/6 正确；在会话 A 的上下文中，基线 3/5，改动 2/5，没有改善，已撤回。随后改为同一次列表内的大小统一使用最大文件的单位（`small.bin (0.8 MB)`、`m1.py (<0.1 MB)`）：在会话 A 的上下文中跑 6 次，5 次先用 `list_dir` 浏览（3 次直接据此作答，2 次再用 `find` 核对），全部正确；另 1 次一开始就用 `find`，三个文件对了但顺序错了。改动前，没有用命令核对、只根据 `list_dir` 作答的 7 次里只有 1 次正确。

**离线验证**：用 `LD_PRELOAD` 拦截 `socket`/`connect`/`getaddrinfo` 做计数。对照组 `nosh doctor`（联网）记录到 3 次 inet 连接和 6 次 DNS 解析；`nosh --offline -a --auto "how many files are in the data directory?"` 完成任务（退出码 0），期间没有任何 inet socket 和 DNS 解析，只有一次连接本机 `/run/systemd/userdb` 的 Unix socket（glibc NSS 查询用户信息）。另外，在没有网络的 `unshare -rn` 命名空间里执行 `nosh --offline -s …` 也能正常输出命令。以上检查在最终构建上复测，结果相同。

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

8 个推理线程（物理核心数），Q4_K_M，上下文 8K。decode/prefill 的基准数据来自 release 构建（`nosh debug gen`）；场景中的数据和延迟数据来自最终 3 轮使用的 `fastrel` 构建（继承 release，thin LTO、16 个 codegen unit）：

| 指标 | 目标 | 实测 | |
|---|---|---|---|
| decode | ≥ 12 tok/s | 24.6–25.7 tok/s（短上下文）；19.4–23.1 tok/s（场景中，1.1–3.0K 上下文）；16.4 tok/s（4.4K 上下文） | ✔ |
| prefill | ≥ 100 tok/s | 131–147 tok/s（短增量）；130–140 tok/s（场景首个任务约 1K token）；135–140 tok/s（场景 8 的 2.5K token 冷 prompt）；102 tok/s（2.2K→4.3K 上下文） | ✔ |
| engine 常驻内存（8K） | ≤ 2.3 GB | 3.2–3.8 GB（RSS 峰值；最终 3 轮中 3.45–3.65 GB） | ✘ 见 §6 |
| 启动到提示符（不含用户 rc） | ≤ 50 ms | 中位数 8 ms（5–11 ms）；加载 Ubuntu 默认 `~/.bashrc` 时约 60 ms | ✔ |
| `nosh -c` 相对 bash 的额外开销 | ≤ 10 ms | `nosh -c true` 3.0 ms/次，`bash -c true` 5.1 ms/次 | ✔ |
| 命令不存在 → 本地拼写建议 | ≤ 50 ms | 4 ms；WSL 默认 PATH（42 项中 32 项在 `/mnt/c`，经 9p 访问）下为 79 ms | ✔（WSL 下 ✘） |
| `#` 任务首 token（engine 已加载、同一对话） | ≤ 0.8 s | 0.48–0.95 s，中位数 0.75 s | ✔ |
| `#` 任务首 token（engine 冷启动） | ≤ 3 s（依赖磁盘前缀缓存） | 6.2–7.0 s（另加模型加载 2–3 s） | ✘ 需 M2 |

说明：

- 冷启动的首 token 包括两部分：约 1K token 的静态前缀（system prompt 和工具定义）的 prefill，以及进程内第一次前向时 candle 对权重做的一次性 x86 重排（约 2.5 s，短 prompt 首次 TTFT 2.6–3.0 s 主要就是这部分）。M2 的磁盘前缀缓存和常驻 engine 可以消除这两部分。
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

## 6. 已知问题

1. **内存超标**（3.2–3.8 GB，目标 2.3 GB）：candle main 的 x86 重排内核为 Q4K/Q6K 权重另存一份分块布局，原始权重也仍然保留（约多占 1.6 GB）；KV 使用 f32（每 token 86 KB）。
2. **冷启动首 token 6–7 s**：没有磁盘前缀缓存，每个新进程都要重新 prefill 约 1K token 的静态前缀，并做一次性权重重排。
3. **2B 模型的可靠性**：偶尔写错命令（排序字段、`sort -h -n` 混用）、算错总数、先给出错误的中间结果、自相矛盾；偶尔声称做了实际没做的事（场景 10 说已切换目录），或者在该发起命令时反问用户（场景 4）；偶尔模仿任务头的格式输出 `[task …]` 之类的行。倾向于用 `list_dir`/`read_file` 逐个查看，而不是一条 `find`/`wc` 命令，因此步数偏多。temperature 1.0（官方推荐的设计默认值）放大了结果的波动：三个构建各跑 3 轮，完全正确的次数分别是 27、20、25。
4. **brush 后台作业**：`cmd &` 显示 `[1]+ <pid unknown>`，`kill %1` 失败。brush-core 0.5 把异步命令当作任务运行，没有 pid；需要向上游修复。
5. **中断 agent 命令时丢弃 brush 的 future**：超时或 Ctrl-C 时向本次命令新增的进程发送 SIGTERM/SIGINT（2 s 后 SIGKILL），放弃整条命令行并恢复变量作用域深度，行为与 bash 中按 Ctrl-C 一致。如果当时正处在 shell 函数内部，brush 的调用栈帧（`FUNCNAME` 等）可能残留（很少见，需要上游提供取消接口）。如果当时正处在 shell 函数内部，brush 的调用栈帧（`FUNCNAME` 等）可能残留（很少见，需要上游提供取消接口）。
6. **nosh 进程组内的子进程**：brush 在 nosh 自己的进程组中运行 `$(…)` 和 builtin 之后的管道阶段。agent 命令超时或中断时这些进程会被逐个清理，但它们可以直接读写终端，不会触发 SIGTTIN 识别（例如 `$(ssh host …)` 会直接在终端上询问密码，而不是被识别为需要终端的命令并交给 `propose_command`）。
7. **提示符下 Ctrl-C 的覆盖范围**：循环体里运行外部命令时（`while true; do sleep 1; done`），Ctrl-C 只结束当前子进程，brush 会继续循环；只含 `[[ ]]`/`(( ))` 而没有其他命令的循环无法中断；`read` 内建命令等待输入时不响应 Ctrl-C。这些都需要 brush 上游支持中断。agent 命令不受影响（超时和 Ctrl-C 会放弃整条命令行）。
8. **WSL 特有**：PATH 中的 `/mnt/c` 目录经 9p 访问很慢，"命令不存在"的判定约 79 ms；首次列 PATH 约 0.4 s（在后台预热）。
9. **Ctrl+G 建议使用独立会话**：`LocalChatEngine` 只维护一份 KV，切换会话时从最长公共前缀开始重算，所以 Ctrl+G 之后的下一个 `#` 任务要重新 prefill 主对话。
10. **保守的风险分级**：写入目标是运行时计算的路径（`for f in *.txt; do mv …`、`find -exec cp {} {}.bak`）一律评为 Dangerous，需要键入 `yes`。
11. **审批卡片出现时会丢弃预输入**：为防止之前缓冲的按键误答审批，出现卡片时清空终端输入缓冲；设计文档里写的是"agent 运行期间用户的输入先缓冲"。
12. **`list_dir` 只检查起始目录**：`list_dir ~/.ssh` 需要审批，但 `list_dir ~`（depth ≥ 2）会列出 `~/.ssh` 里的文件名和大小（不含内容）。
13. **尚未实现（MVP 范围外或加分项）**：后台下载、多源并行下载、`ai history/private/undo/model`、`thinking = "auto"`、`engine.*` 配置（MVP 在进程内推理，这些键会被接受但忽略）。

## 7. 偏离设计之处与决策

| 事项 | 设计 | 实现 | 原因 |
|---|---|---|---|
| candle 版本 | `candle-core` 0.11 | 固定 git rev `9b1be4a`（main） | 0.11.0 只有编译期 AVX2，并且依赖带 onig（C 库）的 tokenizers 0.22；main 支持运行时 AVX2/AVX-512 VNNI 分派和 x86 重排内核，Q4K GEMV 快 2.3 倍 |
| 推理线程 | — | `RAYON_NUM_THREADS=1`、`CANDLE_NUM_THREADS=物理核心数`，只作用于 nosh 进程，在 shell 子进程中还原 | rayon 线程池与 candle 的 barrier pool 争抢核心，decode 只有 6.5 tok/s；调整后达到 20 tok/s |
| 注意力 | candle 算子 | 自有分块 GQA 内核（直接调用 candle 已依赖的 `gemm` crate），decode 按 key 切分 | 见 §5.2 |
| KV | 预先分配 8K | f32，按 1024 token 增长 | 降低短对话的内存占用 |
| 下载 | 单连接流式 | 64 MiB 分块 Range 请求，每块有超时 | 便于检测卡顿、从断点换源 |
| 文件锁 | fs4 | std `File::try_lock`（fs4 只用来查询磁盘空间） | MSRV 1.89 |
| 选源 | 地区 + 测速 | 地区只根据 locale/时区推断；吞吐从收到第一个字节开始计时 | 排除 TLS 握手和重定向的影响 |
| 命令历史 | reedline | reedline 接 brush 的历史；`HISTFILE` 默认为 `<data>/state/shell_history` | `history` 内建命令与编辑器共用同一份历史 |
| `-c`/脚本 | 纯 bash | 非登录时不加载任何 rc | brush 在非交互模式下遇到 `BASH_ENV` 会报"未实现"，会产生额外输出 |
| 会话状态保护 | 禁止 exit/logout/exec | 权限分析里把它们定为 Forbidden；修改 PATH、`trap`、别名、函数等属于 Mutating 且 `changes_session` | 与审批矩阵统一处理 |
| agent 超时 | 中断 | 中断整条命令行（超时发 SIGTERM，Ctrl-C 发 SIGINT，2 s 后 SIGKILL），不继续执行后续命令 | brush 没有取消接口；与 bash 按 Ctrl-C 的行为一致 |
| `ai` 内建 | 内建命令 | 用户没有同名命令（别名/函数/PATH）时才生效 | 避免遮蔽用户自己的 `ai` |
| `nosh -a` 输出 | — | 回答写 stdout，工具卡片和统计写 stderr；stdout 不是 TTY 时不加 `┃` 前缀 | 便于重定向和管道 |
| 引擎 | 默认共享 engine 进程 | 进程内推理（相当于 `engine.shared = false`） | MVP 范围 |
| 统计输出 | `xtask bench` | `NOSH_STATS=1`、`nosh -a --json` 的 `done` 事件、`nosh debug gen` | 满足 T7 测量需要 |
| 任务头 | `[task trigger=… cwd=… venv=… git=… time=…]` | 输入或失败的命令含中文时追加 ` lang=zh` | 2B 模型偶尔用英文回答中文问题；只影响任务消息，system 保持不变 |
| `list_dir` 输出 | 遵循 .gitignore，深度 ≤ 3 | 树形列表；同一次列表中所有文件大小使用最大文件的单位 | 模型会把 "781.2 KB" 排在 "11.4 MB" 前面（§3 调优记录） |
| 用户 allow/deny 规则 | glob 规则；allow 不能覆盖 Forbidden | 逐条匹配简单命令：allow 要求行内每一条简单命令都匹配（可以放行 Dangerous），deny 匹配任一简单命令或包装之后的命令；含隐藏字符的命令不适用 allow | 设计没有规定匹配的粒度；按整行匹配时 `ls; rm -rf x` 能借 `ls*` 规则放行 |
| 隐藏字符 | — | 含控制字符、双向控制符、零宽字符的命令评为 Dangerous，并在卡片上显示为转义 | 防止审批卡片显示的内容与实际执行的不一致 |
| builtin 中断 | — | 交互和 agent 会话中每个 builtin 执行前让出一次调度 | brush 在只含 builtin 的循环里从不让出，超时和 Ctrl-C 无法生效；代价约 1 µs/次，`-c` 和脚本不受影响 |

## 8. 对 M2 的建议

1. **常驻 engine + 磁盘前缀缓存**：把静态前缀的 KV 落盘（约 50 MB），冷启动首 token 可以从 6–7 s 降到 1 s 以内；常驻进程还能省掉每次 2–3 s 的加载和 2.5 s 的权重重排。
2. **降低内存**：重排后释放原始量化权重（或向 candle 上游提议）；KV 改用 f16 或 q8；这样才有可能达到 2.3 GB。
3. **提高 2B 模型的可靠性**：约束解码（工具名、参数名）；工具输出按小模型的特点设计（`list_dir` 统一单位对场景 1 有明显效果，而在工具描述里加提示没有效果）；agent 模式试用较低的 temperature（例如 0.6–0.7），与官方推荐值 1.0 对比成功率；建立评测集（大文件、端口、行数、改名、`cd` 等）并固定 seed 做回归，每个场景至少跑 10 次，3 次的样本不足以比较不同构建。
4. **多会话 KV**：按会话保留 KV（或至少为建议模式单独保留一份），避免 Ctrl+G 冲掉主对话的缓存。
5. **brush 上游**：异步作业的 pid 与 `kill %n`；可取消的执行接口（agent 超时时不必丢弃 future）；子进程统一放入独立进程组（`$(…)`、builtin 之后的管道阶段）；子进程因 SIGINT 退出时中止循环（与 bash 一致）；`read` 等内建命令响应中断；进程创建钩子（为 Landlock/seccomp 沙箱做准备）。
6. **体验**：模型后台下载与提示符上的进度；审批卡片显示风险原因的中文说明；`ai out` 折叠视图；WSL 下对 `/mnt/*` PATH 做缓存以加快命令存在性判断。
