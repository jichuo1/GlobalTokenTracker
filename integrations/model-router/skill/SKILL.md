---
name: gtt-coordinator
description: Coordinate bounded research, extraction, local code reviews, logic checks, and long-running research groups across configured coding tools through GTT's model router; prefer external workers, avoid overlap with Codex subagents, and use explicit caller-side Luna handoffs when needed. Use for substantial tasks that benefit from independent supporting evidence, not trivial edits or final deployment decisions.
---

# GTT 协同调度

通过 `gtt-router` MCP 提前获取主线需要的辅助信息。Codex 保留任务定义、源码核对、最终决策、修改、测试和集成的责任。外部报告和交叉检查是候选证据。

## 分配与 token 效率

- 在等待构建、读取主线代码或实现之前，识别真正能并行的辅助问题。优先外部软件；外部同类路线满额时先转其他外部来源，再考虑本地 Luna。不要为了“多模型”重复已经清楚的工作。
- 首次调用 `router_inventory` 使用默认紧凑输出。通过 `router_plan` 看能力、速度、额度、截止时间和选择理由；模型名、模型列表或成功算术探针不能证明评审能力。
- 默认 `routing_strategy=auto` 按任务类型选路；确有取舍时可设 quality/speed/reliability/token_efficiency。策略只改变合格路线间的偏好，不能绕过质量、额度或能力门槛。token_efficiency 只采用至少三次同类实际用量，并按材料规模修正；缺少数据是 unknown，不代表低消耗，更不代表低价格。
- 普通任务保留 `allow_fallback=true`，尝试数应覆盖成员、互证与至少一次合格补位；`preferred_route` 作为偏好，不固定唯一来源。只有明确做单路线诊断或用户要求严格选定来源时，才关闭回退并只试一次。查看 plan/submit 的 warnings，不能忽略回退被关闭或没有补位次数的提示；显式调用方约束仍被服务尊重。
- 快速资料收集用 `research` 且 `needs_web=true`；只整理已提供材料用 `extract`/`summarize`。有逻辑要求的局部代码用 `code_review`/`logic_check`，给出源码片段、具体疑点、基准提交和验收条件。实现长程任务仍留在主线。
- 仅传必要片段，通常先控制在数千字符；逻辑检查、局部评审与交叉核对默认总输出上限 32768 tokens、截止时间 600 秒，检索/提取/整理默认 8192 tokens、180 秒。总输出包含隐藏思考，最终报告通常压到 5000 字符以内。高难度任务可显式提高到 65536 tokens，并按需设置最长 900 秒；上限是余量，按实际返回用量计量。简单整理可显式降低预算。用 `context_label` 和 `base_revision` 识别材料，禁止传完整私有仓库、整个主线程历史、凭证文件或真实密钥。外部提交需要处在用户当前任务授权范围内；新目的地、发布或外部写入仍需要对应授权。
- 未指定 `token_budget` 时，调度器按输入、输出上限、并发与交叉核对计算默认总预算，最高 200000 tokens；显式总预算优先。通过 `router_plan` 检查 requested/effective_output_tokens 和模型明确上报的 output_limit。模型上限较低时会按实际能力收紧；大上下文、多路并发若仍超出总预算，应拆成独立小任务。不要在截断后反复用同一小上限重试。
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

需要多个阶段、来源互证或高消耗研究时，使用 `router_group_plan`/`router_group_submit`，给出 `topic`、必要 `context`、稳定 workflow/work_key，并说明是否需要网页证据。每组自适应 3–8 个角色，含一名高质量负责人；采集优先速度，分析/检查保留智力门槛，多组共享并发和计量。默认最多三个组执行，主线预留一个全局槽位。模型清单和榜单只是路由依据，不能当成果验收。

组默认累计预算 50000000 tokens，包含思考、正文、失败尝试的可观测用量与保守估算。每轮不加 GTT 人工 token 上限，原生模型上下文/输出范围和协议容量仍有效。默认 12 轮/6 小时，可显式设置至 64 轮/7 天；上限不等于实际消费。通过 `router_group_collect` 分页回收关键材料，主线保留独立核对、统筹、发散与最终质量验收。

负责人可要求补查、替换失败/低质量模型、申请扩展或交回 Codex。预算扩展需 Codex核对理由并显式 `router_group_extend(approved=true)`，最多 100000000；增减席位需 `router_group_adjust_seats(approved=true)`，仍在3–8范围。模型输出不会自动批准这两类变更。处理成员 `codex_fallback` 时，沿已有真实子代理能力补位、同步 lease，核对后通过 `router_complete_codex` 回报；组仍须负责人和 Codex终审，不把Luna名字当能力证明。

重启保留报告和已计量消耗并标 interrupted；只有明确的 `router_group_resume` 继续，不自动重放未知/已付费调用。不完整成员证据不能直接验收。组报告进入 `submit_codex` 后，由 Codex核对再用 `router_group_complete(accepted=true)` 标记完成；未解决问题保持待审或续研。若当前连接尚未出现组工具，使用已安装 `router.py call --method ... --params-file ...`，新MCP连接可发现新工具，不重新启动另一层Codex。

## 与 Codex 子代理共用调度

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

驻留服务在闲置容量下做小型生成恢复检查，默认首次等待 30 分钟，持续失败指数退避至最多 6 小时；每小时最多两次且有独立 token 上限。前台任务到来会中断检查并让出容量。检查成功恢复的是可用性，不会抹掉质量反馈或宣称智力升级；模型列表检查不足以恢复生成可用性。可以在本地 recovery 配置关闭或限制检查，不通过聊天传递凭证。

## 扩展与参考

需要新增执行源、额度含义或榜单字段时，读取 [references/integration.md](references/integration.md)。只扩展本地已授权的适配器配置，不修改现有项目或全局的模型、权限、凭证或登录状态。协调器有自己的 Antigravity 项目，网页读取及缓存读取权限只作用于该项目，工具门禁进一步限制到本次成功抓取的缓存文件。

若当前聊天尚未加载新 MCP/Skill，说明需刷新 MCP 或新开会话；可用已安装的 `router.py call --method ... --params-file ...` 做本次已授权的检查。不要声称未加载的工具已经在当前会话可用。
