# nosh

nosh 是一个用纯 Rust 实现、内置本地小模型（默认 MiniCPM5-2B）、可以断网运行的 AI shell。

> 状态：设计已完成，MVP 开发中（见 [MVP 实施计划](docs/MVP-PLAN.md)）。

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

## 文档

- [设计文档](docs/DESIGN.md)
- [MVP 实施计划](docs/MVP-PLAN.md)

## 许可

Apache-2.0