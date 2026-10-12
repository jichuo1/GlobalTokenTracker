# GTT 局域网对等接口 v1

版本化只读 HTTP JSON 协议 `gtt.peer` 让同一局域网内的另一台 GTT 设备拉取本机的用量聚合，与自己的数据并列显示。对等方只交换紧凑聚合，不交换原始事件。目前有核心协议、设备身份、配对、TLS、撤销、mDNS `_gtt._tcp` 发现与按 IP 手动添加，以及便于无界面测试的 CLI；Mac/Windows 界面属于后续阶段。

## 调用与边界

```bash
# 被查看的设备（服务端）
globaltokentracker-cli peer serve                                   # 默认仅 127.0.0.1:4319，TLS
globaltokentracker-cli peer serve --bind 192.168.1.20:4319 --name "书房 Mac"   # 同时在 mDNS 上通告
globaltokentracker-cli peer serve --bind 0.0.0.0:4319 --no-advertise         # 不通告，只能按 IP 添加
globaltokentracker-cli peer list                                    # 已配对的查看方（及本机配对过的设备）
globaltokentracker-cli peer revoke <查看方 id 或名称>

# 查看方
globaltokentracker-cli peer discover --timeout 3 --format json                # 列出网络中的 GTT 设备
globaltokentracker-cli peer pair 192.168.1.20:4319 408036 --name "笔记本"
globaltokentracker-cli peer pull "书房 Mac" --endpoint summary --from 2026-10-01
globaltokentracker-cli peer pull 192.168.1.20 --endpoint hello      # 未配对时只能读 hello
globaltokentracker-cli peer pull "书房 Mac" --endpoint days --no-discover   # 跳过 mDNS，直接用上次地址
globaltokentracker-cli peer forget "书房 Mac"
```

局域网共享默认关闭：GTT 应用本身不启动对等服务，只有手动运行 `peer serve` 才会监听；默认地址是回环 `127.0.0.1:4319`。绑定任何非回环地址（包括 `0.0.0.0`）时必须使用 TLS，`--plaintext` 只允许回环地址，否则在绑定前就拒绝启动。

`serve` 启动时用常规读写方式打开账本，在 `app_state` 中生成一次稳定的 `device_id`（`peer.device_id.v1`），`--name` 存为 `peer.device_name.v1`；未指定名称时依次取 `COMPUTERNAME`、`HOSTNAME`，否则为 `GTT-<device_id 前 6 位>`。此后每个数据请求都用 SQLite 只读连接与单个读事务读取同一 WAL 快照，协议读路径从不写账本。配对与授权状态不写账本，单独保存在 `peer.db`（见下文）。

CLI 的 stdout 只有一个 UTF-8 JSON 文档。客户端侧失败（未配对、指纹不符、连接失败等）输出固定码的失败文档并以非零退出；服务端返回非 200 时也以非零退出。地址可带 `https://` 前缀，省略端口时用 4319；格式见下文“按 IP 添加”。

## 安全模型

**设备身份**：首次 `serve`（非 `--plaintext`）生成长期 ECDSA P-256 密钥与自签名证书，保存在数据目录下的 `peer/`：`device-key.der`、`device-cert.der`。证书 DER 的 SHA-256（64 位小写十六进制）就是设备身份。Unix 上目录为 0700、文件为 0600；Windows 上依赖用户配置文件目录的默认 ACL。

**TLS**：只启用 TLS 1.3（rustls + ring）。查看方不使用 CA 或 webpki 信任库，只接受配对时固定的那张证书（比较指纹），并照常校验 TLS 1.3 握手签名，证明对方持有私钥。指纹不符时在发出任何请求、令牌之前即失败，错误码 `peer_fingerprint_mismatch`。未配对时对任意证书只读 `hello`，并核对 `hello` 声称的指纹与实际证书一致。

**配对**：服务端启动时在 stderr 打印 6 位一次性配对码，按回车可重新生成。配对码有效 120 秒，成功一次即作废；每次失败（含中途断开）退避 1、2、4、8、16 秒，第 5 次失败后作废，需重新生成。同一时刻只进行一个配对。

