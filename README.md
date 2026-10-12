<div align="center">

<img src="assets/icon.png" width="96" height="96" alt="GlobalTokenTracker">

# GlobalTokenTracker

**聚合本机 AI 编码工具用量的桌面应用 · Windows（WinUI 3）与 macOS（SwiftUI）**

**简体中文** · [English](README_EN.md)

<br>

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-00AEEC?style=flat-square&labelColor=2d3a55)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/Rust-1.90-DEA584?style=flat-square&logo=rust&logoColor=white&labelColor=2d3a55)](https://www.rust-lang.org)
[![Windows](https://img.shields.io/badge/Windows-10%201809%2B-0078D4?style=flat-square&logo=windows&logoColor=white&labelColor=2d3a55)](#环境要求)
[![macOS](https://img.shields.io/badge/macOS-26%2B-000000?style=flat-square&logo=apple&logoColor=white&labelColor=2d3a55)](#环境要求)
[![UI](https://img.shields.io/badge/UI-WinUI%203%20%C2%B7%20SwiftUI-00AEEC?style=flat-square&labelColor=2d3a55)](#环境要求)
[![Release](https://img.shields.io/github/v/release/jichuo1/GlobalTokenTracker?include_prereleases&style=flat-square&label=release&labelColor=2d3a55)](https://github.com/jichuo1/GlobalTokenTracker/releases)
[![Issues](https://img.shields.io/github/issues/jichuo1/GlobalTokenTracker?style=flat-square&labelColor=2d3a55)](https://github.com/jichuo1/GlobalTokenTracker/issues)

[功能](#功能) · [支持的源](#支持的源) · [环境要求](#环境要求) · [安装与更新](#安装与更新) · [使用](#使用) · [局域网多设备](#局域网多设备查看预览) · [构建](#构建项目) · [隐私](#隐私说明)

</div>

---

<details>
<summary><b>目录</b></summary>

- [项目简介](#项目简介)
- [功能](#功能)
- [支持的源](#支持的源)
- [环境要求](#环境要求)
- [安装与更新](#安装与更新)
- [使用](#使用)
- [局域网多设备查看（预览）](#局域网多设备查看预览)
- [构建项目](#构建项目)
- [隐私说明](#隐私说明)
- [许可证](#许可证)
- [致谢](#致谢)

</details>

---

## 项目简介

GlobalTokenTracker 是一个桌面程序（当前版本 0.8.0，支持 Windows 与 macOS），用来统一统计本机各 AI 编码工具的用量：token 计数、按价目表估算的美元成本、订阅配额/额度余量、延迟与模型分布。它只读取各工具写在磁盘上的本地日志与状态文件做增量统计，数据全部存放在本机 SQLite 账本中，过程不产生网络上报。

Windows 版界面采用 WinUI 3 原生实现（Rust + windows-reactor），macOS 版是原生 SwiftUI 应用；两者共用同一套 Rust 核心（适配器、计价、账本），同一份日志在两个平台上得出相同的数字。目标是小体积、冷启动快、长期常驻托盘/菜单栏稳定。

---

## 功能

<table>
<tr>
<td width="50%" valign="top">

#### 统计视图

- 总览 / 明细 / 配额 / 数据源 / 价格 五页视图
- 工具 + 模型级联筛选，选择持久化
- 30 日 token 趋势条（Direct2D 按需绘制）
- 单事件粒度明细：模型、各桶 token、成本、耗时、来源文件

</td>
<td width="50%" valign="top">

#### 计量与计价

- 非缓存输入 / 缓存读 / 缓存写 / 输出 分桶精确计量
- 价目表**多方佐证**：9 个公开来源（models.dev、LiteLLM、llmpricing.dev、OpenRouter、Vercel AI Gateway、Helicone、Langfuse、llm-prices.com、Portkey）每 12h 同步；同一模型以**多数来源一致的价格**为准，单个来源的错价（如某源把 gpt-5 标成半价）不会进账
- 价格页可**按模型搜索**（忽略大小写与分隔符：`opus 4.6`、`gpt mini`），每行显示“几家来源一致 / 几家报价”，悬停看各家报价；可只看有分歧的模型
- 离线种子兜底，断网仍可计价
- USD 估算与订阅 credits/百分比严格分列，不混算

</td>
</tr>
<tr>
<td width="50%" valign="top">

#### 可靠性与数据安全

- 增量字节游标扫描：日志追写只读新段，截断/轮转自动识别
- 账本双代快照备份，文件丢失/损坏打开时自愈
- 只读扫描，绝不回写任何源文件
- 配额历史有界化（30 天滚动，每键保最新行）

</td>
<td width="50%" valign="top">

#### 体验与集成

- 定时刷新 / 仅文件变更监视 双模式
- 系统托盘常驻，开始菜单快捷方式，可选加入用户 PATH
- 安装器原位升级：自定义目录记忆、运行进程自动处理
- OTLP/HTTP 接收器（`127.0.0.1:4318`）接入支持遥测的工具
- 设置中的「外部协同接口」统一控制协同查询与配套调度器入口，关闭后本地用量统计继续工作

</td>
</tr>
<tr>
<td width="50%" valign="top">

#### macOS 原生应用

- SwiftUI 窗口 + 菜单栏常驻（可在菜单栏显示今日成本、隐藏 Dock 图标、登录时启动）
- 页面切换为“滚筒翻页”动画，可连续翻过中间页、随时打断
- 自定义统计区间：日历 + 24 小时表盘选择起止时间
- 设置页：界面字号、主题与强调色、图表字体、刷新频率、更新通道与自动检查更新
- 图表经 Metal 一次光栅化（GPU 绘制），翻页与滚动更顺滑
- 应用内更新：下载新版 DMG 并打开，拖入“应用程序”即可

</td>
<td width="50%" valign="top">

#### 局域网多设备查看（预览）

- 同一局域网内的设备互相只读查看用量聚合，**默认关闭**
- 一次性 6 位配对码 + TLS 1.3 证书固定，随时撤销
- Bonjour/mDNS 自动发现，也可直接按 IP 添加
- macOS 应用的“设备”页可查看、配对、同步与筛选设备；共享本机与 Windows 界面暂用 CLI——见[下文](#局域网多设备查看预览)

</td>
</tr>
</table>

---

## 支持的源

| 工具 | 数据形态 | 计量级别 |
|---|---|---|
| Claude Code | 本地 JSONL 会话日志 | ✅ token 精确 + 配额（凭据可用时） |
| Codex | 本地会话日志 | ✅ 精确 + 配额 |
| Devin CLI | 本地会话库 `devin/cli/sessions.db` | ✅ 精确（含 TTFT） |
| OpenCode / ZCode / Grok / WorkBuddy | 本地日志 | ✅ 精确 |
| MiniMax Code / Kimi Code | 本地日志 | ✅ 精确 |
| DeepSeek Harness（CLI / DSH Desktop） | 本地会话 `~/.dsh/sessions/…` | ✅ 精确 |
| Cline | `ui_messages.json` + 编辑器 globalStorage | ✅ 精确（含工具自报成本） |
| Command Code | 本地 JSONL | ✅ 精确（含工具自报成本） |
| Antigravity（应用 / IDE / `agy` CLI） | 本地会话库 `~/.gemini/antigravity*/…/*.db` | ✅ 精确（含思考 token；无缓存写数据） |
| CodeBuddy IDE | `state.vscdb` / 会话库 | 🟡 会话级 + 模型倍率 |
| Cursor / Qoder | 凭据 + 官方配额 API | 🟡 配额为主 |

> [!NOTE]
> 未安装的源自动跳过并在"数据源"页标记；工具删本地数据不影响已入账统计（账本在源之外）。精确度以"数据允许"为上限——源文件里没有的字段绝不编造。

---

## 环境要求

**Windows**

| 项 | 要求 |
|---|---|
| 系统 | Windows 10 **1809+**（build 17763），x64 |
| 运行时 | Windows App Runtime 1.5+——**安装程序自动检测并引导安装**（首次需联网，可能弹一次 UAC） |
| 权限 | 不需要管理员，装到 `%LOCALAPPDATA%\Programs\GlobalTokenTracker` |
| 磁盘 | 约 20 MB |

**macOS**

| 项 | 要求 |
|---|---|
| 系统 | macOS **26** 及以上 |
| 架构 | 通用二进制，Apple 芯片与 Intel 均可 |
| 权限 | 不需要管理员，拖入“应用程序”即可 |
| 磁盘 | 约 10 MB |

---

## 安装与更新

### Windows

从 [Releases](https://github.com/jichuo1/GlobalTokenTracker/releases) 下载 `GlobalTokenTracker-Setup-<版本>-win-x64.exe`：

```
双击                  → 图形界面安装（可选目录/快捷方式/PATH）
--quiet               → 静默安装
--dir D:\Tools\GTT    → 自定义目录
--uninstall           → 卸载（保留 %USERPROFILE%\.globaltokentracker 用户数据）
```

**升级**：直接运行更新版本的安装程序即可原位更新——会记住你上次的安装目录（包括自定义目录），自动结束运行中的进程，更新卸载注册项。更新过程不碰账本与配置。

> [!WARNING]
> 安装包当前未签名，SmartScreen 会提示"Windows 已保护你的电脑"——点"更多信息 → 仍要运行"即可。下载后可在 Release 页核对 SHA-256。

### macOS

从 [Releases](https://github.com/jichuo1/GlobalTokenTracker/releases) 下载 `GlobalTokenTracker-<版本>-macos-universal.dmg`，打开后把 GlobalTokenTracker 拖到“应用程序”。账本位于 `~/.globaltokentracker`，与 CLI 共用。

**升级**：应用内检查到新版本后下载新的 DMG 并打开，拖入“应用程序”覆盖即可；账本与设置不受影响。

> [!WARNING]
> 未使用 Developer ID 签名与公证的版本，首次打开会被 Gatekeeper 拦截——在“系统设置 › 隐私与安全性”中选择“仍要打开”即可。可在 Release 页核对 SHA-256。

---

## 使用

GUI 开箱即用（Windows：开始菜单 → GlobalTokenTracker；macOS：“应用程序”或菜单栏图标）。Windows 安装包同时附带 CLI（安装时勾选"加入用户 PATH"后可直接调用）；macOS 的 DMG 只含应用，CLI 需从源码构建（见[构建项目](#构建项目)）：

「设置 → 外部协同 → 外部协同接口」可同时关闭协同观察与新模型调度。已经发出的请求允许完成并正常计量，排队、重试与后台恢复探测不再发出新请求。若使用配套模型调度器，请同时更新调度器与 Skill；新版调度器要求调用方先启动真实 Luna 监工并登记同一 workflow 的有效租约。完整安装包包含 GUI、CLI 和安装器，模型调度器单独部署。

```
globaltokentracker-cli scan             # 手动全量扫描
globaltokentracker-cli prices --update  # 强制刷新价目表
globaltokentracker-cli quota            # 立即轮询配额通道
globaltokentracker-cli prune --keep-days 90      # 重建汇总后裁剪 90 天前的明细
globaltokentracker-cli export all --out out.csv  # 导出明细 CSV（all/today/week/month）
```

刷新频率与刷新模式（定时 / 仅文件变更）在界面"设置"里调整。

---

## 局域网多设备查看（预览）

让同一局域网内的几台电脑互相**只读**查看用量，在一处并列看到所有设备。被查看的设备只提供按小时汇总的聚合（token、成本、额度、数据源状态），从不发送原始事件、会话、项目名或路径；额度按账号计，不会跨设备相加。

- **默认关闭**：应用不会自行开启，只有手动运行 `peer serve` 才会共享；默认只监听本机回环 `127.0.0.1:4319`，绑定局域网地址时强制 TLS 1.3。
- **配对**：服务端打印一次性 6 位配对码（120 秒有效、只能用一次、错误后退避，5 次失败作废）。配对码逐位承诺验证，截获流量也无法离线穷举；配对后查看方固定对方证书指纹，服务端只保存令牌的哈希。
- **撤销**：服务端 `peer revoke` 立即生效；查看方 `peer forget` 删除本地凭据。
- **发现**：共享时在 Bonjour/mDNS 上通告 `_gtt._tcp`（只含名称、设备 id、指纹前缀，不含用量），退出时撤回。访客 Wi-Fi、VLAN 或客户端隔离拦截 mDNS 时，直接按 IP 添加（IPv4/IPv6，链路本地地址带 `%网卡`）。已配对设备换了 IP 会先按设备 id 重新发现。

```bash
# 被查看的设备
globaltokentracker-cli peer serve --bind 0.0.0.0:4319 --name "书房 Mac"   # 打印配对码，回车换新码
globaltokentracker-cli peer list                                          # 已配对的查看方
globaltokentracker-cli peer revoke 笔记本

# 查看方
globaltokentracker-cli peer discover                                      # 列出网络中的设备
globaltokentracker-cli peer pair 192.168.1.20:4319 408036 --name 笔记本
globaltokentracker-cli peer pull "书房 Mac" --endpoint summary
globaltokentracker-cli peer sync                                          # 把已配对设备的聚合同步进本机账本
globaltokentracker-cli report week --device all                           # 本机 + 各设备合计与分设备明细
globaltokentracker-cli peer forget "书房 Mac"
```

macOS 应用已支持查看：侧边栏“设备”页列出本机与已配对设备（同步状态、计入开关、忘记）、可能的重复计算与附近设备，“添加设备”用地址和配对码配对；配对后工具栏多出“设备”筛选，总览新增“按设备”卡片，配额页显示来自其他设备的额度。共享本机目前仍需 CLI `peer serve`，Windows 界面即将推出。同步来的数据单独存放（不写入本机事件表、也不会再共享出去），在统计中作为“设备”维度与本机并列，额度按账号取最新、不相加。协议与安全模型详见 [docs/PEER_API.md](docs/PEER_API.md)。

> [!NOTE]
> macOS 需要“本地网络”权限，Windows 只应在“专用”网络放行 TCP 4319 与 UDP 5353。

---

## 构建项目

```powershell
git clone https://github.com/jichuo1/GlobalTokenTracker.git
cd GlobalTokenTracker

# 需要 Rust 1.90+（edition 2024）与 Windows SDK
cargo build --release --workspace   # 产出 ui/cli/setup 三个 exe
cargo test --workspace              # 测试
cargo clippy --all-targets -- -D warnings

# 打包单文件安装程序（内嵌 payload → dist\）
powershell -ExecutionPolicy Bypass -File installer\package.ps1
```

macOS（需要 Rust 与 Swift 6.2+，Command Line Tools 即可，无需 Xcode）：

```bash
macos/scripts/build-app.sh --universal                  # target/macos/GlobalTokenTracker.app
macos/scripts/build-dmg.sh v0.8.0                       # 安装用磁盘映像
cargo build --release -p globaltokentracker-cli         # CLI：target/release/globaltokentracker-cli
```

详见 [macos/README.md](macos/README.md)。

签名属可选增强：`installer/sign.ps1` 支持证书指纹 / PFX（测试）/ 云签 dlib 三种模式，经环境变量驱动，详见 [docs/SIGNING.md](docs/SIGNING.md)。

---

## 隐私说明

- 价目表同步只向上述 9 个公开来源发 **GET**，不携带任何用量、路径或账号信息（每 12 小时一次 + 每次启动一次）；断网时回落内置离线种子，功能不受影响。另有版本更新检查（GitHub Releases）与可选的官方配额查询，均只读取、不上传日志内容。
- **监听端口**：默认只有 OTLP 接收器，**仅监听 `127.0.0.1:4318`**，只收本机回环，不接受局域网连接。
- **局域网共享默认关闭**：只有手动运行 `peer serve` 才会打开对等服务（默认 `127.0.0.1:4319`）；绑定局域网地址时强制 TLS 1.3，只有用一次性配对码配对过的设备才能读取只读用量聚合（不含会话、项目、路径）；查看方固定对方证书指纹，服务端只存令牌哈希，可随时 `peer revoke` 撤销。
- **mDNS 通告只在共享期间存在**：内容仅为名称、设备 id 与证书指纹前缀，退出时撤回；`peer discover` 只在你运行时监听。详见 [docs/PEER_API.md](docs/PEER_API.md)。
- 所有源日志**只读**，应用不会修改、删除或上传任何工具的数据文件。
- 账本与配置存于 `%USERPROFILE%\.globaltokentracker`（macOS：`~/.globaltokentracker`），卸载和升级均保留；程序目录内不存任何用户数据。

---

## 许可证

按 **MIT OR Apache-2.0** 双许可证分发，任选其一遵守：[LICENSE-MIT](LICENSE-MIT) · [LICENSE-APACHE](LICENSE-APACHE)。

---

## 致谢

调研与实现过程中参考（未直接引用代码）：

- [cc-switch](https://github.com/farion1231/cc-switch) · [TokenTracker](https://github.com/xiufengsun/TokenTracker) · [cursor-usage](https://github.com/chocolatemale/cursor-usage) · [tokcat](https://github.com/handlecusion/tokcat) · [agent-trail](https://github.com/camtrik/agent-trail)
- [windows-reactor](https://crates.io/crates/windows-reactor)（Rust ↔ WinUI 3 反应式绑定）
- 价目数据来源：[models.dev](https://models.dev) · [LiteLLM](https://github.com/BerriAI/litellm) · [llmpricing.dev](https://llmpricing.dev) · [OpenRouter](https://openrouter.ai/models) · [Vercel AI Gateway](https://vercel.com/ai-gateway) · [Helicone](https://www.helicone.ai/llm-cost) · [Langfuse](https://github.com/langfuse/langfuse) · [llm-prices.com](https://www.llm-prices.com) · [Portkey](https://github.com/Portkey-AI/models)
