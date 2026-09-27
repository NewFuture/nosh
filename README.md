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

## 命令建议与终端交接

项目指引优先加载适用的 `AGENTS.md`；确实没有 AGENTS.md 时，才提供最近 README 的精简参考片段，不与指引重复加载。新任务会按当前目录与文档版本更新；读取仍受路径保护和预算约束，README 中的示例不视为待办命令。

普通 agent 的模型工具为 `run_command`、`read_file` 和 `grep`。`grep` 内嵌 ripgrep 的 Rust 实现，不依赖系统 `rg`，只搜索文件内容；目录与文件名查询使用 `run_command` 调用 `ls` 等命令。`list_dir` 已移除；管道附件模式仅开放读取与内容搜索，不额外开放命令执行。

`nosh -s` 和 Ctrl+G 使用无工具的独立短对话，返回一个完整 shell program，经 brush 语法和可解析命令名校验后输出或预填，从不自动执行。接受单行、单一 shell fence 和完整多行结构；拒绝说明文字、多候选、不完整语法和隐藏控制字符。静态检查递归覆盖命令／进程替换，按顺序和作用域检查可确定的函数调用；动态命令名、`eval`／`source`、条件定义和递归等复杂动态行为只能视为“无法确认”，不因此拒绝或额外显示提示。校验不证明运行成功或符合用户意图，执行前仍需检查。普通 agent 的建议只显示在最终文本中，不自动预填。

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
| [MVP 实施计划](docs/MVP-PLAN.md) | 已完成的历史范围和任务分解，不是当前待办 |
| [MVP 报告](docs/MVP-REPORT.md) | 分阶段实测、设计偏差、已知问题和数据来源 |
| [固定 seed 的真实模型评测](eval/README.md) | 27 场景，revision 8 合并裁判解释修正及输出诊断契约；历史 main 的 25 场景双跑基线为 48.0%，不代表新数据集成绩 |

开发入口见 [维护与扩展约定](docs/DESIGN.md#123-维护与扩展约定)：工具目录、对话日志、推理执行各自维护边界；错误显式传递，性能结论区分辅助路径优化与真实模型实测。

## 许可

Apache-2.0。`third_party/candle-core` 是打了一个小补丁的 candle-core（MIT OR Apache-2.0），来源与改动见其中的 [NOSH_PATCH.md](third_party/candle-core/NOSH_PATCH.md)。