# 0.5.0 接入验收记录（2026-10-24）

本次验证覆盖统一受理层、GTT core/CLI/FFI、两端的工作流调用兼容和运行观察恢复。源码与独立实例验证完成后，用户明确批准正式切换；常驻服务已升级 0.5.0，正式 CLI 和本机 Skill 已同步。保存的调度总开关和过期模型池保持原值；GUI 新修改未打包安装，不能把后端切换当作全部视觉验收。

## 离线证据

- 协调器完整回归：391 项通过，包括 14 项新增受理测试；覆盖 24 个并发同 ID 提交只受理一次、完成后幂等、内容冲突、任务/组/工作流共用入口、保存图 revision、未知结果换 ID 保护、受理中取消、重启后 interrupted 对账、整图永久不兼容预检、全文分页与分支游标。
- Rust 格式、workspace Clippy（`-D warnings`）及 CLI 构建通过。新增 CLI/core 测试覆盖请求文件、稳定 ID、身份互斥、统一握手与旧工作流转换、只恢复当前 workflow 的观察。
- 一次默认并行 workspace 回归出现既有价格 tooltip 测试的语言竞争：断言预期“未计票”，实际正文是英语“not counted”；相关 pages.rs / i18n.rs 未修改，单项重现检查通过。保留原失败日志，另运行不跳过测试的串行完整回归，结果见 GTT 的 REVIEW.md。不能将这次失败从原记录中抹去。
- macOS 修改经源码核对，未在本机编译或运行；视觉与交互验收不从 Windows 测试推出。

## 原生模型负载

使用既有 Antigravity 适配器的 `gemini-3.8-flash-medium`，在独立状态目录启动新协调器，精确单路线、至多两路并发、禁用后台恢复。发给模型的是约 5.6k 字符的受理代码及明确分工，不包含凭证或完整私有仓库。

第一轮两个并行提取节点成功，分别上报 13,113 与 12,218 tokens；原配置的下游 logic_check 被模型目录的 task_kind_not_allowed 拒绝，未发出该节点的原生请求。这暴露了应在上游付费之前拦截下游永久不兼容的问题，已新增整图预检和对应回归。测试脚本还暴露了 CLI 构建文件名误用，已改为在任何生成前解析并验证 CLI 路径。

保留首轮证据，第二轮通过 --resume-evidence 导入这两份完整报告，将成功节点作为本地输入，只补跑模型允许的 summarize 节点；该节点上报 14,639 tokens，工作流成功结束，完整 JSON 报告均可取回。累计三次原生生成共 39,970 tokens，所有 tokens_reserved 为零；成功节点没有再次生成。attempt 的 native_backed 与 actual_model 证明此次走原生适配器，不能仅以顶层 succeeded 代替这一证据。

每轮四个相同请求的并发提交只产生一个 dispatch。真实原生结果只证明所选路线与上述负载，不能推出所有模型、研究组长程负载或大输出上限通过。模型报告仍是候选审阅材料；片段缺失的 cancel、恢复方法使其部分推断缺少全链路上下文，主线以完整源码与竞态测试判断，不把模型投票作为修复依据。

## GTT CLI 与关闭开关

真实构建的 globaltokentracker-cli 对独立服务完成 plan、同 ID submit、status、collect、report、list、cancel；重启独立服务后，同 ID submit 找回原 dispatch，原生 attempt 数仍为 1（第二轮的唯一新生成）。关闭独立实例外协开关后，status/collect/report/list/cancel 继续成功，plan/submit 明确拒绝。

证据保留在被 Git 忽略的 diagnostics/dispatch-live-* 目录：首轮 evidence.json、补跑 evidence.json、cli-reconciliation.json 及计划和请求文件。两个正式配置文件的前后 SHA-256 相同；独立负载阶段未切换正式常驻进程。源码中的原生负载脚本可复现；未知付费节点不支持无确认补跑。

## 用户确认后的正式切换

确认无在途任务后，已备份正式 CLI、Skill、配置和运行库，切换到 GTT 内嵌协调器的 0.5.0 入口。health 读回 contract=2 与 dispatch_v1；正式 CLI release 产物哈希匹配构建候选，scheduling list 成功，现有 MCP 客户端读取设置成功。正式配置 revision=4、enabled=false 与两个配置文件 SHA-256 均保持；146 条历史作业完整保留。

正式 CLI 的有界提交检查返回 scheduling_disabled，受理表与新增作业均为零，未因验收打开调度或发出模型调用。授权备份和验收结果保留于 diagnostics/dispatch-activation-*/activation.json。界面源代码的观察恢复与报告重绘修改不在本次 CLI/服务安装范围，macOS 仍需其平台运行验证。
