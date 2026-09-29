# nosh

nosh 是一个用纯 Rust 实现、内置本地小模型（默认 MiniCPM5-2B）、可以断网运行的 AI shell。

> 状态：Linux 上的 MVP 已完成，结果见 [MVP 报告](docs/MVP-REPORT.md)。

- **本身就是 shell**：兼容 Bash（内核为 brush-core）。普通命令直接执行；`#` 显式交给 AI，未知命令先尝试本地纠错，执行失败默认提示求助入口。
- **输入辅助**：交互输入自动高亮，提示待完成语法、命令识别和路径状态；慢查询不阻塞编辑，不改写或执行输入。
- **上下文连续**：agent 和用户共用同一个 shell 会话，cwd、变量、venv 等状态会一直延续。每次任务附上当前项目类型、manifest 基本信息及 Git 状态；切换项目或进入普通目录后重新判断，不沿用上一项目的描述。
- **本地推理**：基于 candle + GGUF。首次使用时自动下载模型，之后可以完全离线。默认模型在 8K 上下文下常驻内存约 2.7 GiB（x86 AVX2/VNNI；详见 [MVP 报告 §5.3](docs/MVP-REPORT.md)）。
- **安全**：agent 发起的命令要经过风险分级和审批。

```bash
nosh                                   # 进入 nosh shell
# 找出当前目录下最大的 10 个文件         # 以 # 开头，交给 AI
nosh -a "把 logs 里 7 天前的日志打包"   # 一次性任务
nosh -s "解压 foo.tar.zst 到 /tmp"     # 只输出命令
```

## 构建与运行（Linux）

```bash
cargo build --release                  # 需要 Rust ≥ 1.89
./target/release/nosh model pull       # 下载并校验模型（约 1.57 GB），之后可离线
./target/release/nosh doctor           # 检查 CPU、内存、模型、下载源
./target/release/nosh                  # 启动 shell；--norc 跳过 ~/.bashrc，--safe 同时关闭 AI
```

