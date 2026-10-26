---
name: gtt-coordinator
description: Coordinate bounded tasks and visual workflows through GTT's model router. Respect directed route pools, configurable call profiles and the workflow's optional Luna setting. Ordinary/group calls keep real Luna supervision by default. Prefer external workers and avoid duplicate work.
---

# GTT 协同调度

通过 `gtt-router` MCP 提前获取主线需要的辅助信息。Codex 保留任务定义、源码核对、最终决策、修改、测试和集成的责任。外部报告和交叉检查是候选证据。

## 正式受理入口（0.5.0）

0.6.0 的 Codex 母节点 / 高速操作员见 [操作员与回报拓扑](references/operator-workflows.md)。用户明确要把文件操作交给高速模型时，Codex 负责完整拓扑、分片、审批和验收；使用 operator 节点及本次 execution 工作区授权承接具体修改，不再把实现工作一律留给主线。原生模型工具仍禁用，操作由 GTT 的可信执行器完成。

0.7.0 可由 Rust 接管状态数据库和调度运行记录；检查版本、处理存储故障或切换本机服务时，读取 [Rust 状态运行时](references/rust-runtime.md)。日常派发协议保持兼容，不因新后端而自动重放未知结果。

所有新计划/提交使用 `router_dispatch`，`request.kind` 为 `task`、`group` 或 `workflow`；分别携带 `task`、`request` 或 `definition` / `saved_workflow_id`。`action="submit"` 必须给稳定 `request_id`，拿到 `dispatch_id` 后以它或原 request_id 查询 `status`、`collect`、`cancel`。网络返回不明确时先查原身份，不换 ID 重放。`report` 带 worker、工作流 node_id 与 offset，无损读取大报告并核对 sha256。旧 router_plan/submit、router_group_plan/submit、router_workflow_plan/submit 已从工具发现中退役，仅为缓存客户端兼容转发；下文这些旧名指对应动作，新调用全部换成统一入口。GTT CLI 的 `scheduling` 命令也复用这一协议，详见 [统一接入](references/dispatch-intake.md)。

外协或调度开关关闭时停止新派发；认证的本机管理入口仍允许 status/collect/report/list/cancel，以回收已发生的用量和取消存量。统一入口不自动启用开关、更新过期池、恢复未知付费请求或增大预算。普通任务与研究组保留真实监工要求，工作流按保存的 luna_supervision 执行。

支持 output_steps_v1 的服务可显式设置 max_output_steps=2..16，所有完整 JSON 段共用原总预算、尝试次数与硬截止；仅全部段校验通过且末段 final=true 才验收。后续失败保留 complete=false 的部分内容，不重放未知付费结果。adaptive/native 首次和后续预约均按剩余预算收紧；fixed 和历史用量保护保留。操作员仍由 Codex 按授权文件或行范围分片。

安排操作员前先判断分工收益：少量确定性修改通常由母节点直接完成；多个独立且足够大的分片再并行。母节点提供函数级算法、边界和验收条件，避免把完整设计问题再交给高速操作员重复思考。同一路线反复把输出额度耗在思考且未完成时，先核对真实失败用量并在当前已授权模型池内选择合格替代，不要只提高上限或降低质量门槛。token_efficiency统计包含新发生且有真实计量的失败请求；未知用量仍不能当低成本。分别报告原生执行时间、主线验收等待和端到端时间，不将非任务等待计入提速结论。

支持 controller_work_v1/operator_recovery_v1 时，母节点可在下发后认领独立研究、规划和检验，用就绪队列与回报收件箱决定下个动作，不默认阻塞等待全部分支。真实活动用短租约报告，当前文件检验有读写冲突与过期证据检查；确定截断可由母节点绑定提案恢复，未知结果不重放。使用时读取 [母节点并行工作与截断恢复](references/controller-work.md)。

## 分配与 token 效率

普通任务默认 `timeout_mode=activity`，原 `deadline_s` 是首次响应期待窗。真实正文、推理增量、工具阶段或原生状态变化可继续执行；随后按 `idle_s + stall_grace_s` 判断停滞，总执行仍受默认3600秒的 `hard_deadline_s` 限制。用户给定硬截止时用 `timeout_mode=hard`。进程存活、重复心跳、空帧和轮询时间戳不能续期。当前ZCode RPC仅返回最终结果，无法可靠观察思考进度；按材料规模配置 `ttft_s`，不要伪造活动。组总截止和token预算仍是硬边界。

