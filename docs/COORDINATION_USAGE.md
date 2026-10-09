# 协作调用用量接入

GTT 会自动发现数据目录下的 `router/router-state.sqlite`，以 `gtt_coordinator` 数据源只读扫描其中的 `usage_records` 表。自定义位置可通过 `GTT_ROUTER_DB` 指定绝对数据库路径。数据目录仍沿用 `GTT_DATA_DIR` 的约定；无需提供 API 密钥，也不会读取调度器的作业内容或配置。

## 计量契约 v1

表结构为 `usage_records(seq INTEGER PRIMARY KEY, record_key TEXT UNIQUE NOT NULL, payload TEXT NOT NULL)`。`seq` 是全局递增水位；更新已有记录时也必须换成新的全局水位。`record_key` 是跨重扫、重启不变的调用身份。记录与作业状态应在生产方同一事务中保存。

`payload` 是 JSON，包含以下字段：

- `version: 1`；`record_key`；`app`；可空的 `provider_id`、`model`、`request_model`、`session_id`。
- `ts_start` 为 epoch 毫秒；可空的 `duration_ms`；`status` 为调用结果。
- `measurement` 为 `reported` 或 `estimated`。只有 `reported` 进入正式用量账本；调度预留和超时估算不作为已消耗用量。
- `input_tokens` 不含缓存；`output_tokens` 包含供应商计入输出的思考用量；`reasoning_tokens` 单独展示，不再次加到总量。
- `cache_read_tokens`、`cache_write_5m_tokens`、`cache_write_1h_tokens`；`unclassified_tokens` 为有明确总量但无法拆分的剩余用量。各字段为非负整数，单字段上限为一万亿。
- `native_backed` 为布尔值；`native_dedup_key` 为可空的原始请求去重键。

只有总量的历史记录通过 `unclassified_tokens` 进入工具、模型、趋势和总量统计；明细显示“未分类”，CSV 单独导出该列。任何带未分类部分的记录保持 `unpriced`，不会按猜测的输入/输出比例计算费用。

## 去重与恢复

OpenCode 可提供 `opencode:msg:<message-id>`，与原生适配器共享去重身份。原生账本已有足够完整的用量时保持其归因；原生记录不完整时沿同一键补全，不新增第二笔。

Antigravity 聚合记录、缺少原始请求 ID 的历史 OpenCode 记录，以及调用方 Codex 的本地用量依赖各自原生日志。它们不会作为新消费再加一次。自定义执行源若会同时写原生日志，必须声明 `native_backed`；当前 v1 直接请求去重键仅支持 OpenCode。缺少请求身份的原生聚合不会自动按时间或模型名称猜测匹配。

直接调用但不写原生日志的来源使用 `gtt_coordinator:<record_key>`。输出格式、工具策略等校验失败发生在生成之后时，明确返回的用量仍然入账。恢复检查产生的明确用量也使用独立稳定键；旧版本仅留下预算计数、没有计量标志的恢复检查不强行补成实际用量。

每批最多读取 2048 条记录。GTT 将事件入账和水位推进放在同一事务中，写入失败或不兼容记录不会推进水位，下一次扫描可重试。旧版本调度器没有此表时正常兼容。消费者始终只读生产方数据库，价格与聚合仍走 GTT 既有管线。
