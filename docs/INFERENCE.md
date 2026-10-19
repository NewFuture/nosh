# 本地推理与 CUDA

默认模型按[系统设计 §2.1](DESIGN.md#21-模型与格式)在加载前根据可用主机内存或可用单卡 CUDA 显存选择 2B Q8、2B Q4 或 Qwen3.5-0.8B Q8。必要检测失败，或两个可用资源均低于 1.5 GiB 且总物理内存低于 4 GiB 时，暂停自动安装和加载；不相加内存和显存，也不合并多卡容量。型号选择与下面的设备/上下文预算是两层检查。显式模型 ID 或路径保持优先，不会在加载后自动更换模型。

默认设备为 **`auto`**：根据构建能力、可见 GPU 和当前可用显存，在加载时选择 GPU 或 CPU。普通构建仍是 CPU-only，不需要 CUDA；使用 GPU 需在 Linux/WSL 上有 NVIDIA 驱动与 CUDA toolkit（`nvcc`、头文件及 cuBLAS 等运行库），并显式构建 CUDA 版本：

```bash
export PATH=/usr/local/cuda/bin:$PATH
export LD_LIBRARY_PATH=/usr/local/cuda/lib64${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
cargo build --release --locked -p nosh-cli --features cuda
./target/release/nosh --offline doctor
./target/release/nosh debug gen "Explain a shell pipeline." --temp 0
# 显式禁用 top-k，其他采样参数不变
./target/release/nosh debug gen "Explain a shell pipeline." --top-k 0
./target/release/nosh -s "列出当前目录"
# 显式覆盖自动选择
CUDA_VISIBLE_DEVICES=0 ./target/release/nosh --device cuda -s "列出当前目录"
./target/release/nosh --device cpu -s "列出当前目录"
```

`--device auto|cpu|cuda|cuda:N` 对交互 shell、Agent、CommandAssist、debug 和 doctor 一致生效，覆盖 `[model] device = "auto"`（默认）。型号确定后的 `auto` 通过 CUDA API 检查设备及空闲显存，根据实际架构、GGUF 权重、上下文、prefill 块和临时张量估算预算，并保留至少 512 MiB 余量；在满足预算的 GPU 中选择空闲显存最多的一张，相同则取较低逻辑序号。该设备阶段在没有 CUDA 构建支持、驱动／设备不可用、显存不足或选择 f32 KV 时可使用 CPU，并报告原因；未指定型号时，必要资源探测失败会先按默认选型规则暂停自动路径。它依据显存快照，不是 GPU 利用率调度器，不保证共享 GPU 没有计算负载。

显式 `cpu` 强制 CPU；显式 `cuda`／`cuda:N` 强制 GPU，初始化或加载失败会报错，**不会退回 CPU**。`N` 是 `CUDA_VISIBLE_DEVICES` 筛选后的逻辑序号：只暴露物理卡 1 时仍用 `cuda:0`。自动选择在初始化 CUDA 库后再次核对空闲显存，但不预留显存；之后若其他进程抢占导致加载失败，明确报错，不用宽泛重试掩盖模型损坏、shape 或算子错误。选择只在加载时进行，不迁移正在运行的模型。CUDA 版依赖的动态库必须能被系统加载；若动态链接器在进程启动前报缺库，程序无法回退，应使用 CPU-only 产物。

量化 GGUF 权重、embedding、RoPE、GQA、KV cache 和前馈层均在单张 GPU 上，Qwen 的卷积/循环状态和更新也留在 GPU，跳过 CPU prepack；分词、调度和采样留在 CPU。GPU attention KV 当前**仅开放默认 f16**，使用张量拼接，未实现 FlashAttention、多 GPU 或 Metal。Llama 支持 KV 前缀截短，Qwen 的回退规则见下文。`--device cuda --kv f32` 会在权重／分词器加载前明确报错，不会偷偷改用 f16 或 CPU；CPU 的 f32 KV 不变。不同后端的量化内核会有数值差异，固定 seed 不保证 CPU/GPU 输出逐字相同。debug、加载说明与原生评估 trace 会报告实际设备；RSS 仅为主机内存，显存和利用率请用 `nvidia-smi` 观测。无法观测的 RSS 显示 unavailable，不填零。CUDA 版本产物依赖匹配的 NVIDIA 动态库，默认 CPU 产物不受影响。

Qwen 的 GPU GQA 将每次 prefill 的 attention 计算再按最多 32 个 query token 分块，scores、causal mask 和 softmax probabilities 只覆盖当前 query 块及截至该块末尾的 KV，不为整个 prefill 块同时保留完整注意力矩阵。按 KV head 分组计算，不重复展开 K/V；KV 展宽与累加仍为 f32。MiniCPM5 保留原有 query 布局，不因 Qwen 优化改变其数值验收路径。此内部工作区分块不改变 prefill 配置、上下文长度、KV 精度或缓存截短语义；自动设备预算仍保留原有保守余量，不以一次实测峰值保证显卡容量。

GPU f32 KV 不满足当前开放要求，不通过放宽数值门槛启用。CPU 与 CUDA 使用不同量化内核，比较时必须区分 dtype、激活量化和设备；验收条件以[真实模型测试](../crates/nosh-llm/tests/real_model.rs)为准。

没有 GPU 的构建主机可以通过 `CUDA_COMPUTE_CAP=80` 等显式目标架构编译、类型检查和链接 CUDA 产物；应使用与 host compiler 相容的 toolkit，不关闭 NVIDIA 的编译器版本检查。这只是构建验证，不是 GPU 执行证据。SDK 中的 driver stub 只能用于链接，不能放入运行时库路径冒充实际驱动；真实数值、取消、会话和显存验收仍需可用的 NVIDIA 驱动及 GPU。

首次使用 CUDA 内核可能触发驱动 PTX/JIT 编译，首 token 明显更慢；后续新进程可能复用驱动缓存。测速必须区分首次 JIT 冷启动、驱动缓存已暖的新进程和同一进程的 KV 复用，不能统称“冷启动”。`NOSH_PROFILE` 的 CUDA 分算子计时仅反映异步提交开销；debug 的整体 prefill/decode 计时会等待 GPU 完成。

评估器默认显式选择 CPU，不继承宿主的模型配置。GPU 评估传 `python3 -m eval --device cuda ...`；验证自动选择传 `--device auto`，逐 trial 记录实际设备和选择原因，见[评估说明](../eval/README.md)。测量结果保存为带构建、硬件和运行条件的独立产物，不在此维护旧构建吞吐表。

采样器热点可单独测量：`cargo test --profile fastrel -p nosh-llm sampling::tests::sampling_top_k_throughput -- --ignored --exact --nocapture --test-threads=1`。这是固定词表与 logits 的微基准，不能当作模型推理或整轮 eval 的加速倍数；端到端对照仍需固定模型、输入、设备和生命周期。

## Qwen3.5 文本模型

`qwen3.5-0.8b:q8_0` 原生读取 registry 固定的 `qwen35` GGUF 和官方 tokenizer。24 层文本主干包含 18 层 Gated DeltaNet 和 6 层 gated GQA；固定文件的 block_count 为 25，另外一层 NextN/MTP 不执行。加载校验矩阵形状、norm、卷积/循环维度、部分 RoPE 与 tokenizer 对应关系，采样屏蔽 padded vocabulary 行，不把 `hidden / heads` 当成其真实 head dimension。

```text
nosh model pull qwen3.5-0.8b:q8_0
nosh --offline --model qwen3.5-0.8b:q8_0 --device cpu debug gen "Say hello." --temp 0
nosh --offline --model qwen3.5-0.8b:q8_0 --device cpu debug gen "Explain a pipe." --kv f32
nosh --offline --model qwen3.5-0.8b:q8_0 --device cuda debug gen "Say hello." --tools
```

CPU 的卷积/Gated DeltaNet 状态固定为 f32，attention KV 可用 f16/f32；CPU attention 复用已有的有界增长 KV store/GQA，递推使用与量化矩阵相同的 Candle barrier pool，不为每个 token 保存快照。CUDA 路径使用设备驻留的 f32 循环状态和 f16 attention KV。CUDA 运行、数值与状态验收必须在有驱动、toolkit 和 GPU 的环境显式执行；无 GPU 主机上的编译、链接或 CPU 张量参考不代替实机证据。

固定 GGUF 没有独立 `output.weight` 时，CUDA embedding 与输出投影共享同一份不可变的量化存储，不重复读取/上传 tied 权重；显式输出权重仍独立校验和加载。CPU 仍保留独立输出矩阵，以便预打包后释放其原始块而不破坏 embedding 查表。归一化复用 Candle 融合 RMSNorm，输入布局需要时先连续化；不降低循环状态精度。设备选择的显存预算保持保守，不用一次采样峰值代替容量保证。

只有完整活动状态所对应的 token 前缀可以直接续接。rewind、compact 或切换会话若要求截到更早位置，Qwen 清空全部 attention/卷积/循环状态并从日志重新 prefill，cached tokens 如实为零；Llama 的局部 KV 截短不变。取消时 prompt tokens 只统计实际完成的 prefill chunk，不把尚未处理的输入算进吞吐。失败的 forward 使活动缓存失效，不能继续使用更新到一半的层状态。GGUF 元数据和张量读取使用缓冲 I/O，避免大词表在 WSL/挂载文件系统上产生大量细粒度读操作。

Qwen 的无 BOS 文本角色、思考开关和工具帧遵循固定官方模板，工具说明文字为 nosh 定制。后续动态 System 背景作为 user-role 的不可信 context 数据，而非追加官方不支持的 system role；UserAnswer 仍按工具回复渲染，并保持引擎内的独立来源。较早用户回合的 reasoning 按控制 token 边界省略，工具参数内的字面内容不被当作 reasoning 删除。

采样沿用 nosh 的现行参数、工具温度与重复检测，不声称完整复现官方推荐配置，不增加 presence penalty。小模型的指令遵循和任务成功率必须看独立原生报告。

## Qwen 数值验收

`qwen35_real_model` 是 opt-in 测试目标；普通 CI 不下载模型。`NOSH_MODEL_PATH` 指向包含固定 GGUF 和 tokenizer 的目录，测试会校验两者 SHA-256。固定参考还核对发行包及 DLL 哈希、KV/循环状态精度、prefill 块和关闭 FlashAttention 的条件，不只核对 logits 文件哈希。`NOSH_QWEN_DEVICE` 可显式选择 cpu/cuda:N；GPU 缺失不能跳过后计作通过。generation、chunk、会话、KV/预打包和任务路径质量是独立检查，质量失败不能改判成引擎成功。

固定输入使用 `tests/fixtures/model_context.txt` 的 3300/8065 个 prompt token，加 48 个 teacher-forced token。共享 `tests/support/divergence.rs` 的 cosine、KL、NLL、top-5 和 confident top-1 门槛与 MiniCPM5 相同，不以生成文字一致代替数值比较。

参考导出仅用于开发验证：使用 llama.cpp b11706 的 Windows x64 CPU release，不是生产推理依赖。必须同时提供原始 ZIP 与解压目录；`tools/qwen35-reference.py` 在加载任何 DLL 前校验固定归档 SHA-256，再逐项核对目录内全部 DLL 与归档内容，缺失、额外或改写的库均拒绝。manifest 记录实际验证得到的归档及完整 DLL 哈希，验收同时核对固定 DLL 清单，不接受旧的仅校验 `llama.dll` 的来源记录。先显式生成不可覆盖的参考输入，再导出对应 logits/manifest：

```powershell
$env:NOSH_MODEL_PATH = "VERIFIED_QWEN_MODEL_DIR"
$env:NOSH_QWEN_REFERENCE = "NEW_REFERENCE_DIR"
cargo test --profile fastrel -p nosh-llm --test qwen35_real_model --locked qwen35_export_reference_inputs -- --ignored --exact --nocapture --test-threads=1
python tools\qwen35-reference.py --archive VERIFIED_B11706_CPU_ZIP --bin VERIFIED_B11706_CPU_DIR --model "$env:NOSH_MODEL_PATH\Qwen3.5-0.8B-Q8_0.gguf" "$env:NOSH_QWEN_REFERENCE\3300.json" "$env:NOSH_QWEN_REFERENCE\8065.json"
cargo test --profile fastrel -p nosh-llm --test qwen35_real_model --locked qwen35_matches_pinned_reference_at_3k_and_8k -- --ignored --exact --nocapture --test-threads=1
cargo test --profile fastrel -p nosh-llm --test qwen35_real_model --locked qwen35_kv_and_prepack_match_at_3k_and_8k -- --ignored --exact --nocapture --test-threads=1
```

在 Linux/WSL CUDA 构建中加 `--features cuda`，并用 `NOSH_QWEN_DEVICE=cuda:0` 执行同一参考比较；KV/预打包控制用例本身明确是 CPU-only。参考模型、输入、logits、库与构建来源必须可追溯，旧参考/报告不覆写。Jinja golden 由 `gen_template_fixtures.py --chat-format qwen35 --template PINNED_JINJA` 生成，校验官方模板哈希；不依赖运行时 Python/Jinja。

### CUDA 实机验收

在 Linux/WSL 的已授权单卡环境中，复用上面校验过的模型与参考产物。先确认驱动确实可用，再显式运行 GPU 算子和同一真实模型测试目标，不把缺卡跳过或 CPU 回退计作通过：

```bash
nvidia-smi
export CUDA_VISIBLE_DEVICES=0
export NOSH_MODEL_PATH=VERIFIED_QWEN_MODEL_DIR
export NOSH_QWEN_REFERENCE=VERIFIED_REFERENCE_DIR
export NOSH_QWEN_DEVICE=cuda:0
cargo test --profile fastrel -p nosh-llm --features cuda --lib --locked \
  model::qwen35::tests::cuda_recurrent_outputs_and_state_match_scalar_across_chunks \
  -- --ignored --exact --nocapture --test-threads=1
cargo test --profile fastrel -p nosh-llm --features cuda --test qwen35_real_model --locked \
  -- --ignored --nocapture --test-threads=1
cargo build --profile fastrel -p nosh-cli --features cuda --locked
python3 -m eval --suite smoke --device cuda:0 --binary target/fastrel/nosh \
  --model-path "$NOSH_MODEL_PATH" --output NEW_GPU_SMOKE_DIR
```

真实模型目标中，分块、参考 logits、生成和会话生命周期按 `NOSH_QWEN_DEVICE` 在 GPU 执行；明确标注 CPU-only 的 f32 KV/预打包控制仍在 CPU 执行，不能混计为 GPU 用例。固定构建及运行条件下另存加载、TTFT、prefill/decode、峰值显存、驱动/GPU 型号与 JIT 冷暖记录；RSS 不替代显存。模型任务失败与引擎/数值失败分别保留，不改请求、裁判或门槛。
