# nosh

nosh 是一个用纯 Rust 实现、内置本地小模型（默认 MiniCPM5-2B）、可以断网运行的 AI shell。

> 只维护当前版本。支持范围与限制见[系统设计](docs/DESIGN.md#03-实现状态)。

- **本身就是 shell**：兼容 Bash（内核为 brush-core）。普通命令直接执行；`# 任务` 显式交给 AI，未知命令先尝试本地纠错，执行失败默认提示求助入口。
- **前缀命令**：单独键入 `#` 自动弹出本地命令补全，`#help` 查看帮助，`#mode auto` 等管理命令本地解析；`#fix [补充说明]` 由 Agent 诊断上次失败。子命令和固定参数也支持 Tab 补全，未知命令或错误参数不转交模型。完整语法见[交互命令](docs/DESIGN.md#91-shell-界面)。
- **输入辅助**：交互输入自动高亮，提示待完成语法、命令识别和路径状态；慢查询不阻塞编辑，不改写或执行输入。
- **输入编辑**：启动时自动选择 Emacs/Vi，支持常用动作改键。Tab 优先补全、明确无结果时兜底 AI；F2 直接请求建议，仅回填、不执行。完整键位、搜索与取消规则见[输入编辑](docs/INPUT-EDITING.md)。
- **上下文补全**：命令与路径默认 smart-case 模糊；内置 Git `switch`、GNU Make 静态目标及 `npm run`／Yarn 项目脚本补全，保留已加载定义的语义。共用一个有界、可取消的补全进程；覆盖和限制见[补全说明](docs/COMPLETION.md)。
- **上下文连续**：agent 和用户共用同一个 shell 会话，cwd、变量、venv 等状态会一直延续。每次任务附上当前项目类型、manifest 基本信息及 Git 状态；切换项目或进入普通目录后重新判断，不沿用上一项目的描述。
- **本地推理**：基于 candle + GGUF。首次使用时按下载策略准备模型，之后可以完全离线；设备选择与运行限制见[推理说明](docs/INFERENCE.md)。
- **安全**：agent 发起的命令要经过风险分级和审批。

```bash
nosh                                   # 进入 nosh shell
#help                                  # nosh 提示符内查看帮助
# 找出当前目录下最大的 10 个文件         # 前缀后加空白，交给 AI
nosh -a "把 logs 里 7 天前的日志打包"   # 一次性任务
nosh -s "解压 foo.tar.zst 到 /tmp"     # 只输出命令
```

## 构建与运行（Linux）

```bash
cargo source prepare
cargo build --release                  # rust-toolchain.toml 固定 Rust 1.98.1
./target/release/nosh model pull       # 下载并校验模型（约 1.57 GB），之后可离线
./target/release/nosh doctor           # 检查 CPU、内存、模型、下载源
./target/release/nosh                  # 启动 shell；--norc 跳过 ~/.bashrc，--safe 同时关闭 AI
```

首次 clone、切换依赖版本或补丁后，先准备 Reedline 与 brush 修补源再运行 Cargo/IDE；重复准备不会覆盖未导出的本地修改。普通 Rust 构建不要求 Python、Node 或 npm。离线准备、同仓联调、补丁导出与升级见 [源码维护说明](docs/REEDLINE-MAINTENANCE.md)。

常用选项：`--auto` / `--yolo`（审批模式）、`--offline`、`--model-path <gguf>`、`--no-download`。Linux 默认配置为 `~/.config/nosh/config.toml`，平台路径、支持的键和可用示例见 [配置说明](docs/DESIGN.md#11-配置)。`--offline` 阻止模型下载与探测，不限制 shell 命令自身联网。

AI 任务默认显示 **`审批: 自动`**；可用 `#mode confirm|auto|yolo` 切换。用户 deny 始终优先，有效用户白名单三档免审批，并可覆盖内置禁止；未获白名单覆盖的内置禁止在询问模式须键入 `yes`，自动 / YOLO 直接拒绝。YOLO 对其他操作免逐次审批，不绕过工具范围或外部认证。

自动模式以**便利优先、防御破坏**为目标：普通 `mv` / `cp` 默认执行，危险目标、破坏性效果与用户 deny 仍拦截；不要求原子不覆盖或备份证明。常见构建 / 测试 / 检查也默认执行，明确接受未知项目代码风险，不代表沙箱隔离或可恢复保证。规则采用 TOML 条目，例如 `deny = [{ command_prefix = "docker system prune" }]`，不再使用旧字符串 glob 数组。完整矩阵、作用域及限制见 [审批说明](docs/APPROVAL-MODES.md)。

`#auto off` 只暂停部分自动路由，不是全局禁用 AI；彻底关闭 nosh AI 可用 `NOSH_DISABLE_AI=1`（保留 rc）或 `--safe`（同时跳过 rc），此时也不拦截前缀命令。具体例外见 [输入判定与开关边界](docs/DESIGN.md#42-ai-触发与输入判定)。

## 实时输入提示与语法高亮

正常交互终端默认启用 `input_assist`，无需插件或模型；`--safe` / `NOSH_DISABLE_AI=1` 不会关闭它。输入按语法高亮，命令与路径状态异步查询；未知或动态行为不伪装成确定错误，路径存在不代表已获执行授权。可在用户配置中关闭：

```toml
[shell]
input_assist = false
```

基本终端和非 TTY 不启用实时输入提示；`NO_COLOR` 关闭样式，但保留已启用的状态栏文字反馈。判定规则、两个有界辅助进程、资源预算和恢复条件见[实时输入辅助](docs/INPUT-ASSIST.md)。

## 可选 NVIDIA CUDA 推理

默认构建是 CPU-only；CUDA 构建的 `auto` 根据可见设备与可用显存选择后端。显式 `--device cuda[:N]` 失败不会回退 CPU，GPU 当前仅支持 f16 KV。构建步骤、设备选择和运行限制见[本地推理与 CUDA](docs/INFERENCE.md)。

## 命令建议与终端交接

交互 shell 在**提示符上方**显示环境、输入状态、操作提示与模式，不预留屏底区域；`[shell] status_bar = false` 可关闭。**tmux 的未提交草稿历史残留仍未解决**。布局、主题注入和兼容边界见[状态行设计](docs/STATUS-BAR.md)。

Agent 使用 `exec`、`read_file`、`grep`，有交互终端时可提问；管道附件模式不开放命令执行，也不从管道读取提问回答。项目指引优先使用适用的 `AGENTS.md`，否则使用 README 简介与索引。详见[工具契约](docs/LLM-TOOLS.md)和[项目上下文](docs/PROJECT-CONTEXT.md)。

**CommandAssist 只建议，不自动执行：**

| 意图 | 入口与行为 |
|---|---|
| Generate | `nosh -s`、非管理输入的非空 F2、适用的 Tab 兜底；输出或预填完整命令 |
| Fix | 用户命令失败后的后台修复建议；不自动执行 |
| Next | 用户命令成功后的后台建议；没有合理下一步可以不建议，无手动入口 |

手动 `#fix [补充说明]` 始终进入 Agent，自动携带上次失败命令、退出码及匹配输出；说明可省略，不走 CommandAssist Fix。Agent 的工具调用仍受现有审批规则约束。

空白草稿用 F2 接受已有候选，回车才执行；没有候选时不额外请求模型，继续输入会取消旧建议。`[shell] command_assist = false` 关闭自动辅助，但保留 Generate 和 Agent 入口（包括 `#fix`）。完整协议、命令校验与调度见[CommandAssist](docs/COMMAND-ASSIST.md)；静态校验不能证明动态命令安全。

agent 命令遇到 SIGTTIN 或明确的 sudo 密码诊断时，harness 直接交回原命令并结束任务，不再调用模型或执行同轮后续工具；不会自动重试，也不接触用户密码。复合命令前面的部分可能已经执行；交接提示会明确警告，请检查当前状态和整条命令后再自行运行，以免重复副作用。

## 终端与字符兼容

### 项目环境与命令区域

可显式设置 `[shell] project_env = "direnv"` 或 `"mise"` 接入已授权的项目环境，默认关闭、两者互斥。人工提示符与 agent 工具调用之间刷新；单条 `cd … && …` 内不自动刷新。加载失败时明确警告，人工仍可修复，agent 停止执行。mise 首版仅支持已安装工具的 PATH/业务变量，不接管完整激活功能。

`[shell] terminal_integration = "auto"` 默认根据终端提示简单识别，使用通用 OSC 7/133 报告目录和真实命令区域；可设为 `off` 或 `on`。工具版本、恢复规则、终端范围与资源预算见[项目环境与终端集成](docs/SHELL-INTEGRATION.md)。

### 最近命令输出

交互 shell 默认采集最近一条命令的终端输出，可在配置文件中关闭：

```toml
[shell]
capture_output = "last"  # last（默认）| off
```

`last` 原样转发终端字节，只在内存保留最近用户命令的 **4,096 字节文本尾部**，按命令 ID 用于失败诊断，普通任务默认不带入。它是合并的终端输出，不是分离的 stdout/stderr；不改变文件重定向，不额外写完整日志。`off`、`-c`、脚本和一次性 `-a/-s` 不进入中转路径。

**这不是脱敏功能**：显式启用的 `NOSH_EVAL_TRACE` 仍可能记录已注入的证据。注入矩阵、去重、混流、不可用状态和 PTY 降级规则统一见[输出采集设计](docs/OUTPUT-CAPTURE.md)。

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
| [文档导航](docs/README.md) | 当前设计、功能契约与测试评测的完整入口 |
| [代码架构与维护](docs/ARCHITECTURE.md) | crate 依赖、接口边界、实现与测试组织、开发命令 |
| [系统设计](docs/DESIGN.md) | 当前行为、配置与实现限制 |
| [真实模型评测](eval/README.md) | 当前运行器、场景、观测与报告契约 |

## 许可

Apache-2.0。`third_party/candle-core` 是打了一个小补丁的 candle-core（MIT OR Apache-2.0），来源与改动见其中的 [NOSH_PATCH.md](third_party/candle-core/NOSH_PATCH.md)。Reedline、brush 上游及派生补丁采用 MIT；固定上游的 `LICENSE` 随源准备保留，来源与维护方式见 [源码维护说明](docs/REEDLINE-MAINTENANCE.md)。