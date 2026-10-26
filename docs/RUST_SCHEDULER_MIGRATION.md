# Rust 调度内核与状态运行时迁移

## 0.7.0：持久化与运行状态

第二阶段已将协调器状态数据库的唯一写入权交给常驻 Rust 内核。Python 不再打开该数据库的写连接；两种后端使用同一所有者锁，防止同时接管。旧调用通过私有 SQL 兼容层过渡，作业、运行记录、选路预约、健康记录和事件的高频路径使用原生事务。

2026-10-25 经用户最终确认完成本机切换：正式服务 0.7.0，`state_owner=rust`，MCP 与 Skill 已同步。保留当前开启的调度开关、revision 22、11 条模型池及全部历史；安装后的 CLI 与 MCP 读回通过。记录见 `target/diagnostics/rust-stage2/activation.json` 和 `post-install-verification.json`。

Rust 管理运行记录、持久版本及缓存。Python 仅缓存 Rust 已确认的只读副本，返回调用方前复制；写入必须通过 Rust 的版本校验。兼容 SQL 修改、回滚或版本冲突立即使缓存失效。大报告和节点历史首次传输后按增量更新；内部仍使用 16 MiB 物理帧，超过单帧的完整状态分块传输，逻辑消息上限 256 MiB，不改变公开报告接口的原有容量限制。

作业、用量和进度事件可在同一事务提交；事件容量耗尽时，同事务记录缺口而保留作业及计量。工作流取消与异步提交重新读取最新版本，旧副本不能覆盖取消结果。NaN 等无效原生计量按未知处理，不作为零消费，也不再写入无效 JSON；读取旧历史时保留报告文本，只将裸非有限数值规范化为 null。

选路和资源预约合为一次原生请求，路线目录按内容身份缓存，健康及质量状态直接从 Rust 所有的数据库读取。模型适配器、命令/文件 broker、Codex 分配与证据验收、研究组上层控制仍在 Python，公共 MCP/HTTP 契约继续为 2。

新配置版本 2 要求 Rust 后端，旧协调器会在加载配置时拒绝启动。安装包将配套 Python 运行时放在 GTT 安装目录的 `model-router` 下，并同步 MCP 启动路径；现有 GUI/CLI 继续使用兼容接口。配置、认证、开关、模型池与历史不由迁移重置。

已将已有调度回归纳入主仓库测试目录，修正了监工、步骤数、截止模式、思考等级和手动检测语义的过时夹具。两种后端使用相同语料；测试不再依赖另一仓库或放宽正式门禁。新增持久化专项覆盖事务回滚、提交回复丢失、重启、单一所有者、版本冲突、只读缓存失效及 22 MB 历史记录完整传输。

验证命令与原始证据：

```powershell
# 在 integrations/model-router 中执行
python scripts/run_scheduler_regressions.py --backend python --output <报告绝对路径>
python scripts/run_scheduler_regressions.py --backend rust --kernel <内核绝对路径> --output <报告绝对路径>
python scripts/benchmark_scheduler_state.py --kernel <内核绝对路径> --output <性能报告绝对路径>
python scripts/verify_state_migration.py --source <原数据库> --destination <全新隔离目录> --kernel <内核绝对路径> --output <报告绝对路径>
```

`target/diagnostics/rust-stage2` 保留第一阶段冻结副本、新回归、性能样本和正式历史的只读复制验证。154 条作业、516 条用量记录、6 条受理记录、2 条操作收据在隔离迁移前后内容哈希一致，原库仅以只读方式备份；未使用真实模型生成。

以下为第一阶段的历史记录，最终状态与验证以第二阶段交付清单为准。

## 第一阶段

基准：GTT 主仓库 `b75d9a1`，不是独立协调器仓库的旧副本。实现位于隔离分支 `codex/rust-scheduling-core`；本阶段未合并到正式服务源码、未安装、未重启正式服务、未修改模型池或认证。

## 本阶段实际边界

新增 `globaltokentracker-core::scheduling_kernel` 与常驻私有 stdio 程序 `gtt-scheduler-kernel`，复用已有 serde_json/标准库，未新增生产依赖。

- Rust：完整选路评分与拒绝理由、输出余量、候选顺序、任务预算/探索预算/来源与提供商容量预约决策、工作流依赖就绪和终态判断、原生结果 unknown 分类、回报传播目标计算。
- Python：目录/额度/设置快照采集、唯一持久状态与计量写入、容量锁内原子提交预约增量、模型适配器、命令/文件可信执行器、Codex 审批和证据验收。研究组的上层研究循环也继续保留。
- Rust 模式普通/操作员工作流通过 job 保存、控制决策和执行 Future 完成事件唤醒。30 秒等待是丢失外部状态通知时的保守复查上限；不再以 200/500 毫秒轮询推进。
- Python 后端保留为显式对照和部署前回退选择。选定 Rust 后内核崩溃、超时、协议错误会阻断执行，不自动重启、重放或切回另一后端。

这不是完整 Rust 守护进程：内存运行记录和数据库写入权仍由 Python 统一持有。后续迁移可在行为契约稳定后接管持久化/运行记录所有权，再让 Python 进一步收缩为适配工作进程；不能让两种实现同时维护两份预约账本。

## 协议与一致性

私有协议版本为 1，单次请求/响应上限 16 MiB，单服务复用一个 Rust 子进程，并发请求串行归属响应。EOF 结束子进程；超时覆盖 stdin 写入和响应等待。stdout 使用 64 KiB 缓冲，避免 JSON 小片段变成大量 Windows 管道写入。

