//! Runtime UI language. `t!`/`tf!` resolve the active locale from a
//! thread-local set once per render (`set_lang` in `Shell::view`); keys are
//! the zh literals already used in the code, so a missing table entry falls
//! back to Chinese instead of blanking out. English templates keep `{}` /
//! `{name}` placeholder *order* identical so `tf!` can substitute
//! positionally.
//!
//! Design note: core-side formatters that emit zh text (`Range::label`,
//! `quota_kind_label`, `已过期`) are wrapped with `tr` at the UI call site;
//! `compact()` swaps 万/亿 for K/M/B because suffix math can't be retexted.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    Zh,
    En,
}

impl Lang {
    pub fn from_config(s: &str) -> Self {
        match s {
            "en" => Self::En,
            _ => Self::Zh,
        }
    }
}

/// Render-locale — a plain atomic (not thread-local) because D2D draw
/// closures may run on whichever thread the compositor paints on.
static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set_lang(l: Lang) {
    CURRENT.store(l as u8, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::En,
        _ => Lang::Zh,
    }
}

/// Compact token count honoring the active locale: 万/亿 in zh, K/M/B in en.
pub fn compact(n: u64) -> String {
    if lang() == Lang::Zh {
        return globaltokentracker_core::viewmodel::fmt::tokens_compact(n);
    }
    let trim = |v: f64| {
        let s = format!("{v:.1}");
        s.trim_end_matches(".0").to_string()
    };
    if n >= 1_000_000_000 {
        format!("{}B", trim(n as f64 / 1e9))
    } else if n >= 1_000_000 {
        format!("{}M", trim(n as f64 / 1e6))
    } else if n >= 10_000 {
        format!("{}K", trim(n as f64 / 1e3))
    } else {
        globaltokentracker_core::viewmodel::fmt::tokens_exact(n)
    }
}