- 在等待构建、读取主线代码或实现之前，识别真正能并行的辅助问题。优先外部软件；外部同类路线满额时先转其他外部来源，再考虑本地 Luna。不要为了“多模型”重复已经清楚的工作。
- 首次调用 `router_inventory` 使用默认紧凑输出。通过 `router_plan` 看能力、速度、额度、截止时间和选择理由；模型名、模型列表或成功算术探针不能证明评审能力。
- 默认 `routing_strategy=auto` 按任务类型选路；确有取舍时可设 quality/speed/reliability/token_efficiency。策略只改变合格路线间的偏好，不能绕过质量、额度或能力门槛。token_efficiency 只采用至少三次同类实际用量，并按材料规模修正；缺少数据是 unknown，不代表低消耗，更不代表低价格。
- 普通任务保留 `allow_fallback=true`，尝试数应覆盖成员、互证与至少一次合格补位；`preferred_route` 作为偏好，不固定唯一来源。只有明确做单路线诊断或用户要求严格选定来源时，才关闭回退并只试一次。查看 plan/submit 的 warnings，不能忽略回退被关闭或没有补位次数的提示；显式调用方约束仍被服务尊重。
- 快速资料收集用 `research` 且 `needs_web=true`；只整理已提供材料用 `extract`/`summarize`。有逻辑要求的局部代码用 `code_review`/`logic_check`，给出源码片段、具体疑点、基准提交和验收条件。实现长程任务仍留在主线。
- 仅传必要片段，通常先控制在数千字符；小材料逻辑任务从32768 tokens / 600秒开始，检索/提取/整理从8192 / 180开始。总输出包含隐藏思考，报告通常压到5000字符以内。高负载任务可结合切片与实际模型容量显式增加输出至524288，并按材料规模配置首次响应窗；上限是余量，按实际用量计量。简单整理可降低预算。用context_label/base_revision标识材料，禁止传完整私有仓库、整个线程历史、凭证文件或密钥。外部提交须属于当前授权范围；新目的地、发布或外部写入仍需对应授权。
- 未指定 `token_budget` 时，按输入、输出、并发与互证计算默认预算，常规默认最高200000；显式大输出/native可扩大，普通任务硬上限4000000。显式总预算优先。普通显式输出允许至524288，默认fixed；未指定输出默认adaptive，按材料规模留思考余量。native是采用目录原生输出上限的显式选择，未知容量仍有界。通过plan检查requested/effective与output_limit及上下文余量，不把512k目录上限当实际生成证据。
- 明确length截断且正文为零时，先检查实际输出余量；adaptive可在原总预算和尝试次数内提高下一次上限，fixed不会超过调用方显式值。不要反复用同一不足上限重试，也不要仅凭零正文缩片。logical_fixed_output_below_reasoning_default提示固定逻辑预算偏低；output_recovery_unavailable表示没有合格的更大容量。部分JSON不能当完整报告，网页来源门槛仍须核对。
- 计划/提交返回resident_protocol_mismatch时，核对实际安装副本和常驻服务，而非重提任务；当前客户端要求runtime_contract_version=2。共享服务切换须遵循用户维护授权，保留配置/账本/认证，并在空闲窗口刷新客户端。源码更新不能证明运行版本已切换。
- 真正长正文需显式report_max_chars（默认120000，最多4194304），以同步提示词、解析和传输容量；仍要求完整JSON与严格complete=true。0.3.1通过report_capacity_v1声明此能力，旧任务协议2保持兼容；旧服务收到新字段前会返回resident_feature_missing。collect的大报告裁剪不是模型生成截断，完整结果保留本机状态库。显式queue_wait_s优先于旧external_queue_grace_s接力默认，仍受共享容量与总截止约束。
- 默认预算另留一次换源余量；允许回退且仍有尝试次数时，单次执行会留出总截止时间的一部分用于接力。较长评审材料会按同类任务历史输入规模修正 ETA，出现 `large_excerpt_consider_split_or_research_group` 时按问题/代码边界拆分，或转研究组；不要机械复制完整文件，也不擅自截断用户指定材料。
- 使用同一个 `workflow_id`。`work_key` 写“具体问题+用途”，例如 `comment-url:protocol-research` 与 `comment-url:code-review`。同用途不要重复提交；实现与独立评审应使用不同键。
- 拆分检索角度，不把同一提示词机械复制给多个模型。`fanout=2` 默认附带一次交叉检查；只有不需要互证时才关闭 `verify`。共同引用同一网页不算两个独立来源，模型投票不能消除无证据结论。
- 已知官方页面时提供 `focus_urls` 与少量 `focus_terms`，避免让 Flash 重新搜索整个站点。网页任务默认只允许六次检索/抓取/缓存读取，输入会被聚焦为短摘录；超出范围应拆成新的明确分片，而不是重复翻页。长程搜集本身也需要按主线决策点逐批安排。

