# 扫描来源兼容性核查与适配（2026-10-26）

本轮以 `8daf747` 的干净主工作区为基线，逐项核查 `adapters::registry()` 注册的 **19 个来源**。变更涉及 11 个适配器和一个目录解析辅助文件；保留既有格式、请求去重身份、缓存与输出口径。没有新增生产依赖或修改保存的工作流/模型池。本轮源码随后纳入提交前复核；追加修复与最终验证见 `docs/REVIEW.md` 的 S189。

当前交付为源码及本地构建。**本任务尚未替换正在运行的正式 GUI/CLI，也未重写或回填正式账本。** 本地扫描与实验均使用独立账本。运行中的 GTT 仍按正常流程计入本任务的真实生成用量，此处不作正式账本字节不变的声明。历史已入账的错误及已有字节游标需要在安装时单独规划受控回填；不能把一次新账本验证当作历史数据已修复。

## 证据与范围

- 使用 Exa MCP 开展分来源检索，随后读取官方文档、固定 Git SHA 的上游源码和官方发布物。检索命中用于定位，表中结论按实际阅读的源代码、官方序列化样本及本地计量字段核对。
- 开源仓库固定快照：`openai/codex@970111f6`、`anomalyco/opencode@f669c46c`、`MoonshotAI/kimi-cli@9ab1286b`、`MoonshotAI/kimi-code@1011d8b6`、`MiniMax-AI/minimax-code@da22f11c`、`cline/cline@dab48234`、`xai-org/grok-build@2bdd1d6a`、`deepseek-ai/deepseek-harness@22ddee37`、`CodebuffAI/freebuff@2992f9b0`；Command Code 的公开仓库与官方会话文档同时核查。
- 闭源客户端使用公开说明与本机实际结构作交叉检查。没有对应客户端数据时，以样本验证解析合同；没有将这些项目标为真实用户负载验证。
- 原始聊天和凭证没有发送给辅助模型；下发材料为本项目扫描代码、公开协议、合成样本和受控执行回执。环境变量目录覆盖要求对应变量对 GTT 进程可见。

## 全部来源覆盖表

| 来源 | 当前存储合同与核查结果 | 本轮适配/验证 |
|---|---|---|
| Claude Code | `projects/**/*.jsonl`；流式同 message.id 合并；输入与缓存分开 | 支持 `CLAUDE_CONFIG_DIR`，发现深度覆盖更深 subagents；本机 52 个输入项及回归验证 |
| Codex | `sessions` / `archived_sessions` rollout JSONL；`last_token_usage` 每次用量；独立 usage record 不重复相加 | 支持 `CODEX_HOME`；处理 secondary 配额窗口及其独立变化；本机 28 项及路径/增量验证 |
| Devin | SQLite `sessions.db`，以及 **3000.11.205 原生 v1 文件后端** | 补 `DEVIN_SESSIONS_DIR/cli` 与 Windows 本地目录；读 `sessions/<id>/{meta.json,log.jsonl}`，跨后端同 request_id 去重，无 request_id 时保留旧 SQL fallback；本机 SQL、官方离线导出、12 项单测及两轮端到端验证 |
| Cursor | `state.vscdb/cursorDiskKV` 用户 turn 元数据；本地 tokenCount 常为零 | 保持 **Metadata** 能力，未把文本长度、上下文占用或用户 turn 冒充实际 tokens；本机 1 个库及既有回归 |
| Qoder | 项目与 subagent JSONL，vendor credits 与 usage | 支持 `QODER_CONFIG_DIR`，跨 append 保留 project，保留纯缓存付费记录；本机 16 项（当前样本主要提供 credits）及路径验证 |
| OpenCode | 旧 `session/message` 与新 `session_v2/session_message`；message ID 为计量身份 | 双 schema 共存、保留未迁移旧行、跨库冷扫不清其他 message；迟到完成保留水位；支持 XDG、显式 DB 与渠道 DB；本机旧库、新格式样本及大小写扩展名验证 |
| ZCode | `model_usage` 每个逻辑请求/attempt，运行中记录暂不推进水位 | 保留逐行缓存语义判定、迟到写入与跨原生来源保护；本机 1 个库及既有回归 |
| Grok | `updates.jsonl` turn usage/per-model map | 支持 `GROK_HOME`、`.cwd`；按当前官方 **1e10 ticks/USD** 换算，修复旧 1e9 除数的 10 倍误差；本机 7 项、黄金样本及路径验证 |
| WorkBuddy | JSONL `providerData.rawUsage` 与上下文配额 SQLite | 保留最大缓存别名兼容、真实 response 身份；新增本机字段形状回归：63890 prompt − 63744 cached = 146 非缓存输入；本机 55 项 |
| CodeBuddy IDE | conversation/messages JSON 中请求快照 | 保留原有精确快照替换、同请求去重、回滚和游标保护；本机 85 项及既有回归 |
| CodeBuddy CLI | traces JSON 的已完成 trace 总计；缺维度时保留 unclassified total | 保留 trace 身份去重和维度守恒，不相加 spans/last-call 子计数；本机 16 项、3 个计量 trace 及既有回归。项目 JSONL 目录说明不等于已验证的另一份计费数据源 |
| MiniMax Code | `v2/sqlite/runtime-state.sqlite` 每次调用 usage | 支持 `MINIMAX_DATA_DIR` > `MAVIS_DATA_DIR`；默认目录扫描先确认真实计量库，避免安装目录挡住 legacy；自定义根身份不与同名目录冲突；本机无记录，源码/fixture/路径验证 |
| Kimi Code / Kimi CLI | TypeScript `usage.record` 与 Python WireFile `timestamp/message/StatusUpdate` 是两套合同 | 同时发现 `.kimi-code` 与 `.kimi` / 两个官方覆盖变量；四维 step usage、浮点秒、父子转发去重、未知模型不猜；本机无记录，官方代码、fixture、路径与重复扫描验证 |
| Cline | legacy task UI JSON 和新 SDK `.messages.json` | 真实 request metrics 出现时只删除该 session 旧聚合 fallback；忽略 displayOnly，reasoning 别名取 max；本机无记录，旧/SDK/聚合切换回归 |
| Command Code | 官方当前项目 JSONL/tree session 格式 | 保留已支持结构、header cwd、append 状态与缓存分量；本机无记录，官方文档和既有回归 |
| Gemini/Antigravity | 已注册来源是 Antigravity 的 conversation DB/protobuf；不是新增独立 Gemini CLI 来源 | 保留三类根目录、`GEMINI_CLI_HOME`、response ID 去重及现代 steps 时间恢复；本机 58 项及既有回归 |
| DeepSeek Harness | 最高 generation 的 JSONL/Zstd，assistant usage | 根据当前官方最后一个 tagged `session/end-seed` 排除 fork 继承历史；untagged restore 不改变 cut；本机 6 项、Zstd/append/fork 回归 |
| Freebuff | Desktop 每个 project 的 `desktop-v2.db/metrics_json` | 当前官方公共 TurnMetrics 与既有解析对照；保留当前 usage/cost、foreign harness 排除、未知模型原名；本机无记录，以 fixture 验证 |
| GTT Collaboration | 协调器清洗后的 `usage_records` v1 | 保留 reported/estimated 区别、native identity、跨原生来源去重和未知版本拒绝；本机 1 个状态库的实际记录可读 |

