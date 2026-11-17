# 原生桌面终端

`nosh-desktop` 是可在本机运行的纯 Rust 桌面入口，不是浏览器原型。界面使用
egui/eframe，WGPU 负责原生图形渲染；Alacritty terminal 提供终端状态机与
PTY/Windows ConPTY。本地终端启动真实 nosh，远程终端启动系统 OpenSSH。
没有 WebView、JavaScript、Node、HTTP 控制服务或云端 API。

## 构建与运行

使用仓库固定的 Rust 工具链及[平台构建环境](ARCHITECTURE.md#开发入口)。
桌面程序不属于默认构建目标，CLI 的普通构建不要求图形依赖。

原生 Windows PowerShell：

```powershell
cargo source prepare
cargo build -p nosh-cli -p nosh-desktop --locked
.\target\debug\nosh-desktop.exe
```

Linux/macOS 使用相同 Cargo 命令，运行 `target/debug/nosh-desktop`。需要本地图形
会话、WGPU 可用的图形驱动以及 X11/Wayland（Linux）或系统窗口环境。
Windows 使用 WGPU 的 DX12 后端，不依赖系统 OpenGL 或 WebView2。
本次原生运行验证的平台为 Windows x64；其它平台仍需各自的窗口环境验证。

启动参数：

```powershell
.\target\debug\nosh-desktop.exe --cwd 'C:\work tree' --nosh 'C:\apps\nosh.exe'
```

CLI 查找顺序为 `--nosh`、`NOSH_BIN`、桌面可执行文件同目录、绝对 PATH 目录。
显式指定的路径无效时直接报告错误，不悄悄切换到另一个 shell。
不要求把仓库或 CLI 安装到主检出目录；两个可执行文件放在一起即可使用。

## 本地会话

启动后打开一个真实 nosh 会话，点击 `This computer +` 或标签栏 `+` 新建独立会话。
输入、输出、环境、cwd 和作业由各自的 CLI 进程持有，切换标签不重建进程。
终端支持 ANSI 颜色、光标控制、备用屏幕、窗口缩放、Unicode 输入、选择复制、
粘贴、方向键、Ctrl+C、Tab、历史及基本 SGR 鼠标按键。按住 Shift 可在鼠标模式
程序中选择文本；使用 Ctrl+Shift+C 复制，Ctrl+Shift+V 粘贴（macOS 使用系统快捷键）。
终端输出保留最多 5,000 行滚动历史。
光标、颜色与窗口大小查询由 PTY I/O 路径回复，不依赖前台窗口绘制或标签是否可见。

上方目录标识明确表示**启动目录，不是当前 cwd 快照**。没有解析普通输出来猜测
目录或命令状态。状态栏显示的是本地进程状态；SSH 进程存活不代表已经认证成功。

本地会话默认传入 `--offline --no-download`，不自动下载模型。模型未安装时普通
shell 仍可用，需要 AI 时在终端显式运行 `nosh model pull` 安装；该显式命令按 CLI
自身规则联网。桌面离线设置不阻止用户 shell 命令或 SSH 联网。

## SSH

点击 `+ SSH` 或 `New connection`，填写名称与主机/IP，也可以填写已有 OpenSSH
`Host` 别名。用户、端口、身份文件和跳板机为空时使用系统 OpenSSH 配置；填写后
分别传为独立的 `-l`、`-p`、`-i`、`-J` 参数，不拼接 shell 命令。

`Connect once` 只打开会话；`Save and connect` 另存连接元数据。保存的连接支持重复
打开多个独立终端，也可从侧栏移除；移除配置不关闭已运行的会话。

- 认证、密码、密钥口令、首次主机指纹验证均在真正的 SSH 终端中完成，不由 AI 读取。
- 目标连接显式要求 `StrictHostKeyChecking=ask`；不提供忽略主机密钥变化的按钮。
- 支持 SSH agent、身份文件、别名、代理及跳板机配置；不自行实现 SSH 协议或保存密钥。
- 桌面禁用 agent forwarding、OpenSSH `LocalCommand`、配置的远程启动命令及端口转发；
  不把这些能力作为普通终端连接的隐含副作用。其它受信任的 OpenSSH 配置照常生效。
- 连接失败保留 SSH 原始诊断与真实退出码，不显示模拟“连接成功”。

关闭终端或退出应用需要确认，并回收本机 PTY 与 I/O worker；本地 AI 请求同时取消。
SSH 断开不保证服务器上的程序停止，不代表回滚。重新连接创建新 shell，不自动重放
命令，不声称恢复旧进程或环境。

当前为基础 SSH 终端，不部署远端 nosh，不提供结构化远程 Agent。可在远端终端
自行运行已经安装的 nosh；桌面不会把本地文件、审批或项目上下文冒充远端状态。

## 本地 AI

右侧面板调用真实 `nosh --offline --no-download -s -- <描述>`：

- 使用所选本地标签的**启动目录**，运行在独立只读查询 shell；不共享终端内后续
  `cd`、变量、激活环境或 Agent 对话，也不自动附带终端输出。
- 模型与错误来自真实 CLI；未安装模型、加载失败、无建议和推理失败都明确显示。
- 结果只作为草稿显示，可复制，不发送到终端输入，更不会自动执行。这样不会误将
  生成文本写入密码提示或正在运行的交互程序。
- 同一桌面一次只运行一个面板建议请求，最多 180 秒、stdout/stderr 合计 64 KiB；
  停止、关闭标签或退出都会取消并回收该请求的进程组。
- 各终端 CLI 与面板建议仍是不同进程，可能分别加载模型，不提供跨进程共享模型。

完整本地 Agent 继续使用 nosh 终端的 `# 任务`、`#mode` 等现有入口与真实审批；
面板不伪造审批卡，也不改写终端的审批模式。远程标签禁用本地建议，避免上下文混用。

## 配置与限制

设置保存在系统用户配置目录下的 `nosh/desktop.json`，Windows 通常位于
`%APPDATA%\nosh\desktop.json`。只保存连接元数据、主题和字体大小，不保存密码、
终端输出或 AI 请求。原子写入失败会报告错误；文件损坏、版本未知或超出大小限制时
保留原文件并禁用保存，需要修复后重启。当前未实现多实例同时编辑配置的合并。

当前终端提供基本交互，不宣称覆盖所有终端扩展：鼠标拖动/滚轮上报、复杂字符塑形、
图片协议及完整可访问性语义尚未实现；OSC 剪贴板读写被禁用。没有 SFTP、端口转发
管理、自动连接恢复或图形化远程审批。默认字体不保证覆盖中日韩字形；Unicode 输入
传输与字形覆盖是不同能力，系统字体回退尚待完善。

## GPUI 本地验证入口

此分支额外提供原生 `nosh-gpui`，窗口标题为 `nosh - GPUI Workbench`。
它固定使用 crates.io 的 Apache-2.0 `gpui = 0.2.2`，不是 WebView，也没有复制
Zed 的 GPL 终端模块。egui 的 `nosh-desktop` 入口保留，两个入口共用 Alacritty
终端、ConPTY 生命周期及 OpenSSH 参数校验。CLI 的零 OSC 缓冲解析器仍独立。

原生 Windows x64/MSVC：

```powershell
cargo source prepare
cargo build -p nosh-cli -p nosh-desktop --features gpui --locked
.\target\debug\nosh-gpui.exe --cwd 'C:\work tree'
```

工作台采用会话优先的桌面布局：顶部会话标签、目标信息栏、可折叠
工作区导航和底部状态栏。导航将已打开的会话与保存的 SSH 目标分组；
点击已有会话仅切换，不启动新进程，可直接从导航请求安全关闭。
底部放置状态与字号控制，复制、粘贴等操作集中到可搜索入口，
终端不放在留白卡片中。界面使用 Segoe UI；
标签、按钮、列表与对话框由 GPUI 自绘，不是 Windows 系统控件。
当前 GPUI 界面采用统一的深石墨配色：青绿强调活跃标签、主操作及输入焦点，
SSH 身份保留浅蓝色标记，错误和关闭操作使用独立危险色。侧栏、工具栏与弹窗
用低对比分隔和表面层次区分；终端保留应用自己的 ANSI 颜色，字体大小沿用用户设置。
Windows 将会话标签与标题栏合并为一行，右侧提供最小化、最大化/还原和关闭按钮，
通过 GPUI 的系统命中区域交给 Windows 处理。标签间空白、`n_` 标识和窗口按钮左侧
空白可拖动窗口，双击空白最大化/还原；标签本身仍只切换会话。关闭仍经过安全确认，
搜索、连接或关闭对话框打开时不激活背后的标题栏控件。其它平台保留系统标题栏。
标签显示本地/SSH 类型、名称和进程状态色点；点击标签或
Ctrl+Tab / Ctrl+Shift+Tab 切换，切换不重启进程。
新建、切换、关闭标签或调整窗口宽度后，活跃标签自动滚入视野；
标签溢出时显示前后切换按钮，也可横向滚动查看。
`+ New`、导航中的 `New session` / `Quick open`、终端目标栏的 `Actions...`
及 Ctrl+Shift+P 打开同一个搜索入口：新建本地终端、新建 SSH、切换已有会话、
选择保存的目标，以及复制选择、粘贴、关闭和视图操作。搜索按空白分词、不区分大小写，
每个词都须匹配名称、目标、状态或分组；不会执行用户输入的搜索文本。
上下键或 Tab / Shift+Tab 选择，Enter 打开，Escape 返回原会话；
结果自动滚动到键盘选择项。输入法预编辑期间不激活结果或抢占候选导航。
搜索使用独立的 Unicode/UTF-16 编辑状态，不改 SSH 草稿或终端待输入内容。
Ctrl+Shift+T 仍直接新建本地终端，Ctrl+Shift+N 直接打开空白 SSH 表单，
Ctrl+Shift+W 请求关闭当前会话。标题栏侧栏图标 / Ctrl+Shift+B 折叠或展开工作区导航，
不暂停当前终端。默认在窗口内容宽度小于 1000px 时收起导航，
手动切换后保持用户本次窗口的选择。

SSH 在居中模态对话框中分为 Destination（主机、用户、端口）与
Profile & access（显示名称、身份文件、跳板机）两组。新建时使用空白草稿；
点击保存的目标先填入表单供检查、修改，不立即建立网络连接。
显示名称为空时使用主机；空白可选字段使用 OpenSSH 配置。`Connect` 只连接一次，
`Save & connect` / Ctrl+Enter 保存连接元数据；从保存的目标进入时更新该项，
不重复添加。Connect 不改变保存项。终端继续显示原始 SSH 输出与退出码，
连接进程存活不代表认证成功。连接或搜索对话框打开时底层终端输入暂停；
较矮窗口中的字段区可滚动，连接与取消按钮保留在底部。

点击会话标签的 `x` 或关闭窗口会打开模态确认，目标绑定稳定的会话 ID。
确认期间按键、剪贴板粘贴、原生文本提交和 IME 预编辑都不能写入 PTY，也不能
点击背后的会话或按钮。默认选中保留会话：Enter 保留，Escape 始终保留；
Tab 或左右键切换按钮，显式选中关闭后 Enter 才确认，或直接点击关闭按钮。
提示始终说明 Enter 执行当前选择、Escape 保留；选中的关闭按钮使用危险色。
确认后回收 PTY 与本地进程；SSH 远端程序不保证停止。

终端使用 GPUI 原生字体与绘制，支持 ANSI 颜色、备用屏幕、键盘/Unicode 输入、
窗口缩放、滚动历史、拖动选择、Ctrl+Shift+C/V 复制粘贴及 Ctrl+加号/减号
字体缩放（Ctrl+0 重置）；复制、粘贴也可从搜索入口执行，字号按钮在状态栏。
PTY 协议查询仍在 I/O 路径回复，后台会话不依赖绘制。
SSH 字段支持点击定位、左右/Home/End、Shift 扩选、Backspace/Delete、
Ctrl+A/C/X/V、UTF-16 范围替换及 IME 预编辑。Tab / Shift+Tab 先遍历 Destination，
再遍历 Profile & access，最后进入 Cancel、Save & connect、Connect 按钮，
然后循环回主机字段；按钮显示焦点边框，
Enter / Space 执行选中的按钮。字段内 Enter 连接一次，Ctrl+Enter 保存并连接，
Escape 始终返回终端；按钮获得焦点时文本、粘贴与预编辑均不会修改字段或落入背后的 PTY。
这不是完整富文本编辑器，尚不提供单词/字素级导航、撤销或鼠标拖动扩选。

GPUI 设置单独保存在系统用户配置目录的 `nosh/desktop-gpui.json`，不覆盖 egui
配置；保存的连接在连接列表中重新打开。损坏配置禁用保存并报告错误。此实验尚未迁移
egui 的独立 AI 建议面板、主题切换和连接删除按钮；AI 仍通过真实 nosh 终端显式
调用，不把本地上下文用于 SSH。尚不支持终端鼠标协议上报、图片协议或完整
可访问性语义。此入口只承诺本次 Windows 原生验证，其它平台尚未运行验证。
本机验证保存/编辑流程时可使用 `--settings <path>` 指定独立设置文件，
避免改变日常连接配置；省略时仍使用上述系统配置目录。

```powershell
cargo test -p nosh-desktop --features gpui --locked -- --test-threads=1
cargo clippy -p nosh-desktop --features gpui --lib --bins --locked -- -D warnings
```

GPUI 回归额外覆盖模态输入隔离、默认保留/显式关闭、表单 Unicode 编辑及大小限制、
搜索文本与 SSH/PTY 隔离、输入法候选期间禁止激活，以及跨名称和目标的多词搜索；
真实窗口交互仍需本机图形会话验证，不以无窗口测试替代原生运行。

## 回归

不加载真实模型、不下载模型；PTY 测试不要求创建图形窗口：

```powershell
cargo test -p nosh-desktop --locked -- --test-threads=1
cargo clippy -p nosh-desktop --all-targets --locked -- -D warnings
$env:NOSH_TEST_BINARY = (Resolve-Path .\target\debug\nosh.exe).Path
cargo test -p nosh-desktop --locked real_nosh_keeps_shell_state -- --ignored --test-threads=1
```

覆盖参数边界、配置落盘/损坏、建议错误与取消、原生 PTY 输入输出/缩放/退出、
真实 nosh 状态延续，以及 SSH 连接本机关闭端口时的真实失败反馈。
SSH 成功认证与企业跳板机仍需用户自己的可访问服务器验证；测试不会连接外部主机。
