# nosh

nosh 是一个用纯 Rust 实现、内置本地小模型（默认 MiniCPM5-2B）、可以断网运行的 AI shell。

> 状态：Linux 上的 MVP 已完成，结果见 [MVP 报告](docs/MVP-REPORT.md)。

- **本身就是 shell**：兼容 Bash（内核为 brush-core）。普通命令直接执行；`#` 显式交给 AI，未知命令先尝试本地纠错，执行失败默认提示求助入口。
- **上下文连续**：agent 和用户共用同一个 shell 会话，cwd、变量、venv 等状态会一直延续。
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

### 交互输入反馈

彩色交互终端中，输入会自动区分命令、字符串、变量、操作符、注释、路径与未完成结构；右侧提示显示检查中（`…`）、待完成（`… unfinished`）、确定的语法/命令错误（`!`）、无法确定（`?`）或诊断暂不可用及原因。红色并非审批结果，绿色路径只表示当前快照中存在，不保证权限；新建重定向目标不存在不算错误，前面的命令也可能创建后续输入文件。动态命令、行内 alias/function、临时 PATH 等上下文不确定时保守显示未知。AI 前缀（默认 `#`）及自然语言输入不按未知命令标红。

按键/绘制只扫描至多 4096 字节、256 个词法片段和 32 层嵌套；超限仍可编辑，但诊断暂停。完整 brush 解析及按需文件查询由**唯一后台 worker**完成：连续输入在最后一次编辑后等待 25 ms 再分析，只保留最新一条待处理输入，最多检查 32 个路径候选和 32 个 PATH 目录；同一文本重绘不再排队。150 ms 后停止等待并暂停本次编辑器的后台诊断；正在阻塞的系统调用**不会**被强杀，也不会启动替代 worker，重新启动 nosh 才恢复。输入、退格、取消和退出不等待该 worker；结果只适用于原输入与原会话快照。以上数值是资源保护上限，不是输入延迟或文件系统响应保证；完成的后台结果通常在下一次编辑事件显示。`NO_COLOR`、`TERM=dumb` 或非 TTY 不运行此诊断，但提交时的语法、路由与审批仍正常进行。输入反馈从不运行命令替换、补全函数或模型。

复现对比（2026-09-27，Linux AMD EPYC 7763、Rust debug 构建、16 个 PATH 目录；仅限这台测试机，不是响应保证）：`cargo build -p nosh-cli --locked` 后，用 `TERM=xterm-256color NOSH_DISABLE_AI=1 ./target/debug/nosh --norc --offline --no-download` 在 PTY 中逐字输入 20 个 ASCII 字符，记录每次写入至输出回显的耗时及进程退出时的最大 RSS；再加 `NO_COLOR=1` 重复。开启时中位/最大回显延迟为 0.29/0.69 ms、峰值 RSS 26604 KiB、编辑后 4 个线程；关闭时分别为 0.22/0.49 ms、26648 KiB、3 个线程。另用 `strace -ff -e trace=newfstatat,statx` 包裹相同命令：一次粘贴 `not_a_command_xyz` 后，开启时新增 16 次文件状态系统调用（每个 PATH 目录一次），关闭时新增 0 次；之后仅移动光标 10 次，两边均新增 0 次。Ctrl+C 取消尚未提交的 `echo "$(touch <临时文件>)"`、输入 Unicode 并退出，三种终端模式（彩色、`NO_COLOR`、`TERM=dumb`）均能继续输入且该文件未创建。阻塞 worker、过期结果和暂停后的队列上限另由 `cargo test -p nosh-shell --lib feedback` 的注入测试覆盖；本次 PTY 对比未模拟网络挂载。

## 命令建议与终端交接

`nosh -s` 和 Ctrl+G 使用无工具的独立短对话，返回一个完整 shell program，经 brush 语法和可解析命令名校验后输出或预填，从不自动执行。接受单行、单一 shell fence 和完整多行结构；拒绝说明文字、多候选、不完整语法和隐藏控制字符。静态检查递归覆盖命令／进程替换，按顺序和作用域检查可确定的函数调用；动态命令名、`eval`／`source`、条件定义和递归等复杂动态行为只能视为“无法确认”，不因此拒绝或额外显示提示。校验不证明运行成功或符合用户意图，执行前仍需检查。普通 agent 的建议只显示在最终文本中，不自动预填。

agent 命令遇到 SIGTTIN 或明确的 sudo 密码诊断时，harness 直接交回原命令并结束任务，不再调用模型或执行同轮后续工具；不会自动重试，也不接触用户密码。复合命令前面的部分可能已经执行；交接提示会明确警告，请检查当前状态和整条命令后再自行运行，以免重复副作用。

## 终端与字符兼容

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
| [MVP 实施计划](docs/MVP-PLAN.md) | 已完成的历史范围和任务分解，不是当前待办 |
| [MVP 报告](docs/MVP-REPORT.md) | 分阶段实测、设计偏差、已知问题和数据来源 |
| [固定 seed 的真实模型评测](eval/README.md) | 25 场景及 revision 2 任务语义；历史 main 双跑基线通过率 48.0%，新语义未重新采样 |

开发入口见 [维护与扩展约定](docs/DESIGN.md#123-维护与扩展约定)：工具目录、对话日志、推理执行各自维护边界；错误显式传递，性能结论区分辅助路径优化与真实模型实测。

## 许可

Apache-2.0。`third_party/candle-core` 是打了一个小补丁的 candle-core（MIT OR Apache-2.0），来源与改动见其中的 [NOSH_PATCH.md](third_party/candle-core/NOSH_PATCH.md)。