配对在同一条 TLS 连接内进行：`GET /peer/v1/pair?name=<查看方名称>`，带 `Upgrade: gtt-pair/1`，服务端以 `101 Switching Protocols` 切换为逐行 JSON 消息。配对码不会以可离线验证的形式出现在线上：如果直接发送 `HMAC(配对码, 指纹)`，中间人拿到后可在毫秒内穷举 10⁶ 个码，再分别与两端完成配对。因此采用与蓝牙 Passkey Entry 相同的逐位承诺/揭示：

```text
T = SHA-256("gtt.peer/pair/v1" ‖ 证书指纹 ‖ 查看方名称 ‖ Ns₀ ‖ Nv₀)
第 i 轮（i = 0‥5，dᵢ 为第 i 位数字）：
  查看方 → 服务端  Cv = HMAC(Nvᵢ, "V" ‖ T ‖ i ‖ dᵢ)   查看方先承诺
  服务端 → 查看方  Cs = HMAC(Nsᵢ, "S" ‖ T ‖ i ‖ dᵢ)   服务端承诺
  查看方 → 服务端  Nvᵢ    服务端用自己的 dᵢ 验证 Cv，不符即失败
  服务端 → 查看方  Nsᵢ    查看方用自己的 dᵢ 验证 Cs，不符即失败
六轮全部通过后：服务端 → 查看方  {token, viewer_id, device_id, device_name, fingerprint}
```

每一方都在得知对方数字之前承诺、在对方承诺之后才揭示，主动中间人每转发一位都只能猜，任何一位猜错整次配对作废，总体成功概率约 10⁻⁶。`"V"/"S"` 角色标签防止把承诺原样反射回去；`T` 把配对绑定到查看方实际看到的证书，所以即使逐条忠实转发消息、但用自己证书终止 TLS 的中继也无法完成配对。查看方会记住失败过的配对码，10 分钟内拒绝再次使用（`pairing_code_reused`），因为失败可能意味着有人在截获，此时应在服务端按回车生成新码。

**令牌与授权**：配对成功后服务端生成 256 位随机持有者令牌。服务端 `peer.db` 的 `viewers` 表只存令牌的 SHA-256、查看方 id 与名称、创建时间、最后访问时间（至多每分钟更新）、撤销时间，从不保存明文；比较对所有有效记录做常数时间比较，不提前退出。查看方 `peer.db` 的 `peers` 表保存固定的证书指纹、设备 id 与名称、地址、令牌（明文，用于出示）和配对/最后拉取时间。

**撤销**：`peer revoke` 设置撤销时间；服务端每个请求都重新读取 `viewers` 表，被撤销的令牌下一次请求即返回 401 `auth_invalid`，无需重启。查看方 `peer forget` 删除本地的固定指纹与令牌。

**回环明文**：`--plaintext` 仅用于本机调试，不能配对（`tls_required`），数据端点仍需令牌；客户端只向回环地址发送明文请求（`plaintext_not_allowed`）。

## 发现与按 IP 添加

**通告**：`peer serve` 绑定非回环地址（TLS）时，在 mDNS 上通告 `_gtt._tcp`，直到进程退出；Ctrl-C 或 SIGTERM 时先发送 mDNS goodbye 撤回通告再退出。`--no-advertise` 关闭通告。绑定具体地址时只通告该地址；绑定 `0.0.0.0`/`::` 时通告本机该协议族的地址。实现使用纯 Rust 的 `mdns-sd`（自带守护线程，不需要异步运行时）。

TXT 记录只有四项，不含任何用量信息：

| 键 | 内容 |
|---|---|
| `v` | 协议版本 |
| `id` | `device_id`（32 位十六进制） |
| `name` | 显示名称（截断到 200 字节以内） |
| `fp` | 证书指纹前 16 位十六进制 |