## 本轮偏好与并发

用户明确要求本轮优先某来源或模型时，先用 inventory 将名称对应到准确 source/model/route ID，通过 `router_preferences(action="set", preferences=...)` 创建独立作用域；所有本轮普通任务和研究组都传 `preference_scope`。不要复用其他聊天的作用域。默认匹配项各加40分，可按用户强调程度设0–100；权重显著增加倾向，仍不能越过能力、质量、额度或恢复隔离。模型偏好可匹配多个软件中的同名模型，单个提供商偏好用 route ID。

`concurrency_bias` 为 -1 至1：负值收紧本轮并发，正值提高可用提供商倾向及建议并行数；`max_parallel` 可指定本轮目标，实际仍受全局/来源/提供商限制。plan 返回建议与有效上限，由 Codex结合独立任务数量、速度、成本和反馈选择扇出，不机械复制任务。负责人提出需要扩大或收束并行工作时，Codex判断后更新同一 scope；更新只影响后续预约，不取消已付费请求。席位变更/预算扩展仍走原审批接口。

用 set + scope_id 合并更新，未给出的字段保留；结束本轮清除作用域，默认6小时过期，长程研究可显式设置最长7天。过期或已清除会在 plan/submit 中提示并恢复中性策略。真实 Codex子代理登记也带同一 preference_scope，以计入本轮容量；达到本轮上限时等待，不能再拉 Luna 绕过。scope 是调用方绑定的本轮范围，MCP不会自动解析用户原话。

`max_parallel=2` 仅允许本轮最多两路执行；一项任务需要两份独立报告时还要显式 `fanout=2`（默认加互证）。多个不同问题用不同 work_key 分别提交，不能为凑并发复制同一问题。

## 分组研究

高消耗任务结合独立 `price_weight`、`prefer_free` 与 `prefer_new_models` 选路：明确零价和低价模型优先承担批量采集/整理，负责人仍须达到质量门槛。价格来自原生目录或准确route配置，过期/未知价格不当作免费，订阅权益和实际账单另列。同provider的明确家族和版本队列支持新版本偏好；新版本不继承旧版本榜单成绩，先做真实生成及适合其职责的回归，保留可用旧版作替代。round scope可覆盖这三个字段，不改软件全局选定模型。

需要多个阶段、来源互证或高消耗研究时，使用 `router_group_plan`/`router_group_submit`，给出 `topic`、必要 `context`、稳定 workflow/work_key，并说明是否需要网页证据。每组自适应 3–8 个角色，含一名高质量负责人；采集优先速度，分析/检查保留智力门槛，多组共享并发和计量。默认最多三个组执行，主线预留一个全局槽位。模型清单和榜单只是路由依据，不能当成果验收。

组默认累计预算50000000 tokens，包含思考、正文、失败尝试的可观测用量与保守估算。单次默认adaptive按角色和片大小分配，native显式采用原生输出上限，未知容量仍有界；没有固定每轮总配额。默认12轮/6小时，可设至64轮/7天；上限不等于实际消费。

大材料默认slice_mode=adaptive，目标12000字符、最小2000、最大32000；max_slices默认64可至256，含缩片父记录与支撑记录。角色席位与工作项数分开，max_parallel只约束本组队列，不提高共享容量。根据独立代码/问题边界使用context_range，机器按段落/行切片只提供可追溯片段，不保证语法或跨片语义独立。成功片不重放；未知付费结果只有调用方明确retry_unknown_slices=true才允许重试，并保留原计量。

