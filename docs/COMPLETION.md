# 命令与参数补全

Tab 打开候选菜单，菜单内 Enter 采用候选，再次提交才执行命令。菜单打开后，输入变化并闲置 300 ms 会刷新脚本候选；导航和重绘复用已有结果。

显式请求得到完整、未截断的唯一候选时可以直接回填。自动刷新只更新菜单；部分结果由用户明确选择。选择按候选身份保留，取消保留草稿并使旧查询失效。引用、空格、Unicode 和行中替换使用完整插入值，显示截断仅作用于菜单。

## 支持范围

| 来源 | 覆盖 |
|---|---|
| 命令名 | 当前启用的 builtin、alias、function、PATH 和 hash 中的有效外部命令 |
| 路径 | 当前相关目录的文件／目录、`cd`、重定向及已知路径参数 |
| Git 2.23+ | 子命令名称；`switch` 的选项、冲突样式、分支／起点及已支持的全局路径参数 |
| GNU Make | 常用选项、路径参数、字面目标、静态 `.PHONY` 和 include |
| npm／Yarn | `npm run`／`run-script`、`yarn run`／快捷调用中的项目脚本名和说明 |
| 已加载补全定义 | `complete`、`compgen`／`compopt`、函数／命令、自动加载及定义允许的回退 |
| 缩写 | 所有者启用的适用规则；明确选择后回填准确展开并通知所有者 |

原生命令／路径使用轻量子序列模糊匹配，完整、前缀、其它模糊依次优先，同分稳定。Smart case 折叠 ASCII 大小写，含大写的查询严格匹配；路径的 `nocaseglob` 设置优先。参数提供器使用各自的前缀规则。

已加载定义优先于内置提供器，其顺序、`nosort/noquote/nospace` 和回退规则保持有效。查询失败、无匹配和部分结果分别报告。当前参数无需补全时保留 worker，执行故障才进入回收和暂停流程。`complete_or_ai` 仅在完整无匹配时按现有输入编辑规则进入 AI 建议。

Make 静态读取规则、转义、续行和 include，CLI 路径参数使用分离值或长选项 `=值` 形式，按顺序读取已导出的 `GNUMAKEFLAGS`／`MAKEFLAGS` 中的 `-I`／`--include-dir` 路径；动态目标、条件、模式规则和未支持的 include 选项形式标为不完整。npm／Yarn 查找最近的 `package.json`（npm 显式 `--prefix` 限于指定目录），支持子命令前的 `--prefix`／`--cwd` 及 `=值` 形式。Yarn 快捷调用排除已知内置命令同名脚本，显式 `yarn run` 可补这些名称。脚本参数、workspace 和插件命令由已加载定义提供。

## 配置

```toml
[shell]
completion = true
completion_scripts = true
```

`completion_scripts = false` 跳过已加载脚本提供器，使用内置／原生来源；动态路径仍在隔离 worker 中展开。它与 `input_assist` 独立。基本终端、非 TTY、脚本和 `-c` 使用原有降级路径。

## 执行与资源

原生和脚本查询共用一个 worker。每个请求在后台调度阶段解析一次，上下文与执行类型随请求传递，等待和生成阶段复用结果。原生查询使用轻量快照；脚本按需恢复当前 shell 状态，自动加载及脚本改动保留在 worker 内。

**已加载脚本可以执行程序、访问文件或联网，`--offline` 不约束这些脚本。** 内置 Git／Make 会运行解析到的程序做查询（含 `--version`）；Makefile 和项目脚本名仅静态读取。取消和超时回收可确认归属的任务；未回收任务继续占槽。持续故障暂停自动刷新，显式请求可在回收后重试。

| 项目 | 上限 |
|---|---|
| worker／队列 | 1 个 worker，1 个执行中、1 个最新待处理；InputAssist 另有 2 个 worker |
| 请求期限 | 普通原生 500 ms；PATH 索引／脚本 1,500 ms |
| 集合／菜单 | 16,384 项、4 MiB；菜单 256 项；超限标记部分结果 |
| 缓存 | 集合 6 MiB，显示及派生副本 2 MiB |
| 输入／词 | 256 KiB／8 KiB |
| 快照／IPC | 原生 256 KiB、脚本 4 MiB、帧 8 MiB |
| Make 读取 | 合计 2 MiB、32 个文件、8 层 include |
| package.json 读取 | 2 MiB、最多 32 层祖先目录 |

PATH 名称缓存约 5 秒，其它本地集合约 1 秒，在实际请求时检查有效期。缓存按 cwd、参数和会话复用完整集合，追加字符可找到上一屏以外的候选。

## 扩展与嵌入

实现位于 `crates/nosh-shell/src/completion`：`context`／`snapshot` 提供上下文，`native`／`providers` 生成候选，`service`／`worker` 管理执行，`editor` 接入 Reedline。

新增命令在 `providers/mod.rs` 添加分派及候选函数，复用匹配、路径回填、集合限额和 `Cache::load`。嵌入宿主通过 `completion::Config.worker` 提供 `WorkerCommand`，内部入口调用 `input_assist::run_worker_from_env()`。

缩写所有者提供 `input_assist::Abbreviations` 的版本、适用名称和定义；`selection_observer` 直接接收 `&AbbreviationSelection`，包含 `name` 和 `revision`。源码准备与补丁维护见[源码维护](REEDLINE-MAINTENANCE.md)。
