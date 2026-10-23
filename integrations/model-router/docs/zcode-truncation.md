# ZCode 输出截断调查

本机历史样本中，35 次 ZCode 路由尝试包含 10 次成功、11 次 output_truncated、3 次超时、10 次认证失败、1 次请求失败。截断来自 workspace/generateText 返回的 finishReason=length，与网络超时、结果回收的字符裁剪是不同机制。

已核对安装的 ZCode 实现：RPC maxOutputTokens 进入 generateText 的 options.maxOutputTokens；本次 Step 线路实际请求为 Messages 格式，max_tokens 与 RPC 上限一致，并携带 thinking.type=adaptive 和 output_config.effort=xhigh。没有发现 ZCode 擅自缩小上限的证据。

## 同路由、同任务对照

仅改变输出硬上限，保持同一提供商、模型、最高确认思考档位和短逻辑任务。该任务只检查一段人工提供的玩具函数，无项目源码、工具调用或凭证。

| 输出上限 | 实际输出 tokens | 可见正文字符 | 思考字符计数 | 结束原因 |
|---:|---:|---:|---:|---|
| 1024 | 1024 | 0 | 4418 | length |
| 4096 | 1105 | 786 | 3777 | stop |

上限较低的请求只生成了思考内容，在正文前耗尽总输出预算。提高上限后正常结束，实际用量低于上限。两次总用量分别为 1441 和 1522 tokens，失败和成功均由 GTT 自动入账一笔。实验有随机性，不能由两个样本推出任何模型的通用最小上限。

部分历史请求在 8192 或 10000 上限仍截断，说明长或多问题的任务在最高思考档位下也可能耗尽这些预算；历史失败记录缺少分维度用量，不能倒推每次具体思考占比。上下文窗口也不等于单次输出上限。

## 诊断改动与处理原则

截断异常新增严格白名单诊断：请求输出上限、供应商上报输出/思考 token 数、可见正文字符数、结束原因。没有上报思考计数时保持缺失，不猜数字。正文、思考内容、headers、凭证、供应商原始对象均不进入 public 错误详情。计量路径保持完整。

最高思考档位按已有要求保留。避免仅为了缩短最终报告而设置过小的总输出预算；有逻辑要求的请求要预留思考与正文空间，并把多问题拆成独立小片段。预算紧时选择合适的其他来源或显式回退，不无限重试同一上限。全局上限和调用方预算未因本次调查自动放大。

验证：108 项 Python 测试通过；只读 review 未发现新增诊断泄密或序列化问题；真实对照确认原生请求上限、finish reason、正文/思考字符计数；两次调用 GTT 入账各一笔。

供应商关于共同输出预算的说明：[OpenRouter reasoning tokens](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens)、[Messages adaptive thinking](https://platform.claude.com/docs/en/build-with-claude/adaptive-thinking)。后者解释该 API 格式的语义，不能替代对 Step 线路具体实现的实测。