成员应主动报告盲区research_leads与缺证information_requests，说明问题、原因、材料范围或原报告ID。授权材料/报告的定向支撑可提前补位，最多自动两层；网页/泛化补查由负责人用context_range、evidence_workers及有界next_tasks规划。未解决信息请求不能提交完整终审。负责人在guided探索中继续验证有价值的线索，最多默认2轮、组预算15%，范围扩大的建议报回Codex。不得为了保持活动编造线索。用collect/events回收请求、支撑出处和研究建议，主线仍须独立核对。

信息支撑用information_request_ids绑定worker:index问题编号，support_requests会携带原问题与原因；多问题不能仅凭worker关联全部关闭。分裂支撑须覆盖完整指定范围。探索成员单独记账，负责人复核及后续主线任务沿组总预算；scope_capacity_full、context_exceeded、预算不足和work_already_covered不得通过新Luna任务绕过。合组前处理待执行/未知片和未解决请求，保留原组显式并发与探索限制。

0.3.2的聚合任务最多绑定32份已完成报告及8个精确问题，报告摘录共享14000字符容量，不能把excerpt_only当完整报告；更大集合需分层汇总。定向支撑只回答support_requests，不重复全组审计。信息交付闭合有information_response事件与information_status，仍须独立核验答案。完成的worker活动快照按attempt冻结；纯活动日志约30秒采样，状态/预算/失败及问题变化立即记录，当前进度以supervisor快照为准。大事件保留有界省略标识并推进cursor，不让旧快照卡住事件回收。

角色消息正文超过600字符会标summary_excerpt_only并提供原事件ID，不能把裁剪的案例索引或证据当全文。重启续跑先加载路线目录，再复用缓存结果；slice计数表示当前轮队列，旧轮证据在完整results中保留，不能把新轮0成功误报为旧成果丢失。EOF不完整JSON的大切片可补位缩片，原生stop仍不证明完整；研究组提取最多16条finding，需要更多记录时分组保留，不因条数提示遗漏案例。

负责人可要求补查、替换失败/低质量模型、申请扩展或交回 Codex。预算扩展需 Codex核对理由并显式 `router_group_extend(approved=true)`，最多 100000000；增减席位需 `router_group_adjust_seats(approved=true)`，仍在3–8范围。模型输出不会自动批准这两类变更。处理成员 `codex_fallback` 时，沿已有真实子代理能力补位、同步 lease，核对后通过 `router_complete_codex` 回报；组仍须负责人和 Codex终审，不把Luna名字当能力证明。

重启保留报告和已计量消耗并标 interrupted；只有明确的 `router_group_resume` 继续，不自动重放未知/已付费调用。不完整成员证据不能直接验收。组报告进入 `submit_codex` 后，由 Codex核对再用 `router_group_complete(accepted=true)` 标记完成；未解决问题保持待审或续研。若当前连接尚未出现组工具，使用已安装 `router.py call --method ... --params-file ...`，新MCP连接可发现新工具，不重新启动另一层Codex。

## 与 Codex 子代理共用调度

普通任务、研究组、续研和默认恢复探测仍使用真实 Luna 监工。可视工作流的 `luna_supervision` 是用户可选项；尊重用户已保存或明确授权的关闭选择，通过 `router_workflow_submit` 执行该工作流，无需为它创建监工，也不能伪造登记。手动检测可通过 `router_scheduling(action="check", luna_supervision=false)` 显式关闭本次监工，其他共享门槛与探针预算保留。自动后台恢复仍要求真实在线监工。启用监工时，复用同 workflow 的实际运行监工；首次启动使用当前协作工具、可用 Luna 和 `fork_turns=none`，不能另起 Codex CLI、创建用户聊天或只填名字冒充启动。

实际启动后通过 `router_sync_codex_agents` 登记其真实 ID、实际 Luna model、`role="supervisor"`、`status="running"` 和与待调度任务完全一致的非空 `workflow_id`。为监工使用独立 owner；同 owner 的完整名单会替换原名单，不能续租时遗漏其他真实代理。普通执行代理沿用 `role="worker"`。运行中至少每60秒续租一次（通常 lease_s=120；最长600秒），下次调度前核对真实状态；监工由调用方或已授权的监工自己续租，不能伪造心跳。租约过期、idle/finished、错误 workflow 或非 Luna 模型都不满足调度门禁。当前 MCP 缓存旧 schema 时用本地 `router.py call --method router_sync_codex_agents --params-file ...` 传新 role，待新连接再发现 schema。