常用选项：`--auto` / `--yolo`（审批模式）、`--offline`、`--model-path <gguf>`、`--no-download`。Linux 默认配置为 `~/.config/nosh/config.toml`，平台路径、支持的键和可用示例见 [配置说明](docs/DESIGN.md#11-配置)。`--offline` 阻止模型下载与探测，不限制 shell 命令自身联网。

AI 任务默认显示 **`审批: 自动`**；可用 `ai mode confirm|auto|yolo` 切换。用户 deny 始终优先，有效用户白名单三档免审批，并可覆盖内置禁止；未获白名单覆盖的内置禁止在询问模式须键入 `yes`，自动 / YOLO 直接拒绝。YOLO 对其他操作免逐次审批，不绕过工具范围或外部认证。

自动模式以**便利优先、防御破坏**为目标：普通 `mv` / `cp` 默认执行，危险目标、破坏性效果与用户 deny 仍拦截；不要求原子不覆盖或备份证明。常见构建 / 测试 / 检查也默认执行，明确接受未知项目代码风险，不代表沙箱隔离或可恢复保证。规则采用 TOML 条目，例如 `deny = [{ command_prefix = "docker system prune" }]`，不再使用旧字符串 glob 数组。完整矩阵、作用域及限制见 [审批说明](docs/APPROVAL-MODES.md)。

`ai auto off` 只暂停部分自动路由，不是全局禁用 AI；彻底关闭 nosh AI 可用 `NOSH_DISABLE_AI=1`（保留 rc）或 `--safe`（同时跳过 rc）。具体例外见 [输入判定与开关边界](docs/DESIGN.md#42-ai-触发与输入判定)。

## 实时输入提示与语法高亮

正常交互终端默认启用 `input_assist`，无需插件或模型；`--safe` / `NOSH_DISABLE_AI=1` 不会关闭这项输入辅助。命令、字符串、变量、操作符和注释按结构显示；待完成、查询中和暂不可用等状态会在输入行上方提示。配置的 AI 前缀和 `ai` 入口保留原有语义，不把自然语言正文当作 Bash 脚本诊断。

未识别命令默认不显示长错误文案；停止输入约 1 秒后只用红色删除线标记命令词，`NO_COLOR` / `CLICOLOR=0` 下暂不显示该提示。PATH 中被确认无搜索权限的目录会按快照缓存，并继续查找其他目录；所有候选均缺失或不可执行时，可以判定当前用户没有可执行命令，但不宣称文件不存在。真正的 I/O 故障、未检查完或动态行为才保留未知／暂不可用。详细原因和无颜色 fallback 后续会与补全/状态栏统一展示；当前不会修改系统 PATH 或要求提权。

路径下划线表示当前快照中存在，不代表可读写或已经批准执行。普通参数不存在不报文件错误；`echo hello > new.txt` 允许新目标，`touch input; cat < input` 不把执行前缺失误报为执行必失败。动态命令名、条件定义或无法确定的展开保持未知。

可在用户配置中关闭：

```toml
[shell]
input_assist = false
```

语法分析和文件查询由两个有界辅助进程隔离；超时或超出预算会明确降级，而不是等待查询完成或无限重启。`NO_COLOR` 关闭输入样式；未识别命令的无颜色提示留给后续状态栏/补全统一体验。基本终端和非 TTY 不启用实时输入提示。数据流、判定规则、资源边界、恢复条件及可复现对照见 [实时输入解析设计](docs/INPUT-ASSIST.md)。

## 可选 NVIDIA CUDA 推理

默认设备为 **`auto`**：根据构建能力、可见 GPU 和当前可用显存，在加载时选择 GPU 或 CPU。普通构建仍是 CPU-only，不需要 CUDA；使用 GPU 需在 Linux/WSL 上有 NVIDIA 驱动与 CUDA toolkit（`nvcc`、头文件及 cuBLAS 等运行库），并显式构建 CUDA 版本：

```bash
export PATH=/usr/local/cuda/bin:$PATH
export LD_LIBRARY_PATH=/usr/local/cuda/lib64${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
cargo build --release --locked -p nosh-cli --features cuda
./target/release/nosh --offline doctor
./target/release/nosh debug gen "Explain a shell pipeline." --temp 0
./target/release/nosh -s "列出当前目录"
# 显式覆盖自动选择
CUDA_VISIBLE_DEVICES=0 ./target/release/nosh --device cuda -s "列出当前目录"
./target/release/nosh --device cpu -s "列出当前目录"
```

`--device auto|cpu|cuda|cuda:N` 对交互 shell、Agent、CommandAssist、debug 和 doctor 一致生效，覆盖 `[model] device = "auto"`（默认）。`auto` 通过 CUDA API 检查设备及空闲显存，根据实际 GGUF 权重、配置上下文、prefill 块和 attention/KV 临时张量估算预算，并保留至少 512 MiB 余量；在满足预算的 GPU 中选择空闲显存最多的一张，相同则取较低逻辑序号。没有 CUDA 构建支持、驱动／设备不可用、显存不足或选择 f32 KV 时使用 CPU，并在诊断／引擎状态／原生 trace 中报告原因。它依据显存快照，不是 GPU 利用率调度器，不保证共享 GPU 没有计算负载。

显式 `cpu` 强制 CPU；显式 `cuda`／`cuda:N` 强制 GPU，初始化或加载失败会报错，**不会退回 CPU**。`N` 是 `CUDA_VISIBLE_DEVICES` 筛选后的逻辑序号：只暴露物理卡 1 时仍用 `cuda:0`。自动选择在初始化 CUDA 库后再次核对空闲显存，但不预留显存；之后若其他进程抢占导致加载失败，明确报错，不用宽泛重试掩盖模型损坏、shape 或算子错误。选择只在加载时进行，不迁移正在运行的模型。CUDA 版依赖的动态库必须能被系统加载；若动态链接器在进程启动前报缺库，程序无法回退，应使用 CPU-only 产物。

量化 GGUF 权重、embedding、RoPE、GQA、KV cache 和前馈层均在单张 GPU 上，跳过 CPU prepack；分词、调度和采样留在 CPU。GPU KV 当前**仅开放默认 f16**，支持分块 prefill 和前缀回退，但使用张量拼接，未实现 FlashAttention、多 GPU 或 Metal。`--device cuda --kv f32` 会在权重／分词器加载前明确报错，不会偷偷改用 f16 或 CPU；CPU 的 f32 KV 不变。不同后端的量化内核会有数值差异，固定 seed 不保证 CPU/GPU 输出逐字相同。debug、加载说明与原生评估 trace 会报告实际设备；debug 的 RSS 仅为主机内存，显存和利用率请用 `nvidia-smi` 观测。CUDA 版本产物依赖匹配的 NVIDIA 动态库，默认 CPU 产物不受影响。

f32 限制来自 MiniCPM5-2B Q4_K_M、1212 token prompt＋32 步 teacher forcing 的验收：同为 f32 KV 时，CPU/GPU 的 top-5 完整集合一致率为 18/33，未达到既有 60% 门槛（KL 0.02841、可信 top-1 30/30）；跨 dtype 的 CUDA f32 对 CPU f16 另有 KL 0.04083，超过 0.03。CPU Q8K 与 CUDA Q8_1 激活量化是已发现的不等价因素，**不是已完全确认的根因**。没有放宽阈值或宣称 f32 已验收；默认 f16 的同 dtype 对照通过。

首次使用 CUDA 内核可能触发驱动 PTX/JIT 编译，首 token 明显更慢；后续新进程可能复用驱动缓存。测速必须区分首次 JIT 冷启动、驱动缓存已暖的新进程和同一进程的 KV 复用，不能统称“冷启动”。`NOSH_PROFILE` 的 CUDA 分算子计时仅反映异步提交开销；debug 的整体 prefill/decode 计时会等待 GPU 完成。

为保持历史基线，评估器默认仍显式选择 CPU。GPU 评估传 `python3 -m eval --device cuda ...`；验证自动选择可传 `--device auto`，逐 trial 记录实际设备和选择原因，不依赖宿主配置，见[评估说明](eval/README.md)。

实测源码 `780b952`：Ubuntu、单张 RTX 4090 24 GB、驱动 615.71.09、CUDA 13.4、Rust 1.98.1，MiniCPM5-2B Q4_K_M、f16 KV、seed 42、temperature 0、1457 token 输入／128 token 输出。两次均为新进程、无 KV 命中：

| 驱动缓存状态 | 总墙钟（含加载） | TTFT | Prefill | Decode |
|---|---:|---:|---:|---:|
| 显式空 CUDA 缓存，首次 JIT | 16.153 s | 13.60 s | 107.1 tok/s | 169.5 tok/s |
| 同一驱动缓存已暖 | 2.170 s | 0.19 s | 7490.4 tok/s | 178.7 tok/s |

`nvidia-smi` 捕获到 nosh 的实际 GPU PID，峰值 95% 利用率、2210 MiB 显存。主机推理线程为 4、实际 nice 为 19，另有 CPU 28 线程评估并行运行，**不是无干扰 CPU/GPU 受控对照**。同构建五场景 smoke 原样得到 **2 pass／3 fail／0 error**：本地纠错与 Next 无建议通过；Agent 文件事实／预算及 Generate、Fix 的 finish 字段校验失败，未重抽或改判。四个载模场景均原生记录 `cuda:0`／`F16`；这证明 GPU 入口连通，不代表模型任务质量全通过。

## 命令建议与终端交接

标签化背景用独立 System 消息，真实请求用 User；背景正文按普通文本编码。Available 按能力分组，规则保持简短。项目指引优先加载适用的 `AGENTS.md`；没有 AGENTS.md 时才附 README 首段简介与章节索引，不默认要求读完原文。文档来源相对 cwd 显示，任务开始和工具执行后的目录变化会刷新适用文档；读取仍受路径保护和预算约束。

普通 agent 的模型工具为 `run_command`、`read_file` 和 `grep`。`grep` 内嵌 ripgrep 的 Rust 实现，不依赖系统 `rg`，只搜索文件内容；目录与文件名查询使用 `run_command` 调用 `ls` 等命令。`list_dir` 已移除；管道附件模式仅开放读取与内容搜索，不额外开放命令执行。

`nosh -s` 和 Ctrl+G 使用 **CommandAssist Generate** 的独立短对话，可以按需查询命令身份、帮助和项目文件；通过 `finish` 提交完整 shell program、必要澄清或无建议。只有经 brush 语法和可确认命令名检查的 program 才会输出或预填，从不自动执行。拒绝混合终态、Markdown 命令块、不完整语法和隐藏控制字符；动态行为无法确认不等于安全，执行前仍需检查。

用户命令执行成功后默认在后台生成 **Next** 后续建议，失败时生成 **Fix** 修正建议；没有合理下一步可以不建议。正常终端显示在提示符上方，Ctrl+G 接受，回车才执行；继续输入会取消并丢弃旧建议。裸 `ai fix` 生成修复命令，`ai fix <question>` 保留 Agent 诊断，`ai next` 显式请求后续建议。Agent 内部命令仍由原 Agent 继续处理，不触发新的辅助任务。可通过 `[shell] command_assist = false` 关闭自动辅助，保留显式入口。完整契约见 [CommandAssist 设计](docs/COMMAND-ASSIST.md)。

agent 命令遇到 SIGTTIN 或明确的 sudo 密码诊断时，harness 直接交回原命令并结束任务，不再调用模型或执行同轮后续工具；不会自动重试，也不接触用户密码。复合命令前面的部分可能已经执行；交接提示会明确警告，请检查当前状态和整条命令后再自行运行，以免重复副作用。

## 终端与字符兼容

### 最近命令输出

交互 shell 默认采集最近一条命令的终端输出，可在配置文件中关闭：

```toml
[shell]
capture_output = "last"  # last（默认）| off
```

`last` 使用会话级中转 PTY，在原样转发终端字节的同时，只在内存保留最近一条用户命令的 **4,096 字节文本尾部**。输出只在 `ai fix [question]`、失败后的快捷求助或自动失败诊断中按命令 ID 附带；普通 `#`、`ai "<任务>"` 和自然语言请求默认不带入。同一对话对同一命令只附正文一次，有报错证据时无需为获取同一报错重跑命令。它不改变失败提示/自动求助策略，也不改变建议模式的仅回填契约。完整注入矩阵和 PTY 协议见[输出采集设计](docs/OUTPUT-CAPTURE.md)。

PTY 合并的数据称为“终端输出”，不是分离的 stdout/stderr。`cmd > file` 仍只写文件；成功采集到空输出与未采集/不可用分开记录。超限保留尾部并标明截断，全屏或无法解释的终端控制标记不可用/不完整；已知后台混流不自动附带正文，不保证识别全部写入者。无法建立兼容 PTY 时会警告并保留原 shell 路径，不改成管道采集。`off`、`-c`、脚本和一次性 `-a/-s` 不进入中转路径。

不会额外写入 history、agent 输出文件或完整终端日志；已发送的证据遵守既有内存对话生命周期。**这不是脱敏功能**：显式启用的 `NOSH_EVAL_TRACE` 仍会按原契约记录模型输入中的证据。

### 显示与输入

当前 shell 面向 Linux（含 WSL）和 macOS；Windows 原生 shell 尚未实现。建议使用 UTF-8 locale 和支持 Unicode 的等宽字体。

| 环境 | nosh 自身界面的行为 |
|---|---|
| UTF-8 的 xterm、tmux/screen 等终端 | 彩色与 Unicode 标记；命令预览按终端列宽截断，保留完整的汉字、组合字符和 emoji 字素簇，并响应窗口缩放 |
| `NO_COLOR=1` 或 `CLICOLOR=0` | 不输出颜色/样式；仍可使用光标控制进行交互编辑 |
| `TERM=dumb`、`unknown` 或未设置 | 无 ANSI 控制序列；使用 ASCII 标记、基本输入编辑和静态下载提示，不使用高级补全/历史搜索 |
| 非 UTF-8 或未设置 locale、Linux 控制台或旧 VT 终端 | 装饰标记降级为 ASCII，不改写用户文本；字符编码按 `LC_ALL`、`LC_CTYPE`、`LANG` 的顺序判断，均未设置或为空时保守降级 |
| stdout/stderr 重定向、管道和 CI | 分别判断两个输出流；重定向不加 ANSI，`nosh -a` 的回答不加竖线，思考内容和诊断留在 stderr；不可见的审批提示不会接受输入 |

用户命令（包括 `-c`、脚本及其重定向）的输出按原始字节透传。agent 捕获的命令输出按 UTF-8 增量解码：跨块字符保持完整，非法字节显示为 `�`，不猜测 GBK/其他旧编码；这类命令应自行显式转码。终端预览移除 ANSI/OSC 控制序列并合并回车进度行，JSON 事件和保存结果不经过此显示层清理。

普通回答和命令输出保留 emoji 连字；审批卡片中的命令仍显式显示隐藏字符，双向文本控制符在普通文本中也会显示为转义。不同终端/字体对 emoji 和东亚歧义宽度的实现仍可能有差异。

## 文档

| 文档 | 内容 |
|---|---|
| [设计文档](docs/DESIGN.md) | 架构、当前实现边界、配置与后续方案；先读 [实现状态](docs/DESIGN.md#03-实现状态) |
| [输出采集设计](docs/OUTPUT-CAPTURE.md) | 最近用户命令输出的使用时机、上下文边界、PTY 协议、状态和兼容性 |
| [实时输入解析设计](docs/INPUT-ASSIST.md) | 输入辅助的数据流、判定语义、后台隔离、缓存与资源边界 |
| [Project context 设计](docs/PROJECT-CONTEXT.md) | 紧凑上下文、项目发现、AGENTS／README 加载和缓存边界 |
| [LLM tools 设计](docs/LLM-TOOLS.md) | 工具与模式、grep、权限、结果和建议契约 |
| [CommandAssist 设计](docs/COMMAND-ASSIST.md) | Generate / Fix / Next、简短 instructions、查询工具、finish 协议与后台调度 |
| [MVP 实施计划](docs/MVP-PLAN.md) | 已完成的历史范围和任务分解，不是当前待办 |
| [MVP 报告](docs/MVP-REPORT.md) | 分阶段实测、设计偏差、已知问题和数据来源 |
| [固定 seed 的真实模型评测](eval/README.md) | 原 27 场景回归、5 场景 CommandAssist 与新增 8 场景真实工作流专项分别运行；历史 main 的 48.0% 不代表当前评分成绩 |

开发入口见 [维护与扩展约定](docs/DESIGN.md#123-维护与扩展约定)：工具目录、对话日志、推理执行各自维护边界；错误显式传递，性能结论区分辅助路径优化与真实模型实测。

## 许可

Apache-2.0。`third_party/candle-core` 是打了一个小补丁的 candle-core（MIT OR Apache-2.0），来源与改动见其中的 [NOSH_PATCH.md](third_party/candle-core/NOSH_PATCH.md)。