/// Positional template fill — every `{…}` group (empty or named) consumes the
/// next argument in order. Table entries preserve placeholder order.
pub fn tf(tpl: &str, args: &[&dyn std::fmt::Display]) -> String {
    let tpl = tr(tpl);
    let mut out = String::with_capacity(tpl.len() + 16);
    let mut rest = tpl;
    let mut i = 0;
    while let Some(open) = rest.find('{') {
        let Some(close_rel) = rest[open..].find('}') else {
            break;
        };
        let close = open + close_rel;
        out.push_str(&rest[..open]);
        if let Some(arg) = args.get(i) {
            out.push_str(&arg.to_string());
        } else {
            out.push_str(&rest[open..=close]);
        }
        i += 1;
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// zh literal → English. Unknown keys pass through as Chinese (never blank).
/// Generic over lifetime so runtime-produced strings (`fmt::until`, quota
/// kind labels) translate alongside literals.
pub fn tr(zh: &str) -> &str {
    if lang() == Lang::Zh {
        return zh;
    }
    en(zh)
}

/// The English table behind `tr` — pure, so tests need not flip the global
/// render locale.
pub fn en(zh: &str) -> &str {
    match zh {
        // ---- nav & page titles
        "总览" => "Overview",
        "明细" => "Details",
        "配额" => "Quota",
        "数据源" => "Sources",
        "价格" => "Prices",
        "价目表（$/1M tokens）" => "Price list ($/1M tokens)",
        "设置" => "Settings",
        // ---- overview
        "{rl} Tokens" => "{rl} Tokens",
        "{rl}估算成本" => "{rl} est. cost",
        "{rl}缓存读" => "{rl} cache reads",
        "{rl}事件" => "{rl} events",
        "{rl}趋势" => "{rl} trend",
        "{rl} · 按工具" => "{rl} · by tool",
        "{rl} · 占比分布" => "{rl} · share breakdown",
        "事件 {}" => "{} events",
        "全部 {}" => "All-time {}",
        "输入 {}" => "Input {}",
        "未分类" => "Unclassified",
        "活跃 {}" => "Active {}",
        "今日 · 按小时" => "Today · hourly",
        "全部 · 按天（近 60 桶）" => "All · daily (last 60 buckets)",
        "暂无数据" => "No data yet",
        "暂无配额信号" => "No quota signals",
        "订阅配额" => "Subscription quota",
        "未计价模型" => "Unpriced models",
        "{} — 请在价格页补充覆写" => "{} — add overrides on the Prices page",
        "估算" => "Est.",
        "上移" => "Move up",
        "下移" => "Move down",
        "隐藏" => "Hide",
        "恢复" => "Restore",
        "已隐藏：{}" => "Hidden: {}",
        "完成" => "Done",
        "布局" => "Layout",
        "刷新" => "Refresh",
        "正在扫描数据源…" => "Scanning sources…",
        // ---- filter chrome
        "工具" => "Tool",
        "模型" => "Model",
        "工具筛选" => "Tool filter",
        "模型筛选" => "Model filter",
        "刷新频率" => "Refresh cadence",
        "全部" => "All",
        "今日" => "Today",
        "近 7 天" => "Last 7 days",
        "近 30 天" => "Last 30 days",
        "自定义" => "Custom",
        "自定义 · 按天" => "Custom · by day",
        "二" => "Tue",
        "四" => "Thu",
        "六" => "Sat",
        "日" => "Sun",
        "{} 年 {} 月" => "{}/{}",
        "已选区间" => "Selected range",
        "共 {} 天 · {}" => "{} days · {}",
        "共 {} 小时 · {}" => "{} h · {}",
        "自定义 · 按小时" => "Custom · hourly",
        "按时段" => "By hour",
        "全天" => "All day",
        "{} 小时" => "{} h",
        "点击起始整点，再点击结束整点" => "Click a start hour, then an end hour",
        "再点一个整点作为另一端" => "Click another hour for the other end",
        "选择单日后可按时段统计" => "Pick a single day to filter by hour",
        "点击起始日，再点击结束日" => "Click a start day, then an end day",
        "再点一个日期作为另一端" => "Click another day for the other end",
        "快捷选择" => "Quick pick",
        "昨天" => "Yesterday",
        "本周" => "This week",
        "本月" => "This month",
        "上月" => "Last month",
        "近 90 天" => "Last 90 days",
        "未选" => "None",
        "已选 {}/{total}" => "{}/{total} selected",
        "全选" => "Select all",
        "清空" => "Clear",
        "仅文件变更" => "File changes only",
        "10 秒" => "10 s",
        "30 秒" => "30 s",
        "1 分钟" => "1 min",
        "5 分钟" => "5 min",
        // ---- detail page
        "时间" => "Time",
        "输入" => "Input",
        "输出" => "Output",
        "缓存" => "Cache",
        "缓存读" => "Cache read",
        "缓存写" => "Cache write",
        "成本" => "Cost",
        "时长" => "Duration",
        "来源" => "Source",
        "← 上一页" => "← Prev",
        "下一页 →" => "Next →",
        "第 {} / {} 页 · 共 {} 条" => "Page {} / {} · {} entries",
        "{}  ·  {} 事件" => "{}  ·  {} events",
        "{} tok  ·  {}" => "{} tok  ·  {}",
        "{} tok · {}" => "{} tok · {}",
        "{} · {} tok" => "{} · {} tok",
        "第 {} / {} 项" => "#{} of {}",
        "{} · {}" => "{} · {}",
        "{} · {} 项配额" => "{} · {} quotas",
        "{}…" => "{}…",
        // ---- quota page
        "配额组 {}" => "Quota group {}",
        "余额 {}" => "Balance {}",
        "已用 {} / 上限 {}" => "Used {} / limit {}",
        "用量 {}" => "Used {}",
        "上限 {}" => "Limit {}",
        "重置 {}" => "Resets {}",
        "采集 {}" => "Collected {}",
        "另有 {} 项 · 见配额页" => "{} more · see the Quota page",
        "暂无配额数据" => "No quota data",
        "5 小时窗口" => "5-hour window",
        "每周限额" => "Weekly limit",
        "每月限额" => "Monthly limit",
        "剩余点数" => "Credits",
        "计费周期" => "Billing period",
        "Auto 用量池" => "Auto pool",
        "API 用量池" => "API pool",
        "会话上下文" => "Session context",
        "已过期" => "Expired",
        // ---- sources page
        "尚未扫描" => "Not scanned yet",
        "正常" => "OK",
        "⚠ {e}" => "⚠ {e}",
        "{} 文件 · 累计 {} 行 · 游标 {} · 上次 {}" => {
            "{} files · {} rows ingested · {} cursors · last {}"
        }
        // ---- prices page
        "{} 个模型 · {}" => "{} models · {}",
        "匹配 {} / {} 个模型 · {}" => "{} of {} models match · {}",
        "搜索模型，如 opus 4.6 或 gpt mini" => "Search models, e.g. opus 4.6 or gpt mini",
        "仅看有分歧的" => "Disputed only",
        "没有匹配的模型" => "No matching models",
        "佐证" => "Sources",
        "与所示价格一致的来源数 / 给出报价的来源总数。橙色 = 来源之间有分歧；悬停行查看各家报价。" => {
            "Sources backing the shown price / sources that quoted one. Orange = the sources disagree; hover a row for each quote."
        }
        " · 缓存读 {}" => " · cache read {}",
        " · 缓存写 {}" => " · cache write {}",
        "采信 {}（{}/{} 家一致）" => "Taken from {} ({}/{} agree)",
        "无报价" => "no price",
        "{}（未计票）" => "{} (not counted)",
        "联网同步于 {} 小时前" => "Synced {}h ago",
        "仅本地种子，尚未联网同步" => "Local seed only; not synced yet",
        // ---- tray
        "GlobalTokenTracker — 今日 {}" => "GlobalTokenTracker — today {}",
        "显示 GlobalTokenTracker" => "Show GlobalTokenTracker",
        "隐藏到托盘" => "Hide to tray",
        "退出" => "Quit",
        // ---- widget registry (widgets.rs titles resolve through tr too)
        "统计卡" => "Stats",
        "近 30 天趋势" => "30-day trend",
        "占比分布" => "Share breakdown",
        "费用" => "Cost",
        "按模型" => "By model",
        "按工具" => "By tool",
        "其他" => "Other",
        "本周 · 按工具" => "This week · by tool",
        "未计价提示" => "Unpriced notice",
        "部件" => "Widget",
        // ---- settings page
        "外观" => "Appearance",
        "主题模式" => "Theme",
        "跟随系统" => "System",
        "浅色" => "Light",
        "深色" => "Dark",
        "主题色" => "Accent color",
        "字体" => "Font",
        "界面字号" => "Font size",
        "通用" => "General",
        "外部协同" => "External coordination",
        "外部协同接口" => "External coordination interface",
        "接口状态" => "Interface status",
        "关闭后停用协同查询和新模型调度，不影响用量统计；已发出的模型请求允许完成" => {
            "Disable coordination queries and new model dispatches without affecting usage tracking; requests already sent may finish"
        }
        "接口设置读取失败，外部协同已关闭" => {
            "Interface settings could not be read; external coordination is disabled"
        }
        "接口设置保存失败，已显示实际状态" => {
            "Interface settings could not be saved; the actual state is shown"
        }
        "语言" => "Language",
        "开机自启动" => "Launch at login",
        "登录 Windows 后自动启动（最小化到托盘）" => {
            "Start with Windows (minimized to tray)"
        }
        "点击关闭按钮时" => "When closing",
        "开" => "On",
        "关" => "Off",
        "每次询问" => "Ask every time",
        "彻底退出" => "Quit completely",
        "仅作用于图表文字；界面控件字体跟随系统" => {
            "Applies to chart text; controls follow the system font"
        }
        "主题模式、配色与字号" => "Theme mode, colors and font size",
        "界面语言与启动行为" => "Interface language and startup behavior",
        // ---- activity heatmap
        "活跃热力图" => "Activity heatmap",
        // ---- prices refresh
        "刷新价目" => "Refresh prices",
        "同步中…" => "Syncing…",
        "上次刷新失败：{}" => "Last refresh failed: {}",
        // ---- trend moving average
        "折线趋势" => "Line trend",
        "移动平均" => "moving avg",
        "7 日移动平均 {} tok" => "7-day moving avg {} tok",
        "7 小时移动平均 {} tok" => "7-hour moving avg {} tok",
        "计费" => "Cost",
        "调用" => "Calls",
        "少" => "Less",
        "多" => "More",
        "一" => "Mon",
        "三" => "Wed",
        "五" => "Fri",
        "近一年 {} 天活跃 · 最长连续 {} 天 · 合计 {}" => {
            "{} active days in the past year · longest streak {} days · total {}"
        }
        "{} 次调用" => "{} calls",
        "时长 {}" => "Duration {}",
        "{} 小时 {} 分" => "{} h {} min",
        "{} 分" => "{} min",
        "{} 秒" => "{} s",
        "时长为调用耗时之和；Claude Code / Codex 为按日志时间戳推算" => {
            "Duration is the sum of call times; Claude Code / Codex are estimated from log timestamps"
        }
        // ---- updates
        "更新" => "Updates",
        "当前版本" => "Current version",
        "更新渠道" => "Update channel",
        "正式版" => "Stable",
        "预览版" => "Preview",
        "预览版包含尚未正式发布的新功能，可能不稳定" => {
            "Preview builds include unreleased features and may be unstable"
        }
        "自动检查更新" => "Check for updates automatically",
        "启动时及每 24 小时检查一次" => "At startup and every 24 hours",
        "更新状态" => "Update status",
        "检查中…" => "Checking…",
        "已是最新版本" => "You are up to date",
        "发现新版本 {}" => "New version {} available",
        "正在下载并校验…" => "Downloading and verifying…",
        "检查失败：{}" => "Update check failed: {}",
        "立即检查" => "Check now",
        "立即更新" => "Update now",
        "查看更新说明" => "Release notes",
        "更新失败：{}" => "Update failed: {}",
        "重试" => "Retry",
        "默认" => "Default",
        "默认（Segoe UI）" => "Default (Segoe UI)",
        "蓝色" => "Blue",
        "绿色" => "Green",
        "紫色" => "Purple",
        "橙色" => "Orange",
        "红色" => "Red",
        "中文" => "中文",
        // ---- close prompt dialog
        "关闭 GlobalTokenTracker" => "Close GlobalTokenTracker",
        "要彻底退出，还是隐藏到托盘继续后台统计？" => {
            "Quit completely, or keep stats running in the system tray?"
        }
        "记住我的选择" => "Remember my choice",
        "托盘图标不可用" => "Tray icon unavailable",
        "取消" => "Cancel",
        // ---- LAN devices + sharing
        "仅本机事件" => "This PC's events only",
        "设备" => "Devices",
        "设备筛选" => "Device filter",
        "按设备" => "By device",
        "{rl} · 按设备" => "{rl} · by device",
        "本机" => "This PC",
        "{}（本机）" => "{} (this PC)",
        "{} · 已筛除" => "{} · filtered out",
        "{} · 来自 {}" => "{} · from {}",
        "部分设备可能重复计算了同一份用量——详见“设备”。" => {
            "Some devices may count the same usage twice — see Devices."
        }
        "来自其他设备" => "From other devices",
        "每个账号取最新的一条；额度绝不跨设备相加。" => {
            "The newest report per account wins; quotas are never added up across devices."
        }
        "立即同步" => "Sync now",
        "正在同步…" => "Syncing…",
        "在“设置 → 局域网共享”中开启后，其他设备即可与本机配对并查看本机用量。" => {
            "Turn on Settings → LAN sharing to let other devices pair with this PC and see its usage."
        }
        "设备 ID {}" => "Device ID {}",
        "已配对的设备" => "Paired devices",
        "尚未配对任何设备。在另一台设备上开始共享以获得配对码，然后在下方添加设备。" => {
            "No paired devices yet. Start sharing on the other device to get a pairing code, then add it below."
        }
        "同步于 {}" => "Synced {}",
        "尚未同步" => "Not synced yet",
        "版本 {}" => "Version {}",
        "计入" => "Include",
        "忘记…" => "Forget…",
        "忘记“{}”？" => "Forget “{}”?",
        "忘记设备" => "Forget device",
        "将从本机删除它已同步的用量。如需再次查看，请用新的配对码重新配对。" => {
            "Its synced usage is removed from this PC. To see it again, pair with a new code."
        }
        "可能重复计算" => "Possible double counting",
        "{} 与 {} 有 {} 天的 {} 用量完全相同，可能扫描了同一个同步文件夹；合计中两者仍都计入。" => {
            "{} and {} match on {} days of {} usage — they may scan the same synced folder; totals still count both."
        }
        "添加设备" => "Add device",
        "在另一台设备上开始共享，它会显示一个 6 位配对码，2 分钟内有效且只能使用一次。" => {
            "Start sharing on the other device. It shows a six-digit code that is valid for two minutes and works once."
        }
        "配对码" => "Pairing code",
        "配对" => "Pair",
        "正在配对…" => "Pairing…",
        "已与 {} 配对" => "Paired with {}",
        "附近" => "Nearby",
        "查找设备" => "Find devices",
        "正在查找设备…" => "Looking for devices…",
        "其身份与该 ID 的已配对设备不一致——拉取将被拒绝。" => {
            "Its identity differs from the paired device with this ID — pulls will be refused."
        }
        "另一台设备使用了相同的 ID（账本被复制过？）。" => {
            "Another device uses the same ID (a copied ledger?)."
        }
        "已配对" => "Paired",
        "用此地址配对" => "Use this address",
        "选择“查找设备”以搜索此网络中正在共享的 GlobalTokenTracker 设备。" => {
            "Choose Find devices to look for GlobalTokenTracker devices sharing on this network."
        }
        "没有列出？访客 Wi-Fi、VLAN 和客户端隔离会阻止发现——请改用地址添加设备，例如 192.168.1.20:4319。" => {
            "Not listed? Guest Wi-Fi, VLANs and client isolation block discovery — add the device by its address instead, e.g. 192.168.1.20:4319."
        }
        "设备之间只共享按小时汇总的用量，从不共享提示词、会话、项目或路径。额度按账号计算，绝不跨设备相加。" => {
            "Devices share hourly totals only — never prompts, sessions, projects or paths. Quotas are per account and never added up across devices."
        }
        "局域网共享" => "LAN sharing",
        "共享本机用量" => "Share this PC's usage",
        "开启后，同一局域网中已配对的设备可只读查看本机按小时汇总的用量（TLS 加密，需配对码）" => {
            "Paired devices on this network can read this PC's hourly usage totals (TLS-encrypted, pairing code required)"
        }
        "正在启动或停止共享…" => "Starting or stopping sharing…",
        "共享失败：{}" => "Sharing failed: {}",
        "本机地址" => "This PC's address",
        "在另一台设备的“设备 → 添加设备”中填写此地址" => {
            "Enter this address under Devices → Add device on the other device"
        }
        "本机 IP:{}" => "this PC's IP:{}",
        "有效至 {}" => "valid until {}",
        "新配对码" => "New code",
        "生成配对码" => "Create a pairing code",
        "2 分钟内有效，只能使用一次；连续输错 5 次后作废" => {
            "Valid for two minutes and works once; five wrong tries void it"
        }
        "证书指纹 {}…（对方配对后固定此身份）" => {
            "Certificate fingerprint {}… (paired devices pin this identity)"
        }
        "已配对的查看方" => "Paired viewers",
        "撤销后立即失效，需重新配对才能再次查看" => {
            "Revoked viewers lose access at once and must pair again"
        }
        "撤销" => "Revoke",
        "配对于 {} · 最近访问 {}" => "Paired {} · last seen {}",
        "Windows 防火墙只应对“专用”网络放行 TCP 4319 与 UDP 5353；安装程序会添加对应规则（需要管理员确认）。" => {
            "Windows Firewall should allow TCP 4319 and UDP 5353 on Private networks only; the installer adds these rules (with administrator approval)."
        }
        "配对码为 6 位数字。" => "The pairing code is six digits.",
        "配对码不匹配。请在另一台设备上生成新的配对码后重试。" => {
            "The code didn't match. Show a new code on the other device and try again."
        }
        "此配对码已失败过一次。请在另一台设备上生成新的配对码。" => {
            "This code already failed once. Show a new code on the other device."
        }
        "配对码已过期。请在另一台设备上生成新的配对码。" => {
            "The code expired. Show a new code on the other device."
        }
        "另一台设备当前没有有效的配对码。" => {
            "The other device has no active pairing code."
        }
        "尝试次数过多。请稍等几秒再试。" => {
            "Too many attempts. Wait a few seconds, then try again."
        }
        "另一台设备正在与其他设备配对，请稍后再试。" => {
            "The other device is pairing with someone else. Try again shortly."
        }
        "这是本机。" => "That's this PC.",
        "配对中断，请重试。" => "Pairing was interrupted. Try again.",
        "该设备未通过安全连接共享。" => {
            "That device isn't sharing over a secure connection."
        }
        "无法连接该设备。请检查地址，并确认它正在共享。" => {
            "Couldn't reach the device. Check the address and that it is sharing."
        }
        "无法与该设备建立安全连接。" => {
            "Couldn't set up a secure connection to the device."
        }
        "此地址上响应的是另一台设备，其身份与已配对的设备不符。" => {
            "A different device answered at this address. Its identity doesn't match the paired one."
        }
        "该设备已不再接受本机，请重新配对。" => {
            "The device no longer accepts this PC. Pair again."
        }
        "未配对。" => "Not paired.",
        "网络中只允许安全连接。" => {
            "Only secure connections are allowed on the network."
        }
        "该设备响应超时。" => "The device took too long to answer.",
        "同步失败。" => "Sync failed.",
        "该设备未配对。" => "That device isn't paired.",
        "有多台设备匹配。" => "More than one device matches.",
        "找不到该查看方。" => "No such viewer.",
        "该设备已关闭外部接口。" => {
            "The external interface is turned off on that device."
        }
        "该设备的账本正忙，请稍后再试。" => {
            "The device's ledger is busy. Try again shortly."
        }
        "该设备运行的版本不兼容。" => "That device runs an incompatible version.",
        _ => zh,
    }
}

/// `&T → &dyn Display` unsizing helper for `tf!` (cast syntax is brittle here).
#[doc(hidden)]
pub fn as_display<T: std::fmt::Display>(v: &T) -> &dyn std::fmt::Display {
    v
}

/// Literal → localized `&'static str`.
#[macro_export]
macro_rules! t {
    ($s:literal) => {
        $crate::i18n::tr($s)
    };
}

/// Localized format template + positional args (`Display`).
#[macro_export]
macro_rules! tf {
    ($s:literal) => {
        $crate::i18n::tf($s, &[])
    };
    ($s:literal, $($a:expr),+ $(,)?) => {
        $crate::i18n::tf($s, &[$( $crate::i18n::as_display(&$a) ),+])
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single test — `CURRENT` is a global atomic, parallel cases would race.
    #[test]
    fn locales() {
        set_lang(Lang::Zh);
        assert_eq!(tr("总览"), "总览");
        assert_eq!(crate::heat::fmt_span(30_000), "30 秒");
        assert_eq!(
            crate::heat::fmt_span((3 * 3600 + 12 * 60) * 1000),
            "3 小时 12 分"
        );
        assert_eq!(crate::heat::month_label(3), "3月");
        assert_eq!(tf!("第 {} / {} 页", 1, 3), "第 1 / 3 页");

        set_lang(Lang::En);
        assert_eq!(tr("总览"), "Overview");
        assert_eq!(
            tf!("第 {} / {} 页 · 共 {} 条", 1, 3, 42),
            "Page 1 / 3 · 42 entries"
        );
        assert_eq!(tf!("{rl} · 按工具", "近 7 天"), "近 7 天 · by tool");
        assert_eq!(compact(64_425), "64.4K");
        assert_eq!(compact(1_730_848_235), "1.7B");
        assert_eq!(crate::heat::fmt_span(45 * 60_000), "45 min");
        assert_eq!(
            crate::heat::fmt_span((3 * 3600 + 12 * 60) * 1000),
            "3 h 12 min"
        );
        assert_eq!(crate::heat::month_label(3), "Mar");
        set_lang(Lang::Zh);
    }
}
