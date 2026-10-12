# GTT 局域网对等接口 v1

版本化只读 HTTP JSON 协议 `gtt.peer` 让同一局域网内的另一台 GTT 设备拉取本机的用量聚合，与自己的数据并列显示。对等方只交换紧凑聚合，不交换原始事件。本阶段（v1 第一期）只有核心协议与 CLI，便于无界面测试；设备配对、TLS 证书固定、mDNS `_gtt._tcp` 发现与 Mac/Windows 界面属于后续阶段。

## 调用与边界

```bash
globaltokentracker-cli peer serve                       # 默认仅 127.0.0.1:4319
globaltokentracker-cli peer serve --bind 0.0.0.0:4319 --name "书房 Mac"
globaltokentracker-cli peer pull 192.168.1.20 --format json
globaltokentracker-cli peer pull 192.168.1.20:4319 --endpoint summary --from 2026-10-01
globaltokentracker-cli peer pull 192.168.1.20 --endpoint summary --known 2026-10-11:464b3b56aee976ad,2026-10-12:d7e0afe13358b2d7
globaltokentracker-cli peer pull 192.168.1.20 --endpoint quotas
globaltokentracker-cli --db <账本路径> peer serve
```

局域网暴露必须显式开启：`peer serve` 默认只监听回环 `127.0.0.1:4319`，只有 `--bind` 指定非回环地址才对网络可见，此时在 stderr 打印警告。v1 没有认证与加密，能连到该地址的设备都能读取聚合；请只在可信网络中使用，配对与 TLS 见后续阶段。

`serve` 启动时是唯一的写入点：用常规读写方式打开账本（与 GUI 相同的迁移路径），在 `app_state` 中生成一次稳定的 `device_id`（`peer.device_id.v1`，32 位十六进制随机值），`--name` 存为 `peer.device_name.v1`。此后每个请求都用 SQLite 只读连接与单个读事务读取同一 WAL 快照，协议读路径从不写账本、不创建文件。未指定名称时依次取环境变量 `COMPUTERNAME`、`HOSTNAME`，否则为 `GTT-<device_id 前 6 位>`。

`pull` 的 stdout 只有一个 UTF-8 JSON 文档；连接失败或参数错误返回非零与 stderr。地址可带 `http://` 前缀，省略端口时用 4319。

## HTTP 约束

| 项 | 规则 |
|---|---|
| 路由 | `GET /peer/v{N}/{hello\|summary\|quotas\|sources}`；`N≠1` 返回 400 `unsupported_protocol_version`，不打开账本 |
| 方法 | 仅 GET；其他方法 405 + `Allow: GET` |
| 请求头 | 请求行与头部合计 ≤ 64 KiB，超出 431 `request_too_large`；10 秒内必须发完，否则 408 `request_timeout` |
| 请求体 | 不允许；`Content-Length≠0` 或 `Transfer-Encoding` 返回 413 |
| Host | 必须是 IP 字面量（可带端口）或 `localhost`，否则 403 `host_not_allowed`，用于阻止网页经 DNS 重绑定读取回环服务 |
| 并发 | 每连接一个线程，最多 16 个并发连接，超出直接 503 `server_busy` |
| 连接 | 每连接一个请求，`Connection: close`，`Cache-Control: no-store` |

请求层错误用 4xx/503 表示；账本层结果（繁忙、缺失、外部接口关闭等）一律 HTTP 200，由 `status` 与固定 `error.code` 判断，与协同接口一致。实现基于 `std::net`，没有 tokio/axum 或新增依赖。

## 输出与兼容

每个响应都包含 `protocol="gtt.peer"`、整数 `protocol_version`、`gtt_version`、`operation`、毫秒 `captured_at`、`status`（`ok/partial/unavailable`）与 `warnings`。失败时附 `error: {code, retryable}`。v1 可以增加字段，客户端必须忽略未知字段；不兼容的语义变化必须提升主版本。

| 错误码 | 含义 | 可重试 |
|---|---|---|
| `unsupported_protocol_version` | 请求了未知协议版本 | 否 |
| `external_interface_disabled` / `external_interface_settings_invalid` | 用户关闭了外部接口总开关或其配置无效（与协同接口共用） | 否 |
| `ledger_missing` | 账本文件不存在 | 否 |
| `ledger_busy` / `ledger_unavailable` | 锁争用（等待 750 ms）/ 打开或读取失败 | 是 |
| `ledger_schema_unavailable` | 该端点需要的表或列不存在 | 否 |
| `invalid_request` / `unknown_endpoint` / `method_not_allowed` / `request_too_large` / `request_timeout` / `host_not_allowed` | 请求层错误 | 否 |
| `server_busy` | 并发连接已满 | 是 |