**浏览**：`peer discover [--timeout 秒]` 输出 `devices` 列表，每项含 `device_id`、`name`、`protocol_version`、`addresses`（IPv4 在前，其次全局 IPv6，最后带 scope id 的链路本地 IPv6，可直接用于 `peer pair`）、`port`、`fingerprint_prefix`、`paired` 与 `warnings`，另有 `invalid_answers`（TXT 不合规而被忽略的应答数）。同一设备在多个网卡、多个协议族上的应答按 (`device_id`, 指纹前缀) 合并去重。警告：

- `fingerprint_mismatch`：已配对，但通告的指纹前缀不是已固定的证书；拉取会因固定指纹不符而失败。
- `duplicate_device_id`：两张证书声称同一个 `device_id`（例如账本被复制到另一台机器），分别列出。

**发现只是提示**：配对仍由配对码交换建立信任，拉取仍校验完整的固定指纹；mDNS 应答本身不被信任。

**地址变化**：对已配对设备按名称、id 或指纹拉取时，先在 mDNS 上按 `device_id` 查找（约 1.5 秒，且只接受指纹前缀与固定指纹一致的应答），再回退到上次成功的地址；每个候选地址都校验固定指纹，成功后保存新地址。`--no-discover` 跳过 mDNS。直接给出地址时按给出的地址连接。

**按 IP 添加**：mDNS 被拦截时（访客 Wi-Fi、VLAN、客户端隔离、企业网络）直接用地址配对：`host[:port]`、IPv4、IPv6（裸地址或 `[地址]:端口`），链路本地 IPv6 需带 scope id，如 `fe80::1%en0` 或 `[fe80::1%en0]:4319`（也可用接口序号）。省略端口时用 4319。

**平台说明**：

- macOS：未来的 App 需在 Info.plist 中声明 `NSLocalNetworkUsageDescription` 与 `NSBonjourServices`（`_gtt._tcp`），已加入 `macos/Resources/Info.plist` 及中英文 `InfoPlist.strings`。未获“本地网络”权限的进程发往局域网的数据（含 mDNS 组播）会被系统以 “No route to host” 拒绝，只剩回环可用；从终端运行 CLI 时，权限归属于启动它的终端类 App，需在“系统设置 › 隐私与安全性 › 本地网络”中允许。应用防火墙开启时，还需允许其接收传入连接。
- Windows：只应为“专用”网络配置文件放行 TCP 4319 与 UDP 5353（mDNS），不要放行“公用”配置文件；安装程序暂未自动添加规则。

## HTTP 约束

| 项 | 规则 |
|---|---|
| 路由 | `GET /peer/v{N}/{hello\|days\|summary\|quotas\|sources\|pair}`；`N≠1` 返回 400 `unsupported_protocol_version`，不打开账本 |
| 认证 | 除 `hello`、`pair` 外都需要 `Authorization: Bearer <令牌>`；缺失返回 401 `auth_required`，无效或已撤销返回 401 `auth_invalid`，均带 `WWW-Authenticate: Bearer` |
| 方法 | 仅 GET；其他方法 405 + `Allow: GET` |
| 请求头 | 请求行与头部合计 ≤ 64 KiB，超出 431 `request_too_large`；含 TLS 握手须在 10 秒内发完，否则 408 `request_timeout` |
| 请求体 | 不允许；`Content-Length≠0` 或 `Transfer-Encoding` 返回 413 |
| Host | 必须是 IP 字面量（可带端口）或 `localhost`，否则 403 `host_not_allowed`，用于阻止网页经 DNS 重绑定读取回环服务 |
| 并发 | 每连接一个线程，最多 16 个并发连接；超出时明文返回 503 `server_busy`，TLS 直接关闭连接 |
| 连接 | 每连接一个请求，`Connection: close`，`Cache-Control: no-store` |

请求层错误用 4xx/503 表示，配对被拒用 403（不可用、过期、需要 TLS）或 429（正忙、退避中）。账本层结果（繁忙、缺失、外部接口关闭等）一律 HTTP 200，由 `status` 与固定 `error.code` 判断，与协同接口一致。实现基于 `std::net` 与 rustls 同步流，没有 tokio/axum。

