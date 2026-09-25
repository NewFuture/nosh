# nosh

nosh 是一个用纯 Rust 实现、内置本地小模型（默认 MiniCPM5-2B）、可以断网运行的 AI shell。

> 状态：Linux 上的 MVP 已完成，结果见 [MVP 报告](docs/MVP-REPORT.md)。

- **本身就是 shell**：兼容 Bash（内核为 brush-core）。合法的命令照常执行；以 `#` 开头或者命令出错时，交给 AI 处理。
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

常用选项：`--auto` / `--yolo`（审批模式）、`--offline`、`--model-path <gguf>`、`--no-download`。配置文件位于 `~/.config/nosh/config.toml`，支持的键见设计文档 §11。

## 命令建议与终端交接

`nosh -s` 和 Ctrl+G 使用无工具的独立短对话，返回一个完整 shell program，经 brush 语法和可解析命令名校验后输出或预填，从不自动执行。接受单行、单一 shell fence 和完整多行结构；拒绝说明文字、多候选、不完整语法和隐藏控制字符。语法校验不能证明命令符合用户意图，执行前仍需检查。普通 agent 的建议只显示在最终文本中，不自动预填。

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

- [设计文档](docs/DESIGN.md)
- [MVP 实施计划](docs/MVP-PLAN.md)
- [MVP 报告](docs/MVP-REPORT.md)
- [固定 seed 的真实模型评测](eval/README.md)

## 许可

Apache-2.0。`third_party/candle-core` 是打了一个小补丁的 candle-core（MIT OR Apache-2.0），来源与改动见其中的 [NOSH_PATCH.md](third_party/candle-core/NOSH_PATCH.md)。