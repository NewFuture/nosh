# 本地推理与 CUDA

默认设备为 **`auto`**：根据构建能力、可见 GPU 和当前可用显存，在加载时选择 GPU 或 CPU。普通构建仍是 CPU-only，不需要 CUDA；使用 GPU 需在 Linux/WSL 上有 NVIDIA 驱动与 CUDA toolkit（`nvcc`、头文件及 cuBLAS 等运行库），并显式构建 CUDA 版本：

```bash
export PATH=/usr/local/cuda/bin:$PATH
export LD_LIBRARY_PATH=/usr/local/cuda/lib64${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
cargo build --release --locked -p nosh-cli --features cuda
./target/release/nosh --offline doctor
./target/release/nosh debug gen "Explain a shell pipeline." --temp 0
./target/release/nosh -s "列出当前目录"
# 显式覆盖自动选择
CUDA_VISIBLE_DEVICES=0 ./target/release/nosh --device cuda -s "列出当前目录"
./target/release/nosh --device cpu -s "列出当前目录"
```

`--device auto|cpu|cuda|cuda:N` 对交互 shell、Agent、CommandAssist、debug 和 doctor 一致生效，覆盖 `[model] device = "auto"`（默认）。`auto` 通过 CUDA API 检查设备及空闲显存，根据实际 GGUF 权重、配置上下文、prefill 块和 attention/KV 临时张量估算预算，并保留至少 512 MiB 余量；在满足预算的 GPU 中选择空闲显存最多的一张，相同则取较低逻辑序号。没有 CUDA 构建支持、驱动／设备不可用、显存不足或选择 f32 KV 时使用 CPU，并在诊断／引擎状态／原生 trace 中报告原因。它依据显存快照，不是 GPU 利用率调度器，不保证共享 GPU 没有计算负载。

显式 `cpu` 强制 CPU；显式 `cuda`／`cuda:N` 强制 GPU，初始化或加载失败会报错，**不会退回 CPU**。`N` 是 `CUDA_VISIBLE_DEVICES` 筛选后的逻辑序号：只暴露物理卡 1 时仍用 `cuda:0`。自动选择在初始化 CUDA 库后再次核对空闲显存，但不预留显存；之后若其他进程抢占导致加载失败，明确报错，不用宽泛重试掩盖模型损坏、shape 或算子错误。选择只在加载时进行，不迁移正在运行的模型。CUDA 版依赖的动态库必须能被系统加载；若动态链接器在进程启动前报缺库，程序无法回退，应使用 CPU-only 产物。

量化 GGUF 权重、embedding、RoPE、GQA、KV cache 和前馈层均在单张 GPU 上，跳过 CPU prepack；分词、调度和采样留在 CPU。GPU KV 当前**仅开放默认 f16**，支持分块 prefill 和前缀回退，但使用张量拼接，未实现 FlashAttention、多 GPU 或 Metal。`--device cuda --kv f32` 会在权重／分词器加载前明确报错，不会偷偷改用 f16 或 CPU；CPU 的 f32 KV 不变。不同后端的量化内核会有数值差异，固定 seed 不保证 CPU/GPU 输出逐字相同。debug、加载说明与原生评估 trace 会报告实际设备；debug 的 RSS 仅为主机内存，显存和利用率请用 `nvidia-smi` 观测。CUDA 版本产物依赖匹配的 NVIDIA 动态库，默认 CPU 产物不受影响。

GPU f32 KV 不满足当前开放要求，不通过放宽数值门槛启用。CPU 与 CUDA 使用不同量化内核，比较时必须区分 dtype、激活量化和设备；验收条件以[真实模型测试](../crates/nosh-llm/tests/real_model.rs)为准。

首次使用 CUDA 内核可能触发驱动 PTX/JIT 编译，首 token 明显更慢；后续新进程可能复用驱动缓存。测速必须区分首次 JIT 冷启动、驱动缓存已暖的新进程和同一进程的 KV 复用，不能统称“冷启动”。`NOSH_PROFILE` 的 CUDA 分算子计时仅反映异步提交开销；debug 的整体 prefill/decode 计时会等待 GPU 完成。

评估器默认显式选择 CPU，不继承宿主的模型配置。GPU 评估传 `python3 -m eval --device cuda ...`；验证自动选择传 `--device auto`，逐 trial 记录实际设备和选择原因，见[评估说明](../eval/README.md)。测量结果保存为带构建、硬件和运行条件的独立产物，不在此维护旧构建吞吐表。