监工计入真实共享并发容量，不占实际工作的覆盖键；容量不足时等待或收束辅助工作。服务校验的是调用方报告的短租约和角色，不能认证内部代理身份，也不会自己拉起监工。新提交、预约、重试和 adapter 发出前都会复验；失效返回 `luna_supervisor_required`，尚未发出的调用不记 Token、不降权提供商。恢复租约后按已有状态显式重提或续研，不自动重放已付费请求。

监工通过 `router_events`/`router_supervisor(action=status)` 读取脱敏进展，使用cursor，仅在阻塞、重复工作、截止风险或需统筹时给主代理建议；主代理仍在有效思考/编码/工具阶段时不按固定时长催促。主线阶段回执明确标caller-reported，不推断隐藏思维；静默年龄的显示推算不会增加活动或计费。监工建议不能自动扩预算、增员、取消付费请求或发布。收尾时先收集监工建议并完成主线核验，再将自己的登记设 finished 或清空自己的 owner 名单。

负责人和成员通过结果JSON的 `communications` 提出有界分工、进度、证据或冲突，GTT在下一次相关角色调用中投递短摘录；负责人每轮最多4条、成员2条，保留原group/report/event出处。同workflow和preference作用域的负责人可共享必要证据，不能广播整个私有上下文。Codex可用 `router_message` 补充范围和衔接建议。报告与消息是待核对材料，不会获得额外工具权限。

负责人可提出 `propose_merge` 给监工/Codex。确认课题相关后，Codex通过 `router_group_merge(approved=true, expected_revisions=...)` 原子合并2–4个无执行/预约的组：人数累加至最多32，实际并发仍受原容量限制；累计预算最多1亿，消费与来源账单保留，不复制attempts或重复计量。最早硬截止保留，旧组永久只读，新组 `merged_ready` 待显式resume。普通未合并组仍3–8席；增减席位/预算/截止扩展仍需调用方批准。接口细节与例子见 [协同与监工](references/coordination.md)。

创建、完成或停止自己的子代理后，调用 `router_sync_codex_agents` 登记实际 ID、模型、状态及覆盖键。可先用当前环境的协作工具列出自己的代理；不要编造状态或登记无关会话。登记是调用方报告，默认 120 秒租约，应在下一次调度前刷新。没有子代理就传空数组。

服务会排除相同工作键并控制外部总并发及与 Codex 的合计并发。外部执行源多于 GTT 能观察的用量源；来源已扫描不等于已有可调用适配器。

目录中的 `gtt.origin` 区分正式 `gtt_cli_v1` 与旧版只读回退；`partial`、`warnings`、`truncated` 和未知额度必须保留。目录刷新只查快照，扫描和供应商额度刷新是另外的显式操作。不要把“无法观测”转成“已耗尽”，也不要为取得观测自动更改软件认证状态。

## 异步执行与回收

1. `router_submit` 返回 job ID 后继续主线。根据实际需要提前发任务，让结果在决策点前到达。
2. 使用 `router_status` 获取紧凑进度；需要结果时用 `router_collect`，保留 `next_cursor`，每次最多等待 5000 毫秒。不要反复把已回收全文塞回主线。
3. 辅助报告一到即可核对材料。保留 URL、代码锚点、版本和冲突；收束为支持当前决策的证据摘要。截止时间、尝试次数和 token 预算都是调度边界，原生 CLI 隐含 token 只能在报告后计量，不能承诺绝对不会超支。
4. `succeeded` 表示所请求的报告完成，不是正确性验收。检查 `partial`、`needs_codex`、失败原因和 `truncated`；不要把认证失败、输出截断、没有实际网页调用或没有可用结果算作完成。
5. 独立核对后通过 `router_feedback` 标记是否采纳，以更新同类型任务的路由校准。无需评分时不要伪造反馈。

## Luna 回退

当结果明确给出 `codex_fallback`，且当前仍有子代理容量、任务仍有价值和时间预算时，使用本环境真实协作工具拉起建议的可用 Luna 模型；若模型不可用，按当前公布的模型列表选同类可用模型。不要再次启动 Codex CLI 或创建新的用户聊天。

