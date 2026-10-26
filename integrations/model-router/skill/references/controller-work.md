# 母节点并行工作与截断恢复（0.7.3）

服务通过 `controller_work_v1` 和 `operator_recovery_v1` 声明能力。Codex仍是当前真实调用方；服务记录它报告的活动、依赖和证据，不启动或认证另一个Codex。所有控制继续使用统一 `router_dispatch`。

## 下发后继续有用工作

先按完整拓扑、文件边界及验收条件分配操作员。把目标中尚需研究、验证或规划的真实问题登记为母节点待办；下发后处理就绪项，遇到相关回报再检查。没有独立问题时才等待，不能为了显示忙碌重复研究或编造进度。

所有决定仍携带当前 `expected_revision`、稳定 `event_id` 和当前 `dispatch_id`。以下是 `decision` 的例子：

```json
{
  "type": "controller_work",
  "op": "enqueue",
  "items": [
    {"id": "budget-research", "kind": "research", "instruction": "核对剩余预算与恢复策略的边界", "paths": [], "access": "snapshot"},
    {"id": "verify-write", "kind": "verification", "instruction": "检查真实落盘文件与目标合同", "paths": ["src/example.py"], "access": "current", "depends_on": ["operator-a"]}
  ]
}
```

`kind` 为 research/analysis/planning/review/verification。`paths`若非空，只能读本execution已授权文件，不能通过此接口扩大文件或命令授权。`depends_on`指定本图节点，默认等成功；诊断失败时可显式用`wait_for=terminal`。每次最多登记16项，全图最多64项。

用`op=start,item_id,lease_s`实际认领，租约15–600秒，默认120；工作期间按真实状态用`heartbeat`续租，完成用`finish,item_id,summary`，放弃用`cancel,item_id,reason`。一名调用方一次只认领一个母节点项。租约不延长原生作业截止、不增加预算或并发席位。

`snapshot`返回冻结的内容和完整SHA，允许操作员继续修改源文件；只能把它当该版本的证据。`current`读取当前版本，与同文件写入或workspace命令冲突时先处理其它就绪项；它也会阻止后续冲突写入，独立文件仍可执行。源文件或绑定上游随后变化，检验会标`stale`，需重新认领并检查。过期租约释放读约束，但不能拿过期状态报完成。

`topology`及`status.detail.controller_work`给出`ready_items`、`active_item`、`reports_ready`、`next_action`与`should_wait`。有就绪工作时不阻塞collect等待全部分支。母节点展示状态可为running，`dependency_state`保留已下发任务的依赖完成标记，避免母节点继续工作把下游堵住。活动和身份字段明确为caller-reported，不能把它当隐藏思考的观测。

## 回报与母节点检验

收件箱按节点的job、work_revision、报告及操作结果计算指纹。只有当前指纹可通过`ack_reports`确认，旧回报不能关闭新版本的待办。确认收到不等于质量验收。

```json
{"type":"controller_work","op":"ack_reports","reports":[{"node_id":"operator-a","fingerprint":"当前收件箱中的指纹"}]}
```

母节点可在verification/review项完成时给`verdict=accepted`，或者用`verdict=needs_revision`、准确`reports`指纹和有依据的`issues`提出修正。只有已经applied的当前操作结果可进入此流程；它不会自动执行修正，也不会代替图中已有外部评审或最终accept。

```json
{"type":"controller_work","op":"finish","item_id":"verify-write","summary":"定位到边界条件错误","verdict":"needs_revision","reports":[{"node_id":"operator-a","fingerprint":"当前指纹"}],"issues":["具体输入会触发不符合合同的结果"]}
```

## 确定截断后的恢复

`length`截断生成`generation_recovery`提案，包含原job、工作版本、剩余节点预算、原始截止与建议额度。先分清思考耗尽和部分正文，不能一律缩输入或盲目增额度。完整函数或具体修改边界应由母节点重新规划。部分JSON及残缺operations绝不能落盘。

确认提案后，用`revise`绑定精确`proposal_digest`与新assignment。可选`generation`只允许max_output_tokens、output_mode、route_effort；不能改质量门槛、路线池或总预算。相同指令若没有增加输出余量会被拒绝。无关分支运行时仍可修订；相关下游、同文件和命令冲突必须先闭合。

```json
{"type":"revise","proposal_digest":"提案digest","assignment":{"node_id":"operator-a","instruction":"母节点给出的具体函数级实现或修正","paths":["src/example.py"]},"generation":{"output_mode":"adaptive","max_output_tokens":8192}}
```

恢复累计扣除该节点所有版本真实/保守计量，沿用原绝对截止及工作流总预算、最大轮数。未知付费结果、timeout和unknown文件副作用不产生可自动恢复的提案。`retry_operations`只复用已完整生成且可核对的操作提案，不用于截断恢复。

最终accept仍要满足全部节点成功、审阅通过、信息请求闭合、当前文件SHA匹配、母节点工作闭合及精确evidence_digest。失败不会再被提示成可验收。实际模型截断案例、合成并发验证和端到端提速是不同证据，分别报告。

0.7.4通过`atomic_edit_bundle_v1`支持同文件多个独立`replace_text`：必须来自同一个冻结SHA，各匹配在授权快照中唯一、互不重叠。执行器先校验全部原操作，再组合为一个原子替换；回执保留原操作ID映射和前后跨度哈希。切片仍只能在片内替换；重叠、依赖前一操作产物、混合命令、错误基线或超出字节上限都拒绝。不要为解决批次冲突重新付费生成已经完整且可核对的方案；已知未落盘且基线未变时，可按现有`retry_operations`流程由母节点显式复用。
