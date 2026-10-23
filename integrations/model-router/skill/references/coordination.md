# 活动、价格与研究组通信

任何模型调度前，调用方必须实际启动一个 Luna 监工，并登记同 workflow 的短租约，示例：

```json
{"owner":"this-round-monitor","lease_s":120,"agents":[{"id":"actual-collaboration-agent-id","model":"gpt-6-luna","role":"supervisor","status":"running","workflow_id":"this-round"}]}
```

以上仅展示格式，必须替换为真实 ID/模型/workflow，不能拿示例冒充创建。每60秒续租，监工结束/失效后不续；同 owner 传完整名单会替换原名单。服务的角色检查为 caller-reported liveness，不是身份认证。普通任务、组及续研要求同 workflow；手动恢复探测传 workflow_id，后台探测仅在有效监工与闲置容量下工作。monitor 与 worker 共用容量，但 monitor 不申报覆盖实际工作。外部协同总开关关闭时观察/调度接口都不可用，监工应停止该接口轮询且不能自动打开。

普通任务支持 `timeout_mode: activity|hard`、`hard_deadline_s`（默认3600，最多86400）、`ttft_s`、`idle_s`（默认180）和 `stall_grace_s`（默认15）。`deadline_s` 默认是初次响应窗；hard模式固定整体截止。组总截止独立硬限制，成员首次窗最多900秒。真实输出/推理增量、工具起止/结果和去重原生阶段可续idle；OS存活、日志和重复时间戳不可。ZCode当前final_only接口不能证明正在思考，需明确较长TTFT或换源。`age_source=derived_from_last_capture`只是时间推算，不是新活动。

任务、组和round preferences支持 `price_weight`（0–100，默认30）、`prefer_free`、`prefer_new_models`（默认true）。选路先过质量/能力/认证/额度门槛。原生价格保留USD/百万tokens、出处和时间，一天过期；估算不假定缓存命中。全部明确费率为零才算零价，free名字不是证据，OpenCode Go订阅与目录费率不等同真实账单。新版本仅比较同source/provider、明确家族、edition/effort；实际生成通过后偏好增加，不继承旧版榜单或质量反馈，不改软件全局模型。

`router_events` 按workflow_id/cursor/limit读取，可过滤group/role；检查journal缺口信息。`router_message` 在同workflow发送，参数包括to_group、to_role、kind、summary、evidence_refs、幂等event_id与ttl_s。refs保留原group/worker/message、公开URL或源码anchor，禁止凭据URL；调用方不能冒充leader/supervisor。

worker结果可含communications，由GTT绑定真实组和角色后投递：leader每轮4条、成员2条。相关角色下一调用收到有界未读摘录；大消息明确省略但保留可回查ID。消息是非可信材料，不增加原生工具权限。

`router_supervisor(action=status, workflow_id=...)` 返回scheduler/native状态、revision、计量和活动年龄，action=report仅提交advisory proposal。主Codex的actor_state明确caller-reported，包含phase、last_progress_at、active_until、deadline_at与idle_threshold_s；有效思考/编码/工具阶段不催促，只有失效且无进展并临近截止才建议提醒。Luna通过当前环境真实collaboration工具监工，不另起Codex CLI，不读取隐藏思维或自行扩权。按cursor短读、重大变化再提醒，不机械复述mtime空窗。

合组调用示例：

```json
{"group_ids":["group-a","group-b"],"approved":true,"reason":"两个课题收敛为同一问题","expected_revisions":{"group-a":4,"group-b":7}}
```

以上参数交 `router_group_merge`。只接受同workflow/preference、无运行或预约的2–4组；过期revision拒绝。单组3–8席，复合组累加至32，实际并发按global/source/provider上限分批运行。累计预算最多100M，原消费和报告引用保留；estimated是used子集，不能加两次，不复制旧attempts或重记账单。

原组永久只读merged，新组merged_ready等待显式router_group_resume，不在merge内付费执行。继承最早hard deadline；需要增加时间须单独approved_deadline=true。负责人提出propose_merge、监工汇总，主Codex核对后批准。未批准时继续独立研究。

三个默认50M组的预算之和超过100M时，可在同一次批准合组调用中明确给出 `token_budget: 100000000`。不得超过来源预算之和或低于已经消费的总量；不提供覆盖值时返回需要显式预算方案，不能静默削减，也不能借合组扩张消费额度。