## 关键官方依据

- [Claude 会话目录及目录覆盖](https://code.claude.com/docs/en/sessions)、[Codex rollout 持久化](https://github.com/openai/codex/blob/970111f6cb62cc02fe36a82ea7d71b37a2889c97/codex-rs/rollout/src/recorder.rs)。
- [OpenCode 新数据库 schema](https://github.com/anomalyco/opencode/blob/f669c46c9604ea3d70b8a7ed1f0c10f3a5ac1f61/packages/core/src/session/sql.ts)、[TokenUsage 分量](https://github.com/anomalyco/opencode/blob/f669c46c9604ea3d70b8a7ed1f0c10f3a5ac1f61/packages/schema/src/token-usage.ts)。
- [Kimi CLI 数据目录](https://moonshotai.github.io/kimi-cli/en/configuration/data-locations.html)、[WireFile 实际序列化](https://github.com/MoonshotAI/kimi-cli/blob/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82/src/kimi_cli/wire/file.py)、[独立缓存分量定义](https://github.com/MoonshotAI/kimi-cli/blob/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82/packages/kosong/src/kosong/chat_provider/__init__.py)。
- [MiniMax 数据目录](https://github.com/MiniMax-AI/minimax-code/blob/da22f11c55d4748155d7162c1030414228a00565/packages/config/src/data-dir.ts)、[Cline SDK 持久化](https://github.com/cline/cline/blob/dab482348ffd83d1065dfcf8b538076608925a79/sdk/packages/core/src/services/session-data.ts)。
- [Grok 计费 ledger 单位](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-chat-state/src/usage.rs)、[DSH 继承 cut 语义](https://github.com/deepseek-ai/deepseek-harness/blob/22ddee3792402c3e70ec5f86a63c3e3e895a2f60/docs/persistence-catalog.md)。
- [Devin 3000.11.205 变更说明](https://docs.devin.ai/cli/changelog/stable)、[官方发布物](https://github.com/CognitionAI/devin-cli/releases/tag/3000.11.205)。本轮下载官方 npm 原生包，验证其 registry SHA-512，二进制 SHA-256 为 `b086a1c7cd166961560c86445bf510d7fdc5acc4b75c58f06fb6365b9bfaf1d8`；未安装替换用户的 Devin，未登录、未发起生成。
- [Qoder 官方会话位置](https://docs.qoder.com/cli/sessions)、[CodeBuddy 官方目录说明](https://www.workbuddy.ai/docs/cli/codebuddy-dir)、[Command Code 官方会话说明](https://commandcode.ai/docs/sessions)、[Freebuff 当前计量形状](https://github.com/CodebuffAI/freebuff/blob/2992f9b04db25bfbe020d961e7c06e3eb1c9eedd/common/src/desktop-cloud/turn-metrics.ts)。

## 独立验证结果

- 基线适配器测试：90 项通过。加入 19 项针对格式、计量、迁移与去重的回归后，适配器测试共 109 项。
- 主线 `cargo test -p globaltokentracker-core -p globaltokentracker-cli -p globaltokentracker-ffi --locked`：**347 项通过、4 项按原配置忽略**；core 为 328 passed / 4 ignored，CLI 3 unit + 15 integration，FFI 1。
- `cargo clippy` 对上述三个包全部 targets、`-D warnings` 通过；格式与 diff 检查通过。
- **7 组目录覆盖端到端样本**：Codex、Claude 深层 subagent、Kimi CLI、Qoder 纯缓存、Grok、MiniMax、OpenCode 渠道 DB。新 CLI 的实际发现/解析/入账与已知黄金值一致，第二次扫描计数不变。
- **Devin 官方生成文件的额外端到端验证**：官方 CLI 离线导入 1 个合成 session、3 个 node，其中两个 assistant node 共用 request_id。GTT 同时读取 SQL 和文件后，两次扫描均只保留 1 条请求，`input/output/cache-read/cache-write/duration = 50/20/100/5/1200`，没有重复计量。
- **真实本机负载**：独立账本扫描 19 个来源、328 个输入项，327 项实际扫描，写入或升级 63,916 次，最终 47,942 条去重事件，0 个来源扫描错误，约 48.1 秒。14 个来源有本地输入，5 个来源没有；空来源不构成该客户端的真实负载证明。该扫描在追加 Devin 文件后端支持前完成，后端追加另有官方样本与回归验证。
- 上述结果验证已覆盖合同与样本，不是所有软件版本、所有账户或所有历史账单的准确率百分比。

## 实际工作流与失败保留

使用用户保存的“定向工作流”（读取 revision 35），保留模型池、保存节点与用户配置。本轮按用户明确许可临时调整操作员：先从 max 改 high，再改为现有池内 GLM-5.3-Flash high；Step-5 xhigh 保留。

早期大代码补丁转写发生多次纯思考截断，正文为零；这些调用均未产生文件操作，失败及费用保留。随后改为母节点生成并预验证候选，操作员选择固定登记命令；执行器检验完整 SHA、路径与链接边界后原子写入，并留备份和收据。12 文件主批操作员实际生成消耗 3,658 tokens；本轮全部外部生成含失败累计 **195,962 tokens**，低于累计 200,000 边界。该数字不包含本聊天 Codex 用量和 Exa 服务用量。

原完整图终审节点收到 56.7k 字符回报后因预计无法满足首次响应窗口而被拒绝，0 次生成；没有伪造其完成。用原 Step-5 节点精简补审和 Devin 追加子流程完成了实际回执审核，均 accepted。母节点仍独立承担完整源码、测试和最终验收责任。该任务没有同任务 Codex 单干对照，不能据此给出整体提速百分比。

## 本地复核材料

统一目录：`target/diagnostics/source-compat-20261026/`。

- `before.json`、`task-only.diff`、`current-source-hashes.json`：基线、精确增量与当前源码身份。
- `exa-research-full.json`、`upstream/heads.json`、上游文件与官方 Devin 发布包/离线导出：调查依据。
- `applied-roots.json`、`applied-formats.json`、`applied-devin-files.json` 及 `applied-before/`：真实命令收据和备份；Clippy 的扩展名比较与 Grok 注释另由母节点作最小修正。
- `path_probe.py`、`path-probe-results.json`、`devin-folder-e2e-results.json`：目录及原生文件后端验证。
- `live-isolated-summary.json`、`live-scan.log`：本机负载计量摘要。原始聊天不作为报告发布物。

## 安装与回填边界

正式安装应只更新编译后的 GUI/CLI 扫描组件，保留当前已部署的 Rust 调度内核、Python router、认证、保存的工作流及历史。源码主工作区与已安装 router 的版本并不相同，不能以源码整包安装覆盖当前调度运行时。

安装后对历史计量的处理需要先备份、在副本上重扫核对、再按精确范围回填。尤其 Grok 旧成本与 DSH 已经入账的 fork 继承行，不能仅凭新二进制已经启动就声称旧账本已修复。
