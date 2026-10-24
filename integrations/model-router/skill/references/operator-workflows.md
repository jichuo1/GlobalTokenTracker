# Codex 规划、操作员执行、审阅回报

使用 `router_dispatch`，工作流包含一个 node_type=codex 母节点、若干 operator 和可选 reviewer。task/report 两类边各自无环；可以形成“母→操作员(task)→审阅者(report)→母(report)”反馈。回报可串多模型，不能把它当作自动修改授权。

提交必须有显式 execution={root:absolute existing directory,writable_paths:[exact relative filenames],commands:{}}；不自动授权整个仓库。原生工具保持禁用，高速模型返回 operations，由 GTT 在授权文件中实际执行。不得通过模型输出扩大路径或命令清单。

1. 稳定 request_id 提交后，用 topology 获取全部节点、任务层级、回报链、状态和预算。真实 Codex 根据这些信息决定分片，不只看操作员列表。
2. coordinate(action 参数中的 decision.type=assign) 带 expected_revision、稳定 event_id、assignments。每项包含 node_id/instruction/paths，可带 input_range 字符区间、command_ids 和 acceptance。多级子授权必须在父授权以内；并行文件片避免重叠。

文件内容较长时可带 file_ranges={"src/example.py":[20,80]}（从1开始、两端包含），上下文只给这些行而SHA覆盖全文；此类片段仅允许片内唯一replace_text，不能write_text成全文。选择操作员前查看topology.model_routes的真实effort、健康与用量，不因Flash/DeepSeek名称推定无原生思考或一定更快。
3. events 以 cursor 回收 native 进度、操作收据和审阅报告。只收到回报的审阅模型等待真实结果，不提前运行。核对 native_backed、具体模型、实际操作后文件与收据；模型提案不是写入证据。
4. 审阅拒绝时，真实 Codex 查看具体 proposal，再用 coordinate(decision.type=revise,proposal_digest,assignment) 确认该份修正。不要把事件、模型建议或多数投票当批准，不重复执行成功的其他节点。
5. 所有节点和审阅闭合后，独立核验并用当前 evidence_digest 回填 accept。文件后来发生变化、仍有信息请求或其他分支尚被拒绝时，不能验收。取消和未知副作用继续保留，不自动重放。

完整报告已经生成但文件步骤被已知门禁阻断时，先核对所有原始文件基线仍未改变，再以 retry_operations + node_id + 当前 proposal_digest 明确批准复用提案；这不发新生成、不允许命令或未知操作重放。不得为了重做本地步骤再次让模型生成相同操作。

母节点是实际调用方接口，服务不会自动启动或认证 Codex 模型身份。当前聊天由 Codex 承接时，持续查看 pending_actions 并亲自做分配/确认/验收；不要只提交图后把 awaiting_codex 当成卡死或生成成功。没有当前 caller 能履行母节点职责时，明确保留等待状态，不能伪造回执。

普通研究任务保留原真实 Luna 监督要求；该可视工作流可以显式关闭可选 Luna，让真实 Codex 母节点控制。文件执行不额外记模型 tokens，tokens_estimated 是 tokens_used 的子集标记，不能再叠加一次。长期任务仍受累计预算、max_parallel 和 max_rounds 限制。
