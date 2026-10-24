# GTT 正式调度接入

0.5.0 以 `router_dispatch` 集中承接普通任务、研究组和可视流水线。它负责持久请求身份、目标关联、查询、分页回收和取消；执行器仍共用 RouterService 的模型选择、队列、容量预约与用量计量。研究组的审批、续研和成员交接保留其原有语义，不另起一套模型执行器。

## 入口与能力握手

常驻服务在 `/health` 声明 `runtime_contract_version=2` 和 `dispatch_v1`。Python 客户端和 GTT Rust 客户端均先握手；旧服务缺少能力时明确拒绝，不尝试旧执行路径。Windows 界面和 macOS FFI 的旧工作流调用由 Rust core 转为统一请求；原生 CLI 提供 `scheduling plan|submit|status|collect|report|cancel|list`。请求材料从 UTF-8 文件读取，命令行参数只携带标识和控制值。

MCP 不再发现旧的 `router_plan/submit`、`router_group_plan/submit`、`router_workflow_plan/submit`。缓存客户端仍可调用，服务器兼容适配器全部转入统一受理层；没有独立提交策略。旧状态查询和研究组的预算、席位、续研、终审接口保留，历史作业无需搬迁或重放。

## 请求示例

保存为本机请求文件：

```json
{
  "kind": "workflow",
  "saved_workflow_id": "workflow-example",
  "expected_revision": 5,
  "input_text": "本次需要处理的有界材料",
  "workflow_id": "review-round-1"
}
```

```powershell
globaltokentracker-cli scheduling plan --request-file 'D:\private\request.json'
globaltokentracker-cli scheduling submit --request-file 'D:\private\request.json' --request-id 'review:round:1'
globaltokentracker-cli scheduling status --request-id 'review:round:1'
globaltokentracker-cli scheduling collect --request-id 'review:round:1' --cursor 0
globaltokentracker-cli scheduling report --request-id 'review:round:1' --node-id 'node-1' --worker 0 --offset 0
globaltokentracker-cli scheduling cancel --request-id 'review:round:1'
```

MCP / `router.py call` 的参数为 `{"action":"submit","request_id":"review:round:1","request":上述请求}`。普通任务的 request 为 `{"kind":"task","task":{...}}`；研究组为 `{"kind":"group","request":{...}}`。三个类型均返回 dispatch_id、target_id、kind、state，并保留对应 job_id / group_id / run_id。

## 持久身份、取消与恢复

受理层先把预分配目标 ID 和请求内容指纹落盘，再调用执行器。同 ID、同内容的并发、完成后和重启后重试复用原任务；同 ID 改内容拒绝。运行对象的实际 ID 在落盘前已分配，响应丢失后仍可凭 request_id 找回。异常且不能证明拒绝时保持 unknown；相同内容换 ID 也须明确接受未知付费风险。工作流的执行指纹与控制参数指纹分开：改预算/监工不能逃出未知保护，同 ID 改控制参数也不会被静默去重。

取消可在目录查询或实际受理期间发起：短期取消事件传入任务/研究组/工作流，发出前重验；未落盘即取消时不建立付费目标。已有目标只取消自身进程树并保留实际用量。服务重启会将未完成作业标 interrupted、未决工作流节点标 unknown；客户端仅恢复观察，不自动 submit。

外协关闭时，`/rpc` 不再接收新工作。认证 `/manage` 仅开放配置管理以及 status/collect/report/list/cancel，用于回收存量；计划与提交不能借管理端点执行。调度总开关、来源开关、池有效期、质量、额度和真实监工门禁保持原规则。

## 结果与预算

collect 返回有界结果和 next_cursor，工作流按稳定节点顺序读取，不越过尚未完成的分支。大报告以 report 按字符 offset 返回原始 JSON 分片、next_offset、has_more、total_chars 和 SHA-256；调用方拼接并验证后才能称为全文。查询只读取原报告，不产生模型调用，也不重复入账。

流水线总预算纳入明确用量、估算用量和剩余预约，避免未知原生消费漏算或已消费额度重复占位。整图提交前检查所有节点的永久不兼容（任务类型、能力、质量、认证提示、来源与定向选择）；下游已知不可能执行时，在任何付费上游发出前拒绝。暂时冷却、ETA 与容量等待仍在运行时处理，不通过预检放松这些门槛。

## 验证边界

离线回归覆盖并发幂等、已保存图的 revision、协议握手、关闭开关后的对账和取消、受理中取消、未知结果防重放、完整报告分页、分支游标及整图预检。真实负载脚本为 `scripts/dispatch_live_probe.py`，需显式 `--execute-native`；它启动自己的状态目录和进程，选定单一路线并关闭后台恢复，保留正式配置哈希核对。`--resume-evidence` 仅复用已完成且完整的报告，补跑已知失败或缺失节点，不重放 unknown/interrupted 节点。原生报告正确性、其他模型可用性及 macOS 运行验收应分别说明。
