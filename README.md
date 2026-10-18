# nosh

nosh 是用 Rust 编写的 **Bash 兼容 AI shell**：日常命令照常运行，需要帮助时直接在终端里描述任务。

默认使用本地 MiniCPM5-2B Q8_0，按可用内存／显存调整为 Q4_K_M；资源不足时不自动安装或加载模型，显式设置优先，详见[模型选择规则](docs/DESIGN.md#21-模型与格式)。准备好模型后可离线推理，无需云端 API。支持 **Linux（含 WSL）、macOS，以及 Windows 10/11 x64 的原生 CPU 版 `nosh.exe`**。Windows 不需要 WSL 或 Git Bash；平台差异与首版限制见[系统设计](docs/DESIGN.md#46-平台)。

## 功能特性

| 功能 | 你可以做什么 |
|---|---|
| Bash 兼容 | 运行普通命令、管道和脚本，继续使用变量、别名与函数 |
| AI 任务 | 输入 `# 任务`，让 Agent 查询文件、分析问题并按审批策略执行命令；缺少信息时可向你提问 |
| 连续的会话上下文 | Agent 与用户共享当前目录、变量和虚拟环境；自动了解当前项目与 Git 状态 |
| [命令建议](docs/COMMAND-ASSIST.md) | 输入描述后按 F2，或使用 `nosh -s` 生成命令；命令结束后还可获得修复或下一步建议，**建议不自动执行** |
| 纠错与诊断 | 未知命令先尝试本地拼写纠错；用 `#fix` 让 Agent 根据上次失败命令及可用输出诊断，也可补充要求 |
| [输入编辑](docs/INPUT-EDITING.md) | Emacs／Vi 编辑、自定义快捷键；[语法高亮和实时提示](docs/INPUT-ASSIST.md)无需模型 |
| [上下文补全](docs/COMPLETION.md) | 模糊补全命令与路径，补全 `git switch` 分支、GNU Make 目标和 npm／Yarn 项目脚本 |
| [上下文历史](docs/HISTORY.md) | 用上下键或 Ctrl+R 搜索历史，按工作树筛选，Tab 回填编辑；Enter 兼容，回填不执行 |
| [可见缩写](docs/ABBREVIATIONS.md) | 用短名称展开常用命令，展开后可检查、修改和撤销；支持自定义规则 |
| [主题](docs/THEMES.md) | 内置深浅主题，提示符显示环境与操作状态；可选 Starship 和 Powerline |
| [项目环境与终端集成](docs/SHELL-INTEGRATION.md) | 可选接入已授权的 direnv／mise 环境，并向兼容终端报告目录和命令区域 |

默认设备为 `auto`：普通构建使用 CPU，CUDA 构建会根据可见 GPU 和可用显存选择 GPU 或 CPU。详见[本地推理与 CUDA](docs/INFERENCE.md)。

## 开始使用

取得 `nosh` 可执行文件后（[构建入口](docs/ARCHITECTURE.md#开发入口)），在终端运行：

```bash
nosh model pull    # 按资源选择并校验 2B Q8（约 2.69 GB）或 Q4（约 1.57 GB）
nosh doctor        # 检查运行环境与模型
nosh               # 进入 shell
```

普通命令不需要模型。未提前下载时，首次使用 AI 会按下载设置准备模型；已有模型后可用 `nosh --offline` 启动。

在 **nosh 提示符内**，普通命令和 AI 任务可以交替使用：

```text
git status
# 找出当前目录下最大的 10 个文件
#help
#mode confirm
#theme soft-light
```

单独键入 `#` 可浏览管理命令。命令失败后，输入 `#fix` 请求诊断，或用 `#fix 只分析原因，不修改文件` 补充要求。

`#assist off/on` 暂停或恢复自动错误路由与后台建议，显式 AI 入口仍可使用；审批模式单独由 `#mode` 控制。模型加载未就绪或失败后，普通命令不会反复触发加载，显式 AI 请求成功或再次 `#assist on` 可恢复检查；`#status` 查看当前开关和模型状态。

也可以从原有 shell 一次性调用：

```bash
nosh -a "把 logs 里 7 天前的日志打包"   # 执行一次 AI 任务
nosh -s "解压 foo.tar.zst 到 /tmp"     # 只输出建议命令
```

### 保留现有 Bash

在 Linux／macOS 的 **Bash 5.1+** 交互终端中加载：

```bash
eval "$(nosh init bash)"
```

输入描述后按默认的 **F2**，建议会填回命令行，检查后自行回车执行。初始化不会修改 rc 或默认 shell。完整用法见[Bash 集成](docs/INPUT-EDITING.md#外部-bash-的轻量建议入口)。

## 使用时需了解

Agent 默认采用自动审批，可用 `#mode confirm` 切到询问模式。自动模式允许常见低风险操作及构建／测试，用户 deny 始终优先。**审批不是沙箱，也不保证操作可撤销**；检测到命令需要终端或密码时会交回用户，不自动重试，但命令可能已有部分执行。详见[审批说明](docs/APPROVAL-MODES.md)。

`--offline` 只禁止模型下载与探测，不限制 shell 命令联网。`--safe` 关闭 AI 并跳过 rc；`NOSH_DISABLE_AI=1` 关闭 AI 但保留 rc。常用设置见[配置说明](docs/DESIGN.md#11-配置)。

交互 shell 默认保留最近命令的有界输出尾部用于失败诊断，可关闭。**本地运行不等于自动脱敏**，显式启用的评测 trace 也可能保存相关内容，见[输出采集说明](docs/OUTPUT-CAPTURE.md)。

建议使用 UTF-8 终端。基础终端会使用简化显示；tmux 的未提交草稿历史残留仍是[已知限制](docs/STATUS-BAR.md)。

## 文档

| 文档 | 内容 |
|---|---|
| [功能文档导航](docs/README.md) | 各功能的详细说明与配置 |
| [系统设计](docs/DESIGN.md) | 功能契约、实现边界与限制 |
| [开发指引](AGENTS.md) / [代码架构](docs/ARCHITECTURE.md) | 日常开发、构建与代码组织 |
| [真实模型评测](eval/README.md) | 评测运行方式与结果口径 |

## 许可

Apache-2.0。`third_party/candle-core` 是打了一个小补丁的 candle-core（MIT OR Apache-2.0），来源与改动见其中的 [NOSH_PATCH.md](third_party/candle-core/NOSH_PATCH.md)。Reedline、brush 上游及派生补丁采用 MIT；固定上游的 `LICENSE` 随源准备保留，来源与维护方式见 [源码维护说明](docs/REEDLINE-MAINTENANCE.md)。