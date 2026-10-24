# 定向工作流与调用配置

0.5.0 起，新计划/提交统一使用 `router_dispatch`，详见[统一受理协议](dispatch-intake.md)。本文出现的旧 workflow 方法名仅作兼容说明，不再向新 MCP 客户端发现。

`router_scheduling(action="get")` 返回来源、路线、生成验证状态、限时池、职责模板、配置预设和生效值；`refresh` 重新扫描目录，不生成内容。`save` 传完整 `value` 与读取到的 `expected_revision`，冲突时重载并合并，不能覆盖别人刚保存的设置。来源目录可见、存在认证提示、近期生成成功分别表述。

`settings.enabled` 是调度总开关。`mode="directed"` 只允许 `pool` 中未到期的路线，按顺序选路。每项为 `route_id`、毫秒 `expires_at` 和标签。质量、恢复隔离、原生容量和共享预算仍有效；过期后阻挡新派发，已发出请求允许完成。

工作流定义包含 `id/name/nodes/edges/token_budget/max_parallel/luna_supervision`。监工默认开启；用户选择关闭时按该选择执行。每个模型节点绑定主 `route_id`，可列出 `fallback_routes`；只在这个有序范围内补位。`role/tags/task/x/y` 保存职责、分类、调用参数和画布位置。模型节点可配置 fanout、verify、max_attempts 等；备用模型由用户明确选定，不能因换源逃出池。

`input/merge/gate/output` 是本地节点：只处理输入或证据引用，不伪造模型报告，不计生成费用。gate 检查结构性完成并要求至少一份模型证据，不判断语义正确性。失败或未知节点阻断依赖，完整报告通过节点 `job_id` 的 `router_collect` 回收。

`router_workflow_plan(definition, workflow_id)` 先检查图与全部模型路线，读取 `effective_task`、调用配置及拒绝原因。`router_workflow_submit` 再传本次输入、稳定请求 ID、工作流 ID；`status/cancel` 使用返回的 `run_id`。同 ID 同内容幂等，同 ID 不同内容拒绝。未知结果必须显式风险确认后才新建运行；重启标 interrupted，不自动恢复付费调用。

`call_profile` 有 `preset/limits/defaults/source_options/parallel/combined_parallel/coordinator_threads/source_parallel/recovery`。可选 inherit、balanced、economy、throughput、deep、compatibility、connectivity；读取返回的 `call_presets` 获取可审查的完整值。节点显式参数覆盖默认值，运行参数仍受活动上限与模型原生容量约束。来源级选项限于已支持的原生参数，不能传凭证、命令或权限绕过配置。

信息不足时报告 `information_requests:[{question,reason}]`；后继信息支撑节点按上下文的准确 ID 返回 `information_support:[{request_id,summary,evidence}]`，逐题绑定。未解决请求继续传播，门禁与输出拒绝验收；不能仅用空白或无关报告视为补齐。