输出中不含 SQL、路径或原始错误文本。

## 端点

### `hello`

设备身份与每个 UTC 日的内容指纹：`device_id`、`device_name`、`schema_version`、`bucket_ms`（v1 固定 3600000）、`endpoints`，以及 `fingerprints`：`{"YYYY-MM-DD" | "undated": "16 位十六进制"}`。

### `summary`

UTC 小时 × app × model 聚合。查询参数（均可选）：

- `from=YYYY-MM-DD`：只覆盖该 UTC 日及以后的日子（`undated` 始终在范围内）。
- `known=DAY:FP,DAY:FP…`：查看方已持有的指纹，最多 4096 项；`:` 与 `,` 可百分号编码。

响应 `days` 只包含指纹与 `known` 不同（或查看方没有）的日子，每项为 `{day, start_ms, fingerprint, groups}`；`removed` 列出 `known` 中在范围内但已无任何记录的日子；`unchanged` 为未重发的日子数。查看方应整体替换返回的日子、删除 `removed` 中的日子。

每个 group：`hour`（UTC 0–23，`undated` 中为 null）、`app`、`model`、`events`、`input_tokens`、`output_tokens`、`reasoning_tokens`、`cache_read_tokens`、`cache_write_tokens`（5m+1h）、`unclassified_tokens`、`credits`、`cost_usd`、`active_ms`、`duration_ms`。

语义与本机 Cube 完全一致，由等价测试固定：模型身份使用 `MODEL_EXPR`（`model → request_model → pricing_model → "?"`），头条 token 为 `input+output+cache_read+unclassified`；没有 `ts_start` 的记录归入 `undated`，只计入不限范围的总计。

**按本地时区重分桶**：桶起点 = `start_ms + hour × 3600000`，查看方本地日序号 = `(桶起点 + 本地偏移毫秒) div 86400000`。整小时偏移下结果与本机 Cube 精确一致；:30/:45 偏移时跨本地午夜的那一小时整体归入其起点所在的日子，这是小时粒度的已知近似。

**指纹**：对当天导出的 groups 按 `(hour, app, model)` 排序后做 SHA-256，取前 8 字节十六进制。它覆盖实际导出的内容，因此原位更新、模型重新归属、删除、清理/归档、重新导入都会改变指纹或使该日进入 `removed`，这是按 rowid 水位增量做不到的。注意：本机 `prune`/`archive` 移走的原始事件不再出现在对等方数据中，查看方会看到相应日子被移除，与本机概览一致。

### `quotas`

每个 app/account/window_kind 的最新一条额度记录（最多 256 条，超出 `truncated=true`）：`app`、`account_key`、`window_kind`、`captured_at`、`used`、`limit_value`、`used_percent`（仅 0–100）、`resets_at`、`value_semantics`（`allowance_percent/remaining_balance/context_usage/unknown`）、`scope="app_account_window"`。不做 TTL 判定，由查看方按 `captured_at` 与 `resets_at` 自行判断新鲜度。

额度按账号计，**绝不能跨设备相加**。`account_key` 是账号原文 SHA-256 前 16 位十六进制（与协同接口相同），同一账号在不同设备上得到同一个键，查看方据此去重并取最新的一条；这是伪名，不是不可反推的匿名。

### `sources`

每个来源（最多 128 条）：`source`、`enabled`、`last_synced_at`、`files_seen`、`rows_ingested`、`has_error`。不导出原始错误、路径或文件名。

## 隐私

只共享本机账本中的聚合。不输出提示词、会话 ID、项目名或路径、`raw_ref`、`raw_json`、账号原文、凭证与原始错误。app/model 标识可能来自不可信的会话数据，沿用协同接口的校验（长度、字符集、拒绝 URL/绝对路径/常见密钥形态）；不合规的标识替换为稳定的 `redacted-<16 位十六进制>`，数值仍计入总计，并给出 `identifier_redacted` 警告（`status=partial`）。
