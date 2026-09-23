# nosh

nosh 是一个用纯 Rust 实现、内置本地小模型（默认 MiniCPM5-2B）、可以断网运行的 AI shell。

> 状态：Linux 上的 MVP 已完成，结果见 [MVP 报告](docs/MVP-REPORT.md)。

- **本身就是 shell**：兼容 Bash（内核为 brush-core）。合法的命令照常执行；以 `#` 开头或者命令出错时，交给 AI 处理。
- **上下文连续**：agent 和用户共用同一个 shell 会话，cwd、变量、venv 等状态会一直延续。
- **本地推理**：基于 candle + GGUF。首次使用时自动下载模型，之后可以完全离线。
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

## 文档

- [设计文档](docs/DESIGN.md)
- [MVP 实施计划](docs/MVP-PLAN.md)
- [MVP 报告](docs/MVP-REPORT.md)

## 许可

Apache-2.0