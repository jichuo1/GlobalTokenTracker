# 原生 agent 接入

生产代码只使用 Python 标准库和使用者已经安装的原生软件。不下载新 CLI、不复制软件登录凭证，也不更改原项目、全局模型或权限配置。

`scripts/register_agents.py` 默认输出注册计划，`--apply` 备份后原子更新用户协调器配置；保留既有来源的显式选择与禁用状态。`source_registry` 展示已安装、已注册与未接入原因，软件存在不代表已有执行接口。

DSH 属于可能消耗自有 API 余额的来源，默认只记录已安装，不自动加入可执行调度。只有显式 `--allow-paid-dsh` 才读取其 profile 元数据；已有禁用状态仍保留，执行需另行显式启用。来源被禁用后也不参加恢复探针。

| 适配器 | 原生入口 | 范围 |
|---|---|---|
| `opencode_http` | 独立 OpenCode CLI 的官方 Server API | 全部已连接 provider 下的文本模型，保留同名模型的不同 provider，选择最高已枚举推理变体 |
| `claude_cli` | Claude Code print/stream-json | 固定原生模型或原生家族别名，不自动切换模型 |
| `codebuddy_cli` | WorkBuddy 内置 CodeBuddy headless CLI | 软件自身公布的模型列表；不重复注册同一运行时以虚增容量 |
| `dsh_headless` | DSH Desktop 内置 DeepSeek Harness headless profile | 当前官方 profile 的模型与临时覆盖，保持 factory 服务，禁用已知工具入口 |

OpenCode 目录来自 `/config/providers`，不只复制默认 model，也不从未认证全库假定可用。目录最多 1024 个模型，缓存 30 分钟；强制刷新 inventory 会重读。默认模型与 agent 角色单独记录，角色不算独立模型/并发槽位。弃用、禁用或明确无文本输出的模型被跳过。目录不是逐模型生成证明。

每次 OpenCode 调用在稳定空槽位启动自己的随机端口 loopback server，使用随机 HTTP Basic 密码、禁用代理和已配置 MCP。临时 agent 与 session 权限禁止工具/委派，检查有效权限后显式提交 provider/model/variant。返回身份须与请求一致；工具事件、截断、无完整结果均失败。其他来源通过 stdin 传任务，关闭工具与 MCP，取消只结束本次进程树。原生插件与认证仍由原客户端执行；这些边界不等于操作系统沙箱。

DSH 按当前 profile 的公开 schema 建立临时 patch，不写回原 profile。禁用工具、MCP、环境说明与技能注入，保留主 worker 必需的 factory。模型、effort、maxTokens、cwd 显式设置；usage 纳入预算。模型选择证据是已核对进程覆盖，不伪称提供商的加密证明。未知第三方 profile 被拒绝。

来源并发、降权、恢复、尝试与 token 预算沿用共享服务。新来源不提高既有总并发；隐藏开销和截断响应只能事后计量。不同来源可能共享供应商账户，软件并发不是供应商额度承诺。多 provider 客户端缺少可靠账号映射时保持 unknown，不套用整个软件的一份余额。

Windows 上，Antigravity CLI 的目录发现和 worker 经 `hidden_console_worker.py` 启动桥接：在 GTT 专用进程中关闭 CLI 自动更新与交互启动，将 worker 放到从不切换为输入桌面的专用桌面，并保留一个隐藏控制台；原有 stdin/stdout/stderr 直接传给 CLI。启动主线程前先绑定 Windows Job，桥接结束或被强制结束时清理所属 CLI 树。桥接命令可在旧驻留服务热加载，包装具有幂等性。其他来源沿用 `CREATE_NO_WINDOW`，驻留服务沿用 `DETACHED_PROCESS`。此设置只作用于 GTT 自己启动的进程，不改变系统默认终端。系统级 GUI broker、第三方显式切回输入桌面等情况仍需另行定位，这不是操作系统沙箱。

排查不定期终端闪现可运行 `scripts/watch_windows_console.ps1`。默认观察一小时后自动停止，输出仅写入当前用户的私有诊断目录。它立即保存终端/Electron 窗口事件和此前 20 秒的进程启动快照，不读窗口标题或命令行；不同窗口桌面、短于取样周期的进程和控制台委派可能限制原始调用方的识别。

AA 数据按精确版本/effort 匹配。Opus 5 max 使用公开模型页参考；Muse contributor 仅采用明确标出的模型家族先验，未冒充该提供商路线的实测成绩。未公开的 Terminal-Bench 数字不填。短标记探针证明接口、身份与报告契约；逻辑能力仍需实际任务反馈。

参考：[OpenCode Server](https://opencode.ai/docs/server/)、[OpenCode permissions](https://opencode.ai/docs/permissions/)、[Claude Code CLI](https://code.claude.com/docs/en/cli-reference)、[CodeBuddy headless](https://www.codebuddy.ai/docs/cli/headless)。
