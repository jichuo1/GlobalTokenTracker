# 接入与运行契约

生产入口为本仓库根目录的 `router.py`，仅使用 Python 标准库。默认配置在 `~/.globaltokentracker/router/config.json`；原软件的凭证继续由各自 CLI 读取，不复制进协调器。`bootstrap.py` 保留已存在的协调器配置。

## 进程与状态

每个 Codex MCP 客户端启动一个轻量 stdio 代理，代理连接同一用户的本地驻留服务。驻留服务只监听 `127.0.0.1` 随机端口，使用用户目录内的随机令牌；客户端禁用 HTTP 代理，令牌不经过代理服务器。关闭一个 MCP 客户端不会杀掉其他客户端的作业。

私有状态在 `router-state.sqlite`，独立于 GTT 账本。服务重启把未完成执行和未完成 Codex 交接标为 `interrupted`，保留报告；不会自动重放可能已计费的请求。当前进程只保存有限数量的已完成任务上下文，持久化报告仍可回收。不要把此目录提交到 Git。

刷新 `router_inventory(refresh=true)` 会重新加载来源、模型配置和榜单缓存。修改 `state_dir` 或驻留线程数需要在无运行任务时重启。配置中的源并发上限是本地调度约束，不是声称知道供应商的真实上限；远端 429、额度和认证失败仍会触发有界熔断。

## 任务和状态

`router_submit` 的 `task` 需要 `kind` 和 `prompt`，可提供必要的 `context`、`deadline_s`、`fanout`、`verify`、`min_quality`、`max_attempts`、`max_output_tokens`、`token_budget`、`workflow_id`、`work_key`、`base_revision`。逻辑性任务默认质量门槛 38；无准确匹配的榜单条目只使用显式标出的启发式先验，不能伪装成已验证智力。

模型总输出预算包含思考。最终报告要求 JSON：

单次 `max_output_tokens` 范围为256–524288。逻辑任务默认32768/600秒，其他任务8192/180秒。常规默认总预算封顶200000，显式大输出/native可扩大，普通任务硬上限4000000。已知模型output_limit收紧实际请求及预约；plan/attempt保留requested/effective。未知上限不从上下文窗口猜测。长正文显式report_max_chars为1000–4194304字符，默认120000；该容量同时传给提示词、解析及有界原生帧，不等于实际生成长度。0.3.1通过health的report_capacity_v1声明能力，基础协议仍为2；客户端在新字段派发前检查能力。

显式queue_wait_s优先于legacy external_queue_grace_s接力默认。没有显式值时保留原配置行为；研究组沿组总截止等待。等待不占原生执行预约、不续期模型活动，也不提高全局/来源/provider/scope容量。

```json
{
  "summary": "简短结论",
  "findings": [],
  "sources": [{"url": "https://example.com/actual-page", "title": "页面标题", "claim": "该页面支持的事实"}],
  "uncertainties": [],
  "complete": true
}
```

源码评审把代码锚点放在 `findings` 内，`sources` 可以为空。检索报告必须有实际网页工具观测支持的来源；只写出 URL、搜索条目或首页域名不能冒充已读取具体页面。获取成功也不能证明模型解读正确。

状态包括 `queued`、`running`、`succeeded`、`partial`、`failed`、`cancelling`、`cancelled`、`interrupted`、`needs_codex`。`needs_codex` 表示等待调用方使用自己的真实子代理能力；服务器不会创建 Codex 进程或用户聊天。`router_complete_codex` 是调用方报告，实际模型和用量应来自调用方证据。缺少有效 usage 时记有界估算，不写成零；超预算状态可见。原生 CLI 隐含提示词、缓存及思考开销只有执行后才可获得，预算不是供应商账单的绝对上限。

`router_sync_codex_agents` 使用调用方实际代理 ID 和范围，默认租约 120 秒。相同 workflow/work_key 避免重复，实施与审阅可用不同用途键。外部满额先找合格外部来源，等待短宽限期后才返回 Luna 建议；明确设置 `allow_fallback=false` 时不改执行源。

## GTT 桥接与额度

GTT 设置的「外部协同接口」是协同观察与模型调度的共同门禁。共享文件为全局数据目录的 `external-interface.json`，格式 `{"schema_version":1,"enabled":false}`；有效账本旁的同名文件也可收紧权限，全局关闭不能被自定义账本绕过。GTT_DATA_DIR 与旧目录兼容按原生 GTT 的路径规则解析。文件缺失为旧版开启状态；非布尔值、未知字段/版本、重复键、超4096字节或读取异常均拒绝外部调用。GUI 显示错误，保存切换用同目录原子替换；不触碰 UI 其他配置或来源凭证。

