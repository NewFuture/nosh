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
cargo build -p nosh-desktop --features gpui --bin nosh-gpui --locked
.\target\debug\nosh-gpui.exe --cwd 'C:\work tree'
```

GPUI 始终使用内置的同一份 nosh 代码：只复制 `nosh-gpui.exe` 即可创建本地会话，
不需要旁置或 PATH 中的 `nosh.exe`，也不把另一个程序嵌入后解压。
每个本地 PTY 仍是独立进程：桌面以自己的绝对路径和私有 shell 参数启动自身，
在解析桌面选项、创建 GPUI 窗口或线程之前进入原 CLI。补全、输入辅助等 worker
也由同一个 exe 直接分发，不会再次打开 GUI。CLI 参数、退出码、审批与模型装配
沿用普通 CLI；内部模式不是向桌面进程输入命令的控制接口。

Windows 的 GUI-subsystem 程序不能直接获得 ConPTY 控制台，因此本地启动适配层
先使用 `GetSystemDirectoryW` 定位系统 `cmd.exe`，仅运行固定 bootstrap 并等待
同一桌面 exe 的内置 shell；每个本地会话多一个轻量等待进程。不查 PATH/COMSPEC，
禁用 AutoRun，不生成脚本或解压二级 exe。bootstrap 只含固定参数，exe 路径通过
延迟展开作为数据传递，不把终端输入、工作目录或用户命令拼进 cmd 脚本。
内置 shell 即使已有完整重定向句柄也会附加父控制台，保留重定向并恢复正常 Ctrl+C 策略；
没有父控制台时仍允许标准输入、输出和错误全部重定向的非交互命令运行。
不会调用 `AllocConsole` 打开额外控制台窗口。用户命令仍全部由 nosh/brush 执行，
结束码经等待进程返回，关闭 PTY 回收所属 shell，shell 再回收自己的有界 worker。
其它平台直接 self-exec，无此 cmd 等待层。该单文件折中不是其它终端的通用发布架构。

GPUI 不提供 `--nosh` 外部 CLI 覆盖，也不读取 `NOSH_BIN` 来选择本地 shell。
只有 egui 入口保留这些覆盖选项及前文的“旁置/PATH CLI”查找规则。
终端中的 `nosh` 命令名不会自动安装到 PATH；若需调用内置 CLI 的其它子命令，
可使用桌面 exe 的完整路径加 `--__nosh_shell`，或另行安装普通 CLI。
`init bash` 生成的建议命令保留自身路径及内部模式参数。

单个 release 产物：

```powershell
cargo source prepare
cargo build --release -p nosh-desktop --features gpui --bin nosh-gpui --locked
.\target\release\nosh-gpui.exe
```

“单 exe”不是完全静态分发：Windows/UCRT、VC 运行库及系统图形 DLL 仍由系统提供，
本地会话还依赖系统 `cmd.exe`，SSH 仍调用系统 `ssh.exe`；
模型权重、tokenizer、用户配置和数据均不嵌入 exe。
默认会话继续使用 `--offline --no-download`，没有安装模型也能使用普通 shell；
本次整合不下载模型、不改变系统 OpenSSH 策略。
`desktop/tests/embedded_shell.rs` 将唯一 exe 复制到独立目录、清空 PATH、隔离
`NOSH_HOME`，覆盖重定向/退出码、内部 worker 分发、真实多会话 PTY、文件补全、
中文、Ctrl+C，以及精确 shell/锚点/worker PID 的回收。Windows exe 测试名包含
空格、中文、`%`、`!`、`&`、`^` 和括号，并以无效 COMSPEC、预设 ERRORLEVEL
验证系统路径和退出码不被这些环境设置替代。
设置 `NOSH_GPUI_TEST_BINARY` 为 release 的绝对路径后可用同一回归验证 Windows
GUI subsystem 的实际产物，而不以 debug 控制台程序的行为替代。

工作台只保留顶部会话标签、local/SSH 目标栏、可折叠连接库和底部状态/字号控制。
界面采用 Segoe UI 和深色配色；标签、按钮和对话框由 GPUI 自绘，不是系统控件。
Windows 合并标题栏与标签，新建按钮紧邻标签；最小化、最大化/还原、关闭及空白
拖动区域使用 GPUI 的系统命中区域。连接或确认对话框打开时不激活背后的控件。
其它平台保留系统标题栏。

标签旁的 `+` 打开新建菜单，选择 `Local terminal` 新建本地终端，
选择 `SSH connection...` 打开连接表单。上下键或 Tab 切换菜单项，Enter 确认，
Escape 或点击菜单外取消；菜单打开时文本、粘贴和 IME 不会写入终端或查找框。
Ctrl+Shift+T 仍直接新建本地终端，Ctrl+Shift+N 仍直接打开 SSH 表单。
点击标签或 Ctrl+Tab / Ctrl+Shift+Tab 切换，不重启进程；标签自动滚入视野，
溢出时可横向滚动或使用前后按钮。标签关闭按钮 / Ctrl+Shift+W 请求关闭会话。
目标栏的 `Find output`、`Copy`、`Paste` 直接操作当前终端，退出会话禁用 Paste。
不提供 Quick open、标签重命名、Details、Actions/右键菜单或连接库专用键盘导航。
右键不粘贴，也不向终端发送鼠标协议。目标栏持续显示 local 或 SSH user@host；
标签名称不是实时 cwd，远端目录和 SSH 认证状态不从提示符或输出推测。

标题栏侧栏图标 / Ctrl+Shift+B 折叠连接库，不暂停终端。
默认在窗口宽度小于 1000px 或没有保存目标时收起；手动切换保持本次窗口选择。
连接库只显示保存的 SSH 目标，点击目标连接，Edit 打开表单而不连接。

SSH 在居中模态对话框中突出主机、用户、端口和显示名称，
身份文件与跳板机放在可展开的 Advanced access 中。编辑已配置高级选项的目标时
自动展开；手动收起不丢失字段，并显示实际配置摘要。身份文件支持系统文件选择器，
仅填写路径，不读取或存储私钥内容。新建时使用空白草稿；
保存目标的 Edit 按钮填入表单供修改，不立即建立网络连接。
显示名称为空时使用主机；空白可选字段使用 OpenSSH 配置。`Connect` 只连接一次，
`Save` 仅保存连接元数据，不查找或启动 SSH；Ctrl+Enter 仍可保存并连接。
从保存的目标进入时更新该项，不重复添加。Connect 不改变保存项。
编辑保存目标时可用 Remove 移除元数据，默认保留，显式确认后持久化；
不关闭已有会话或终止远端程序。保存或移除失败时保留原配置并明确报告错误。
保存结果使用可关闭的浮层反馈，不挤压终端或改变其行列数；
打开或切换会话时清除旧反馈，避免和当前连接状态混淆。
终端继续显示原始 SSH 输出与退出码，
连接进程存活不代表认证成功。连接或搜索对话框打开时底层终端输入暂停；
较矮窗口中的字段区可滚动，连接与取消按钮保留在底部。

关闭运行中的会话或含运行会话的窗口会打开模态确认，目标绑定稳定的会话 ID；
已经结束的会话可直接关闭，没有运行会话的窗口直接退出。
确认期间按键、剪贴板粘贴、原生文本提交和 IME 预编辑都不能写入 PTY，也不能
点击背后的会话或按钮。默认选中保留会话：Enter 保留，Escape 始终保留；
Tab 或左右键切换按钮，显式选中关闭后 Enter 才确认，或直接点击关闭按钮。
提示始终说明 Enter 执行当前选择、Escape 保留；选中的关闭按钮使用危险色。
确认后回收 PTY 与本地进程；SSH 远端程序不保证停止。
正常退出使用中性提示，非零退出或意外断开使用错误色，均保留原始输出。
终端输入、绘制及缩放错误归属于对应会话，切换标签不会把错误带到其它会话；
配置读取等应用级错误独立显示。

`Find output` / Ctrl+Shift+F 查找当前终端的屏幕与历史输出。小型查找条覆盖在终端右侧，当前匹配位于
视口上半部时移到底部，否则放在顶部；编辑查询期间不因新输出自动换边，
显式查找/导航时重新定位以避让当前项。不挤压 PTY、不改变行列数。
使用 Alacritty 的网格搜索和坐标，不用拼接字符串反推宽字符位置。
当前为区分大小写的字面文本查找，不启用正则；支持中文/宽字符、终端自动软换行，
不跨硬换行匹配，不提供组合字符规范化。查询最多 256 UTF-8 字节；
每次从最近输出向前搜索至多 250,000 个网格单元，结果最多保留最近 2,000 项。
达到行范围时明确显示 Recent rows / bounded，结果达到上限显示 `2000+`，
不把有界范围内无结果当作全部历史无结果。终端自身仍保留最多 5,000 行历史。
PTY 完成更新的 Wakeup 发布单调版本，布局调整在模型锁内发布版本；查询、
版本、网格尺寸/历史范围或备用屏幕变化才重建匹配。绘制和导航共享缓存，
普通视口滚动、焦点和浮层定位不重新扫描，也不消费其它观察者的更新标记。
位置读取、重建及绘制仍在同一网格锁内，不复用失效坐标。
当前项按完整逻辑行文本及行内匹配次序恢复，不仅比较坐标；追加输出、滚动和可识别的
重排尽量保留原项。旧内容被替换/淘汰、重复行无法唯一识别、范围裁切或切换备用屏幕时，
明确显示 No active match，由 Prev/Next 重新选择，不猜测旧项落到了其它内容。
当前项所在逻辑行继续改写、ConPTY 调整尺寸后重绘产生重复历史时，也采用该保守策略。
查找状态随各自会话保存；切换到另一个会话不复用原查询或结果，
退出的会话仍可查找保留输出，新会话从空查找状态开始。
当前匹配使用金色底色，其余匹配使用次级底色；Prev / Next 或
Shift+Enter / Enter 循环导航，显示当前位置/结果数及 No results。
Tab 在查询、Prev、Next、Esc 之间移动；Esc 关闭查找并返回终端。
输入法预编辑期间不激活导航或切换会话。查找文本、粘贴和导航键不会发往 PTY；
点击终端内容可明确将输入焦点还给终端，查找高亮仍保留到关闭查找。

终端粘贴统一经过安全入口：目标栏 Paste、Ctrl+Shift+V、Shift+Insert
使用同一策略。短单行文本（最多 1,024 UTF-8 字节）
保持快速路径；超过 1,024 字节、含 CR/LF 或除 Tab 外的控制字符时先确认。
粘贴总量超过 1 MiB 时拒绝并明确报错，不截断后偷偷发送。
确认窗显示稳定目标、CR/LF/CRLF 计数的行数及 UTF-8 字节数，
仅在本机展示最多 2,000 字符/10 行预览，控制字符转义并明确标记裁切。
默认选择 Cancel：Enter 取消，Esc 始终取消；Tab/左右键选择后才可用 Enter 确认，
也可明确点击 Paste。Ctrl+Tab 等带修饰键的导航不会改变确认按钮选择。
确认期间键盘、原生文本提交、IME 和其它粘贴入口不写 PTY，不能切换背景会话；
目标退出或请求关闭会丢弃待粘贴内容，取消关闭不会恢复旧粘贴。
发送前再次核对目标仍是同一个活动、运行中的会话，不将待粘贴内容转给其它标签。
确认后沿用终端已有编码：按接收端当前 bracketed-paste 模式包裹内容，
保留原有 ESC 过滤及非 bracketed 模式换行转换，不附加额外 Enter。
Bracketed paste 并不保证所有程序不会执行换行。SSH 字段、查找和其它文本编辑器
的粘贴仍只编辑字段，不被误当作终端命令或打开终端粘贴确认。

终端使用 GPUI 原生字体与绘制，支持 ANSI 颜色、备用屏幕、键盘/Unicode 输入、
窗口缩放、滚动历史、拖动选择、Ctrl+Shift+C/V 复制粘贴及 Ctrl+加号/减号
字体缩放（Ctrl+0 重置）；字号按钮在状态栏。
PTY 协议查询仍在 I/O 路径回复，后台会话不依赖绘制。
SSH 字段支持点击定位、左右/Home/End、Shift 扩选、Backspace/Delete、
Ctrl+A/C/X/V、UTF-16 范围替换及 IME 预编辑。Tab / Shift+Tab 先遍历
主机、用户、端口、显示名称和高级选项开关，仅展开时进入身份文件、Browse、跳板机，
然后进入适用的 Remove、Cancel、Save、Connect 按钮，最后循环回主机字段；
按钮显示焦点边框，主机为空或端口非法时在对应字段旁报告错误。
Enter / Space 执行选中的按钮。字段内 Enter 连接一次，Ctrl+Enter 保存并连接，
Escape 始终返回终端；按钮获得焦点时文本、粘贴与预编辑均不会修改字段或落入背后的 PTY。
这不是完整富文本编辑器，尚不提供单词/字素级导航、撤销或鼠标拖动扩选。

GPUI 设置单独保存在系统用户配置目录的 `nosh/desktop-gpui.json`，不覆盖 egui
配置；保存的连接在连接列表中重新打开。损坏配置禁用保存并报告错误。此实验尚未迁移
egui 的独立 AI 建议面板和主题切换；AI 仍通过真实 nosh 终端显式
调用，不把本地上下文用于 SSH。尚不支持终端鼠标协议上报、图片协议或完整
可访问性语义。此入口只承诺本次 Windows 原生验证，其它平台尚未运行验证。
本机验证保存/编辑流程时可使用 `--settings <path>` 指定独立设置文件，
避免改变日常连接配置；省略时仍使用上述系统配置目录。

```powershell
cargo test -p nosh-desktop --features gpui --locked -- --test-threads=1
cargo clippy -p nosh-desktop --features gpui --lib --bins --locked -- -D warnings
```

GPUI 回归覆盖模态输入隔离、默认保留/显式关闭、表单 Unicode 编辑及大小限制、
查找文本与 SSH/PTY 隔离和输入法候选期间禁止激活；
输出查找覆盖中文软换行/硬换行、字面转义、历史/备用屏幕/重排/擦除、扫描与结果上限；
追加输出保持当前项、同坐标内容替换/历史淘汰/重复行清除选择，
并用实际扫描计数验证未变内容的重复绘制及导航复用缓存；
粘贴覆盖阈值、CR/LF 计数、稳定目标、默认取消、关闭丢弃及所有输入入口隔离。
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