用 `fork_turns=none`，只给该子任务、必要片段和原始输出契约。登记代理覆盖键。回收后核对它的输出，再用 `router_complete_codex` 返回实际模型和内容；原始任务要求互证时还需完成显式交叉检查。服务不会自动拉起本地代理，也不声称验证过调用方报告的模型。

Luna 是节省花销的辅助选择，不能因为名字就断言与高级模型同等。任务超出其能力、质量门槛明确较高、已无并发/额度/时间预算时，保留失败信息，由主线处理，不无限重试或悄悄降级。

## 来源权重与恢复

查看 `router_plan` 的 source/provider/route weight 和 `router_inventory` 的综合权重、next_probe_at。近期反复失败会降权并按原因熔断，旧失败影响以约 30 分钟半衰期衰减，不能用累计失败次数永久排除路线。一条 API 提供商的认证失败不代表整款软件不可用。

认证、额度或模型不可用失败会标 `verification_required`；时间衰减不会单独解除隔离，恢复探针成功后才回到正常候选。用户确认认证/额度已恢复时，先用 `router_recheck(route_id)` 做一次小型生成复检。它占用共享闲置容量，遵守探针小时预算和禁用来源；忙碌或预算不足时保留返回原因，不能通过反复普通任务绕过。复检最多120秒生成及有界清理，当前连接未发现工具时可用本地 API。成功只证明可调用，不重置任务质量反馈。

驻留服务仅在外部协同接口开启、有有效 Luna 监工且有闲置容量时做小型生成恢复检查；无监工时不进行后台生成。手动 `router_recheck` 传与监工一致的 workflow_id（默认 default）。默认首次等待 30 分钟，持续失败指数退避至最多 6 小时；每小时最多两次且有独立 token 上限。前台任务到来会中断检查并让出容量。检查成功恢复的是可用性，不会抹掉质量反馈或宣称智力升级；模型列表检查不足以恢复生成可用性。可以在本地 recovery 配置关闭或限制检查，不通过聊天传递凭证。

## 外部协同接口开关

GTT 设置里的「外部协同接口」同时控制原生协同观察（capabilities/snapshot/refresh）与全部模型调度 MCP/API 入口。关闭后停止新模型请求，包括排队、重试、续研和恢复探测；已发出的请求允许完成并按真实用量记账，GTT 本地用量统计继续工作。开关独立保存在 GTT 数据目录 `external-interface.json`，不写进凭证配置；指定其他账本不能绕过全局关闭，SQLite 兼容回退也不能绕过。缺失文件沿用旧版开启状态，损坏配置按关闭处理并显示错误。遇到 `external_interface_disabled` 或 `external_interface_settings_invalid` 时停止外部协同，不自动改文件或重新打开开关。

## 扩展与参考

可视调度与配置档见 [定向工作流](references/visual-workflows.md)。先读取 `router_scheduling`，遵守来源开关、定向池有效期和 `effective_profile`，不绕过用户选定模型。`allowed_routes` 是有顺序的硬池，空列表禁止所有路线，不能通过 Codex 回退逃出池。节点可配置职责、类型、备用路线与调用参数，14 个职责模板包括 Codex 母节点、操作员、操作审阅员，以及不生成模型内容的输入、合并、门禁和输出节点。职责模板携带完整的角色化高级调用参数（输出 tokens、节点预算、等待与报告上限等，Codex 母节点模板为协调级配置），套用模板会整体替换节点的高级参数。

工作流提交使用稳定 `request_id`；网络返回不明确时先查同一请求与运行状态。未知节点不自动重放；明确接受重复付费风险后才使用新请求 ID 和 `acknowledge_unknown_risk=true`。输出、等待、重试、并发、预算和协议容量以配置档及模型原生能力共同约束，schema 的实现范围不等于当前已启用上限。默认 512k 输出策略保持，修改配置不构成大输出能力验收。

需要新增执行源、额度含义或榜单字段时，读取 [references/integration.md](references/integration.md)。只扩展本地已授权的适配器配置，不修改现有项目或全局的模型、权限、凭证或登录状态。协调器有自己的 Antigravity 项目，网页读取及缓存读取权限只作用于该项目，工具门禁进一步限制到本次成功抓取的缓存文件。

若当前聊天尚未加载新 MCP/Skill，说明需刷新 MCP 或新开会话；可用已安装的 `router.py call --method ... --params-file ...` 做本次已授权的检查。不要声称未加载的工具已经在当前会话可用。
