# 文档导航

从[项目 README](../README.md)构建和运行，从[代码架构与维护](ARCHITECTURE.md)进入开发。文档和测试只维护当前版本，不保留旧接口说明、历史报告或未交付方案。

## 当前设计与开发

| 文档 | 适用问题 |
|---|---|
| [代码架构与维护](ARCHITECTURE.md) | crate 依赖、接口边界、代码和测试位置、开发入口 |
| [系统设计](DESIGN.md) | 当前行为、实现边界和[配置](DESIGN.md#11-配置) |
| [源码维护](REEDLINE-MAINTENANCE.md) | Reedline / brush 修补源准备、联调、补丁导出与升级 |
| [本地推理与 CUDA](INFERENCE.md) | CPU / GPU 构建、设备选择、运行限制和测量要求 |

## 功能契约

| 文档 | 内容 |
|---|---|
| [审批模式](APPROVAL-MODES.md) | Confirm / Auto / YOLO、用户规则优先级与安全边界 |
| [输入编辑](INPUT-EDITING.md) | Emacs / Vi、改键、搜索、取消和建议采用 |
| [命令与参数补全](COMPLETION.md) | 内置与脚本补全、候选语义、后台执行和资源限制 |
| [实时输入辅助](INPUT-ASSIST.md) | 高亮、语法与命令判定、快照、缓存和 worker |
| [提示符状态行](STATUS-BAR.md) | 布局、主题、终端所有权、兼容性与已知限制 |
| [项目上下文](PROJECT-CONTEXT.md) | 项目识别、AGENTS / README 加载与缓存边界 |
| [LLM 工具](LLM-TOOLS.md) | 工具集合、读取与搜索、提问、权限和结果 |
| [CommandAssist](COMMAND-ASSIST.md) | Generate / Fix / Next、查询工具、直接终态与后台调度 |
| [输出采集](OUTPUT-CAPTURE.md) | 最近命令输出、注入条件、PTY 协议、预算与隐私 |

## 测试与评测

| 文档 | 定位 |
|---|---|
| [评测运行器](../eval/README.md) | 当前场景、运行命令、指标、报告与版本化基线规则 |
| [测试组织](ARCHITECTURE.md#代码与测试组织) | 当前单元、跨 crate、PTY 和模型测试的维护位置 |

维护时只更新负责该主题的来源文档，其他页面链接过去。历史材料由 Git 保存；运行成绩作为独立产物记录，不维护在架构文档中，也不把未测量的能力写成已通过。
