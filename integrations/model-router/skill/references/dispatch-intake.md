# 统一受理协议

`router_dispatch(action="plan", request=...)` 仅预检；`action="submit"` 同时提供稳定 `request_id`。普通任务为 `{"kind":"task","task":{...}}`，研究组为 `{"kind":"group","request":{...}}`，工作流为 `{"kind":"workflow","definition":{...},"input_text":"...","workflow_id":"..."}`。执行已保存流水线时，以 `saved_workflow_id` 代替 definition，可带 expected_revision 拒绝旧配置。

收到 dispatch_id 后，用 status / collect / cancel 查询和控制。也可凭原 request_id 查询；两个身份只给一个。list 可按 workflow_id 筛选，limit 为 1..64，cursor 从 0 开始。collect 返回 next_cursor；工作流尚未完成的前置分支不会被跳过。已完成不代表内容正确，仍需独立核验。

大报告的 collect 可能只返回摘录。调用 report 指定 worker；工作流还需 node_id。offset 从 0 开始，max_chars 为 2000..30000。串接各页 report_json，直到 has_more=false，再核对 SHA-256 与完整 JSON。此操作读取已有报告，不发新模型请求。

同一 ID 的重试永久指向原受理；不同内容复用同一 ID 报冲突。崩溃中的 admitting 标 unknown，既有任务按持久目标核对；没有证据不得自动重放。相同任务已存在未知付费结果时，更换 ID 也不能重放；确实接受风险才显式 acknowledge_unknown_risk=true。这个参数不绕过路线隔离、开关、预算或监工。

旧六个计划/提交工具不再向新客户端发现，但缓存调用转入同一受理层。无 request_id 的兼容调用以内容生成稳定身份；明确需要重新执行时使用新入口和新的显式 ID。旧报告查询和研究组审批/续研接口保留，避免破坏历史报告和人工审批。