所有 MCP/API 业务调用在入口读取共享门禁；普通/组提交、续研、每次预约与实际 adapter 调用前还要求有效 Luna supervisor。关闭后没有 SQLite 回退、新重试或后台恢复探测；已有付费调用不强杀，完成仍正常计量。重新开启只恢复接口可用性，继续调度仍须真实监工及有效租约。开关关闭时只能通过用户本地设置重新开启，外部接口不提供开关写入入口。

`observations.py` 优先调用 `globaltokentracker-cli coordination snapshot` v1，验证版本、时效、类型、范围和作用域后只保留白名单。默认自动定位 Windows 已安装 CLI，也可在本机 `gtt_interface.command` 指定绝对路径参数数组；默认 5 秒超时、512 KiB 输出上限。CLI 缺失、旧版或传输失败可回退 `bridge.py` 的 SQLite `mode=ro/query_only`；`allow_legacy_fallback=false` 可关闭。`router_inventory` 的 `gtt.origin` 显示实际传输和回退原因。有效 unavailable 响应不会被数据库回退覆盖。

查询不触发扫描或额度网络请求；需要时显式调用 GTT `coordination refresh --source <id>`，网络额度另加 `--quota`。两条读取路径均使用同一事务，不导出原始账号、提示词、路径、raw_json 或原始错误。每次路由重新检查额度 TTL，目录缓存不延长有效期。历史延迟按同类型成功输入规模保守上调，不能冒充实时 ETA。正式协议与 JSON Schema 在仓库 `integrations/gtt-native/`，原生 GTT 补丁仅含通用源码和合成测试。

只有新鲜、未过重置点、账号匹配且含明确用量百分比的 allowance window 才形成额度判断。`session_ctx` 是上下文，不是余额；未定义语义的 credit 数字不换算百分比。额度缺失或过期是 unknown，不是 0，也不是无限。

## 新来源

新增 `opencode_http/claude_cli/codebuddy_cli/dsh_headless`。OpenCode 每个已连接 provider 的全部文本模型进入目录，默认 model 仅是偏好；角色预设不虚增模型数量或容量。选择最高已枚举推理变体，按实际延迟和反馈校准。新来源只提供无工具文本辅助，不因软件支持浏览器就标记 web 能力。

`scripts/register_agents.py --apply` 在私有目录备份并更新配置，保留显式来源、禁用状态和并发。`router_inventory.source_registry` 显示未接入原因。只有 GUI、缺少 CLI/接口或本地服务未运行时保留未注册状态；不冒充生成成功。驻留服务重载须等待执行任务完成。

内置 `zcode_rpc` 和 `antigravity_cli`。ZCode 只选择有效配置中启用的 API-key 路线，显式固定 provider/model/effort；`tools: []` 与客户端动作拒绝用于有界评审。桌面账号计划的 entitlement 或运行时 provision header 不能仅凭静态配置假装可用，需要另做适配和真实探针。

Antigravity 使用协调器专用项目，补充网页和网页缓存读取许可，拒绝 shell、写文件、网页操作和 MCP 操作；不更改原有项目和全局权限。按 `focus_terms` 把本会话成功抓取的页面聚焦成最多约 6000 字符的 `gtt_excerpt.md`，保留源 URL 和原始行号；`view_file` 只允许该摘录，其他本地文件被拒绝。必要的兼容回退只允许显式限定不超过 120 行的已确认缓存。工具次数受 `web_tool_budget` 限制，缺少页面读取证据的报告不能成功。

任务使用稳定的空白工作槽位来支持原生前缀缓存，每次仍启动新会话而不续接其他任务；开始前重置门禁和导引状态，结束后释放槽位。每份任务证据保存到单独目录。是否真的命中原生缓存以返回的 usage 为准，不声称一定命中。实际工具调用与门禁不匹配就停止并报告，不能把 Agent frontmatter 的 tools 列表当作唯一执行边界。

其他软件可使用配置注册 `command_json`：

```json
{
  "id": "my-coding-tool",
  "adapter": "command_json",
  "enabled": true,
  "trusted": true,
  "command": ["C:/absolute/path/python.exe", "C:/absolute/path/bridge.py"],
  "provider": "configured-account",
  "capabilities": ["text"],
  "max_parallel": 2,
  "models": [{"id": "exact-native-model", "effort": "high"}]
}
```

