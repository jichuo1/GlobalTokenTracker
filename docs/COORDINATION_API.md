# GTT 协同观察接口 v1

版本化 CLI JSON 协议 `gtt.coordination` 为调度器提供来源观测、历史模型与额度信号。GTT 负责数据语义，调度器负责调用、容量、健康和任务分配，Codex 主线负责验收。没有新增生产依赖或常驻 HTTP 服务。

## 调用与边界

```powershell
globaltokentracker-cli coordination capabilities --format json
globaltokentracker-cli coordination snapshot --format json --protocol-version 1
globaltokentracker-cli --db <账本路径> coordination snapshot --source zcode --lookback-days 14
globaltokentracker-cli coordination refresh --source zcode --format json
globaltokentracker-cli coordination refresh --source codex --quota --format json
globaltokentracker-cli coordination refresh --all --format json
```

`capabilities` 不打开数据库，不发现用户文件。`snapshot` 不创建、迁移、恢复备份或改名旧目录；没有当前账本时可只读旧 `.codeledger/ledger.db`。支持 `GTT_DATA_DIR` 和全局 `--db`。

`refresh` 必须指定 `--source` 或 `--all`，会打开/迁移本地账本、扫描启用来源并更新聚合。默认不轮询网络额度；`--quota` 单独授权选定来源的原生额度通道及其原有认证刷新行为。目前网络额度仅支持 Codex/Cursor；其他来源返回 `unsupported`，不访问这两个供应商。禁用来源不扫描、不轮询；来源日志扫描可以导入日志本身已有的额度记录。

## 输出与兼容

刷新先取得 OS 文件锁，再打开/迁移账本；刷新入口不会因锁争用或损坏而自动恢复备份。损坏账本返回固定失败码，保留原文件供显式修复。GTT 原有管理入口的恢复行为保持不变。

成功解析的调用 stdout 只有一个 UTF-8 JSON 文档。业务不可用仍退出 0，由 `status` 和固定 `error.code` 判断；命令语法错误由 Clap 返回非零和 stderr。原有 CLI 命令保留。

响应包含 `protocol`、整数 `protocol_version`、`gtt_version`、`operation`、毫秒 `captured_at`、`status`、`warnings`。状态为 `ok/partial/unavailable`。v1 可增加字段，客户端忽略未知字段；不兼容的语义必须增加主版本。请求未知版本返回 `unsupported_protocol_version`，不开数据库。

| 快照段 | 含义 |
|---|---|
| `sources` | 启用状态、最后扫描时间、计数与 `has_error`；不导出原始错误 |
| `models` | 最近 1–90 天 app/provider/model 聚合、样本、最后观察和平均 duration/TTFT |
| `quotas` | 每 app/account/window 最新记录，按时间和 ID 处理并列 |
| `observation` | 同一事务、回看天数、额度 TTL、执行可用性未知、延迟未归一 |
| `schema_version` | 当前 migration 最大版本或 null，与协议版本分别标识 |
| `truncated` | 结果被截断的段名，有限结果不能当作全集 |

来源最多 128 条，模型最多 512 条，额度最多 256 条，后两项可通过 `--model-limit/--quota-limit` 降低。缺表/缺列只影响相应段并返回 partial；全无可读段返回 `ledger_schema_unavailable`。损坏、访问失败、锁争用返回固定码，不输出 SQL、路径或认证响应。锁等待 750 ms；聚合 SQL 没有独立硬时限，调用方应设置进程截止时间。调度器默认 5 秒，终止并回收超时进程。

SQLite 只读连接与一个读事务固定同一 WAL 快照。读取不改账本内容；SQLite 必要时可能维护共享内存边文件。机器结构参考 [coordination-v1.schema.json](schemas/coordination-v1.schema.json)。

## 额度与隐私

`remaining_percent` 只接受 `5h_block/weekly/monthly/daily/api_pool/auto_pool` 的明确 0–100 用量百分比。默认 TTL 300 秒，可设 1–86400 秒。过期、未来时间、已过重置点、非法百分比均为 null。未知不是 0，也不是无限。

Codex `credits.used` 存的是剩余余额，标记 `value_semantics=remaining_balance` 并单独输出新鲜 `remaining_balance`，不换算百分比。`session_ctx` 是上下文使用量。额度只适用于 `app_account_window`，不是模型级；消费者必须匹配账号，不能把其他账户余额转给当前 CLI。

不输出提示词、会话、项目/源文件路径、账号原文、`raw_json`、凭证和原始错误。账号使用 SHA-256 前 16 个十六进制字符关联，这是伪名而非不可反推匿名。模型/provider 限制长度和字符，拒绝 URL、绝对路径与常见密钥形态；不合规 provider 变为不透明键，不合规模型行被省略并给出警告。

## 刷新互斥与恢复

canonical ledger 对应的 OS 文件锁持有整段刷新生命周期。不得删除/替换锁文件；文件存在不代表已加锁。使用 Rust 1.89 起提供的标准库接口，兼容项目最低 Rust 1.90，平台行为见 [官方文档](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock)。

`app_state` 保存 owner、心跳与完成时间；默认全局冷却 30 秒，可设 1–3600 秒。竞争返回 `refresh_busy/refresh_throttled`。暂停或心跳 SQL 失败时 OS 锁仍保护扫描；失败后不再开启后续来源/网络，当前扫描返回后标记中断，清理等待当前工作完成。进程退出后系统释放锁，可立即回收尚未到期的孤儿租约。删除自己租约和写冷却在同一事务中提交；不删除其他 owner。

互斥仅适用于本协议 refresh；原有 UI 扫描和普通 scan 未参加此锁。已有同步扫描/HTTP 调用没有中途强制取消契约。

## 消费者与扩展

调度器优先使用本协议。CLI 缺失、旧版不支持、版本不兼容或传输异常时可退回只读 SQLite，记录 `origin.transport=legacy_sqlite` 和固定 `fallback_reason`；配置可关闭回退。有效 unavailable 响应被保留，不自行绕过。每次任务排名重新检查额度时间，目录缓存不延长 TTL。

新增 SourceAdapter 自动出现在 metering capabilities；新增网络额度须显式扩展 poller 与语义测试。历史扫描/模型出现不证明生成可用。新增执行软件仍需注册调度器适配器并进行真实生成和权限验收。