公共 MCP/HTTP 契约版本仍为 2。`/health` 追加 `scheduler_backend`、`scheduler_kernel_protocol`、`scheduler_kernel_alive`。旧客户端可忽略新增字段。

Python 向内核传递公开路线元数据和材料的 Unicode 字符数，不传原始提示词、文件内容、运行时凭据或原生命令。健康与质量统计通过两次批量查询取得，避免逐路线重复查询。预约决策在原容量锁与作业锁下应用，尝试数、预算、来源/提供商/scope 计数一次更新。

以下规则继续由既有可信边界保证：

- request_id/指纹、JSON 规范化、原始文件字节 SHA 与 Unicode/行区间语义保留。
- tokens_estimated 是 tokens_used 的子集，不重复相加；本地文件步骤不增加模型消费。
- unknown 结果不自动重放；已知未落盘操作才允许原有的明确 retry_operations。
- 回报线不授权写入；reviewer 等真实收据，修正和验收仍绑定 Codex 决策及当前证据摘要。
- 开关、路线池、思考程度、监工、额度、上下文、超时和取消门禁不放宽。

## 隔离构建与验证

```powershell
cargo build --release --locked -p globaltokentracker-core --bin gtt-scheduler-kernel
cargo test --workspace --locked -- --test-threads=1
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check

# 在 integrations/model-router 中执行；也可用 GTT_TEST_KERNEL 指定程序。
python -W error::ResourceWarning -m unittest discover -s tests -p test_scheduler_kernel.py -v
python scripts/benchmark_scheduler_kernel.py --kernel <内核绝对路径> --samples 50 --output <结果绝对路径>
```

专项包含 160 组固定时间的随机选路对照、40 组预约预算/容量/历史用量对照、零字符计量历史、并发协议归属、原生 unknown 分类、四节点真实服务任务链、中文/Emoji/CRLF 文件行切片与实际落盘、审阅及 Codex 验收、内核死亡后零调用和零自动重启，以及独立 HTTP 守护进程加载 Rust 后端。模型适配器使用夹具，文件与 SQLite 为真实临时文件。

现有主仓库原样 Python 回归存在监工夹具及原生权限夹具等失败：基准 110 项、6 failures/27 errors；本阶段加入 9 项专项后 119 项，仍为同一组 6 failures/27 errors。不得把这些结果称为全绿，也不通过降低正式门禁修复夹具。

另外用独立仓库的 421 项旧回归作为兼容语料，但始终加载当前 GTT 工作树产品代码。仅补齐旧定义夹具必需的 effort，两后端均 417 项通过、1 failure/3 errors，失败名称一致：三个仍预期手动检测受自动恢复预算限制；另一个预期执行路线 ID 不随新增节点 effort 覆盖而改变。当前主仓库已修改这两类行为。原始失败和夹具适配脚本保留在 `target/diagnostics/rust-scheduler/`。

## 性能证据

`scripts/benchmark_scheduler_kernel.py` 对 32/147/512 路线执行成对轮换采样，Rust 数据包括批量快照、Python 元数据整理、JSON 编解码、IPC 和结果转换。另设批量快照的 Python 对照，以区分数据访问优化与语言本身的收益。

四节点链包含真实服务、预算预约、临时 SQLite 和 Python 合成适配器。原生模型调用数为零，不代表真实模型生成、真实 CLI 冷启动或 GUI 端到端延迟。最终采样和一致性清单见 `target/diagnostics/rust-scheduler/benchmark-final.json` 与 `delivery-verification.json`。首轮/中途数据也保留，最终表以成对复测为准。

本机最终采样（路线每组 50 次、交替轮换后端；任务链每后端 8 次）：

| 场景 | Python 原版均值 | Rust 桥接均值 | 耗时下降 | 原版耗时 / 新耗时 |
|---|---:|---:|---:|---:|
| 32 路线 | 2.926 ms | 2.227 ms | 23.9% | 1.31 |
| 147 路线 | 13.038 ms | 8.549 ms | 34.4% | 1.53 |
| 512 路线 | 59.284 ms | 35.048 ms | 40.9% | 1.69 |
| 四节点合成任务链 | 2054.213 ms | 99.339 ms | 95.2% | 20.68 |

147 路线的 P95 为 15.439 → 9.731 ms。批量快照 Python 的同组均值为 5.908 ms，仍快于当前 Rust 桥接；因此不能把原版到 Rust 的全部收益归因于语言。任务链的主要收益来自事件唤醒，单独选路仍有跨语言准备、序列化与 IPC 的开销。

## 正式切换边界

默认仍使用 Python；Rust 必须在隔离配置中明确选择：

```json
{
  "scheduler_kernel": {
    "backend": "rust",
    "executable": "C:\\absolute\\path\\gtt-scheduler-kernel.exe",
    "timeout_s": 10
  }
}
```

本阶段仅产出内核可执行程序和协调器源码，模型调度器原本就独立部署，未更改现有 GUI 安装器载荷。正式切换必须先取得用户确认，再在无在途工作时备份配置、状态数据库及程序，整套部署对应的 Python 桥和 Rust 二进制，启动后核对 health 和历史记录。需要回退时同样先清空在途工作，再明确选择 Python；不得在未知付费结果或执行中途自动换后端。