## 输出与兼容

每个响应都包含 `protocol="gtt.peer"`、整数 `protocol_version`、`gtt_version`、`operation`、毫秒 `captured_at`、`status`（`ok/partial/unavailable`）与 `warnings`。失败时附 `error: {code, retryable}`。v1 可以增加字段，客户端必须忽略未知字段；不兼容的语义变化必须提升主版本。（v1 尚未随正式版发布；`hello` 在加入认证时缩减为公开最小集合，日指纹移到需认证的 `days`。）

| 错误码 | 含义 | 可重试 |
|---|---|---|
| `unsupported_protocol_version` | 请求了未知协议版本 | 否 |
| `external_interface_disabled` / `external_interface_settings_invalid` | 用户关闭了外部接口总开关或其配置无效（与协同接口共用） | 否 |
| `ledger_missing` | 账本文件不存在 | 否 |
| `ledger_busy` / `ledger_unavailable` | 锁争用（等待 750 ms）/ 打开或读取失败 | 是 |
| `ledger_schema_unavailable` | 该端点需要的表或列不存在 | 否 |
| `auth_required` / `auth_invalid` / `auth_unavailable` | 缺少令牌 / 令牌无效或已撤销 / 授权库不可读 | 否 / 否 / 否 |
| `tls_required` | 在明文服务上请求配对 | 否 |
| `pairing_unavailable` / `pairing_code_expired` / `pairing_busy` / `pairing_locked` | 没有有效配对码 / 已过期 / 另一配对进行中 / 失败后退避中 | 否 / 否 / 否 / 否 |
| `pairing_failed` / `pairing_aborted` / `pairing_refused` | 配对码不符或疑似截获 / 交换中断 / 被拒且无具体原因 | 否 |
| `pairing_code_reused` / `invalid_code` | 查看方拒绝重用失败过的码 / 码不是 6 位数字 | 否 |
| `not_paired` / `ambiguous_peer` / `unknown_peer` | 目标未配对 / 名称匹配多台设备 / forget 找不到设备 | 否 |
| `unknown_viewer` / `ambiguous_viewer` | revoke 找不到 / 名称匹配多个查看方（请用 id） | 否 |
| `peer_fingerprint_mismatch` | 对方证书与固定指纹不符，未发送任何请求 | 否 |
| `connect_failed` / `tls_failed` / `plaintext_not_allowed` / `malformed_response` | 连接失败（含所有候选地址均不可达）/ TLS 握手失败 / 向非回环地址发明文 / 响应格式错误 | 否 |
| `invalid_request` / `unknown_endpoint` / `method_not_allowed` / `request_too_large` / `request_timeout` / `host_not_allowed` | 请求层错误 | 否 |
| `server_busy` | 并发连接已满 | 是 |

输出中不含 SQL、路径或原始错误文本；客户端只回显上表中的固定码。

## 端点

### `hello`（公开）

只有身份：`device_id`、`device_name`、`fingerprint`（服务端证书指纹；明文服务上未经证明）、`endpoints`。不读取账本。

### `days`

日指纹索引：`device_id`、`schema_version`、`bucket_ms`（v1 固定 3600000）与 `fingerprints`：`{"YYYY-MM-DD" | "undated": "16 位十六进制"}`。查看方先比较它，再按需拉取 `summary`。

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

只向已配对的查看方共享本机账本中的聚合。不输出提示词、会话 ID、项目名或路径、`raw_ref`、`raw_json`、账号原文、凭证与原始错误。app/model 标识可能来自不可信的会话数据，沿用协同接口的校验（长度、字符集、拒绝 URL/绝对路径/常见密钥形态）；不合规的标识替换为稳定的 `redacted-<16 位十六进制>`，数值仍计入总计，并给出 `identifier_redacted` 警告（`status=partial`）。