桥接程序从 stdin 接收 `{prompt,model,effort,max_output_tokens}`，stdout 返回一行 `{status:"SUCCESS",model,response,usage,tool_calls,observed_urls}`；`response` 是上面的报告 JSON 字符串，诊断写 stderr。命令使用参数数组，不执行 shell；prompt 不进入进程命令行。该适配器只运行用户明确配置和信任的桥接程序，桥接程序负责其原软件权限限制，不能把任意命令注册包装成安全隔离。

也可在 `ADAPTERS` 中注册新驱动。驱动遵循 `ExecutionContext` 的取消/截止时间，返回实际模型、终态、工具记录和 usage；认证/额度失败按提供商域隔离，明确模型不存在等错误按路线隔离。用真实无敏感数据的探针验证，catalog 成功不足以证明生成成功。

## 榜单与校准

`benchmarks.json` 使用准确模型别名，保存来源 URL、观察时间、测试版本、harness、effort 和性能。不同 Terminal-Bench 版本、不同 harness 和不同思考档不直接混成一个“智力等级”。当前种子是带来源的 AA 公共页面快照，不是对本机 API 路线的速度保证。

可在本机环境配置 `AA_API_KEY` 后调用 `router_refresh_benchmarks`，通过官方 API 更新明确匹配模型的 Intelligence Index 与公开性能字段。无 key 不访问受限接口，也不覆盖缓存。API 未返回的 Terminal-Bench 数值保留原时间，不因为其他指标更新就变成“新鲜”。不要求把 key 发进聊天。

本地采纳反馈按 route/task_kind 校准；相同 job/worker 的反馈幂等，可以修正。延迟与 token 消耗有同任务类型的移动平均；有实际历史后，预算预留会采用观测的输入/框架开销，避免只按短用户提示词估算。首次调用的隐藏开销仍无法准确预知。代码修改或结论仍由主线验收。

## 可用性降权与恢复检查

软件来源、提供商和模型路线分别保存近期失败惩罚。可用性乘数参与排名；累计失败次数只用于诊断，不作为永久惩罚。惩罚默认约 30 分钟半衰期；重复故障降权，普通成功加速恢复，真实生成恢复探针清除对应可用性惩罚。采纳率/任务质量反馈保留独立，不被连通性成功冲掉。

只有明确的连接/认证/额度/模型不可用等失败才进入恢复检查候选，已有额度耗尽或禁用容量不绕过。检查从已配置的原生执行源生成固定 JSON 标记，不附带项目代码。恢复后仅证明生成成功，不证明评审能力。

`recovery` 默认配置：enabled=true，poll_s=60，initial_delay_s=1800，max_backoff_s=21600，probe_timeout_s=30，max_probes_per_hour=2，max_tokens_per_hour=24000。生成超时结合已有路线延迟上调至最多120秒，手动复检至少60秒。只在无前台执行时启动一个检查；前台任务/子代理工作优先。失败退避按检查次数增长，取消的检查不降权但仍计入调用/用量预算。没有用量则明确保守估算；若原生隐藏 token 超出预测，当次只能事后记录并停止本窗口的后续检查。

服务和配置都可检查到 next_probe_at 与恢复记录；修改 enabled/频率/预算后用刷新 inventory 重新加载。检查不是 Codex 聊天自动任务，不创建新聊天，不向其他人发消息。

## 本轮偏好接口

`router_preferences` 的 action 为 set/get/clear。首次 set 只给 preferences，返回随机 scope_id；后续 set 带 scope_id 合并更新。preferences 支持 preferred_sources、preferred_models、preferred_routes 的精确目录ID列表，weight=0–100（默认40），concurrency_bias=-1至1（默认0），max_parallel=null或1–32。ttl_s=60至604800，首次默认21600；合并更新未给ttl_s时保留原到期时间。get/clear 需要scope_id，不能带修改字段。

同轮 Task/GroupRequest 传 preference_scope，实际 Codex代理lease也传该字段。plan/submit 的 preferences 显示有效上限、建议并行数和过期警告。max_parallel/负倾向限制的是该scope已预约外部请求与登记运行中代理的合计，仍遵守更小的全局/提供商限制。正倾向不重复生成或改变组席位。Codex根据用户与负责人反馈判断后调整；完成后clear，未清理的作用域会自动过期。
