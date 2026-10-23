#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! globaltokentracker-ui — WinUI 3 shell via windows-reactor.
//! Dumb renderer over core ViewModels; theme + layout are data (`ui.json`
//! next to ledger.db), so skins / widget ordering survive without recompiles.
//!
//! Release builds are GUI-subsystem — no stray console window. Diagnostics
//! (`diag!`, panic stderr) only exist when GTT_DEBUG=1; then we attach to the
//! parent console, or allocate one for a double-clicked debug launch.

mod autostart;
mod close_hook;
mod config;
mod devices;
mod fonts;
mod gpu_slide;
mod heat;
mod i18n;
mod pages;
mod scheduling;
mod theme;
mod tray;
mod trend;
mod updater;
mod watch;
mod widgets;

use config::{REFRESH_OPTIONS, UiConfig};
use globaltokentracker_core::adapters;
use globaltokentracker_core::power;
use globaltokentracker_core::store::{
    EventRow, PriceRow, SourceHealth, default_db_path, filter_prices,
};
use globaltokentracker_core::update::Channel;
use globaltokentracker_core::viewmodel::fmt;
use globaltokentracker_core::viewmodel::{Range, day_start_ms};
use globaltokentracker_core::{Cube, Engine, OverviewVm, Store};
use gpu_slide::{Flight, LayerHost, MAX_SLIDE, Phase, Slide};
use i18n::tr;
use pages::*;
use std::path::PathBuf;
use std::sync::Arc;
use theme::Theme;
use updater::UpdateState;
use windows_reactor::*;

/// Result of one background refresh. `Unchanged` means the scan ingested
/// nothing — the ~150ms of aggregate queries and the whole-view rebuild are
/// skipped, and only the scan-time indicator updates.
pub enum LoadOutcome {
    Fresh(Box<Snapshot>),
    Unchanged { scan_ms: u128, price_due: bool },
}

/// One background refresh produces this bundle (all Send-safe plain data).
/// The sources/prices tables are NOT part of it — they load on their own
/// (`PageData`), so a scan never gates them and they never gate a scan.
pub struct Snapshot {
    pub vm: OverviewVm,
    /// In-memory aggregates behind `vm` — range / tool / model changes are
    /// re-folded from it (microseconds) instead of re-querying the ledger.
    /// Shared: a refresh clones the `Arc`, never the groups.
    pub cube: Arc<Cube>,
    /// `Shell::filter_gen` this snapshot's detail rows were loaded under.
    pub filter_gen: u64,
    pub detail: DetailBundle,
    /// A price refresh is due (startup force or >12h stale) — run it as its
    /// own background task so network latency never gates the first paint.
    pub price_due: bool,
    pub scan_ms: u128,
    /// False for the startup snapshot, which is built from the ledger as it
    /// already is — the first frame doesn't wait for a scan; one follows.
    pub scanned: bool,
    /// Display-currency status captured with this snapshot — the settings
    /// page renders it; `fmt` already converts money under the same rate.
    pub fx: globaltokentracker_core::fx::FxStatus,
}

/// `Arc` rows: the virtualized table's row closure owns a cheap handle
/// instead of copying up to a page of strings on every `view()`.
pub struct DetailBundle {
    pub rows: Arc<Vec<EventRow>>,
    pub total: u64,
    pub page: i64,
}

/// Price table + the live-source sync stamp it was read with.
#[derive(Clone)]
pub struct PriceTable {
    pub rows: Arc<Vec<PriceRow>>,
    /// Live-source sync timestamp (ms); `None` = seed only.
    pub synced_at: Option<i64>,
}

/// Page-scoped table data, loaded by a light query — no filesystem scan, no
/// overview/detail re-aggregation. Prefetched after first paint and
/// refreshed on nav, so opening the page never waits on (or races) a scan.
pub enum PageData {
    Sources(Vec<SourceHealth>),
    Prices(PriceTable),
}

/// Index into the per-page load bookkeeping (`None` = not a lazy page).
fn lazy_slot(page: Page) -> Option<usize> {
    match page {
        Page::Sources => Some(0),
        Page::Prices => Some(1),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Overview,
    Speed,
    Detail,
    Quota,
    Devices,
    Sources,
    Scheduling,
    Prices,
    Settings,
}

pub struct Shell {
    scheduler: scheduling::State,
    snap: Option<Snapshot>,
    /// Sources / prices tables — independent of `snap`, see `PageData`.
    sources: Option<Vec<SourceHealth>>,
    prices: Option<PriceTable>,
    /// Price-page search: what was typed, the "disputed only" toggle, and the
    /// rows they select. The text box is fed `price_query` back on every
    /// render (it has to be: the framework records the text it observed, so a
    /// box left without a `text` would be cleared by the very next render).
    price_query: String,
    price_disputed: bool,
    price_shown: Arc<Vec<PriceRow>>,
    /// Per lazy page (`lazy_slot`): a load is running / was asked for again
    /// meanwhile (re-run on completion instead of racing two queries).
    page_loading: [bool; 2],
    page_again: [bool; 2],
    /// First snapshot landed → sources/prices were prefetched once.
    prefetched: bool,
    /// Overview charts allowed to mount so far — see `OverviewArgs`.
    canvas_ready: usize,
    /// Measured row width of the virtualized tables (DIPs) — see
    /// `pages::TableArgs`. Seeded from the default window until the rulers
    /// report, so the first frame is already close.
    table_w: f64,
    /// Width rulers for the detail / prices table cards.
    table_rulers: [ElementRef<SwapChainPanel>; 2],
    page: Page,
    scanning: bool,
    /// A filesystem event arrived while a scan was running — rescan when it ends.
    pending_rescan: bool,
    last_error: Option<String>,
    config: UiConfig,
    external_interface_enabled: bool,
    external_interface_error: Option<String>,
    theme: Theme,
    editing: bool,
    /// Kept alive for the process lifetime; `!Send`, stays on the UI thread.
    tray: Option<tray_icon::TrayIcon>,
    /// Trend-chart hover state + repaint handle (shared with the D2D closure).
    trend: widgets::TrendHandle,
    /// Activity-heatmap hover state + repaint handle.
    heat: widgets::HeatHandle,
    /// Custom-range calendar: month on show and the first click awaiting its
    /// partner (`None` = next click starts a new range).
    cal_month: (i16, i8),
    cal_anchor: Option<jiff::civil::Date>,
    /// Hour dial: first clicked hour awaiting its partner.
    clock_anchor: Option<i8>,
    /// Hour dial: virtualized animated state handle.
    clock_dial: widgets::ClockDialHandle,
    /// Share-donut hover states — one per share-grid column (6 max) plus the
    /// speed donut (index 6).
    donuts: [widgets::DonutHandle; 7],
    /// Vendor quota channels poll at this cadence (network calls stay rare).
    quota_at: Option<std::time::Instant>,
    /// Overview statistics window (persisted in ui.json).
    range: Range,
    /// Checked tools for the stats filter; `None` = all (persisted in ui.json).
    app_filter: Option<Vec<String>>,
    /// Checked display-model names; `None` = all (persisted in ui.json).
    model_filter: Option<Vec<String>>,
    /// Which filter-strip dropdown is open (in-content overlay, not a system
    /// Flyout — so no FlyoutPresenter surface stroke/shadow halo).
    open_menu: Option<MenuKind>,
    /// Quota page: app groups the user folded away (default all expanded).
    quota_collapsed: std::collections::BTreeSet<String>,
    /// A background price fetch is in flight — prevents overlapping pulls
    /// when consecutive scans all report `price_due`.
    prices_refreshing: bool,
    /// Why the last price refresh produced nothing (shown on the Prices page).
    price_refresh_error: Option<String>,
    fx_refreshing: bool,
    fx_refresh_error: Option<String>,
    speed_by_app: bool,
    speed_metric: SpeedMetric,
    speed_page: usize,
    /// Visible aggregates must rebuild even if the next scan lands nothing —
    /// set by filter/range/page changes, quota polls and repricing.
    views_stale: bool,
    /// The next load rebuilds the aggregation cube from scratch (manual
    /// refresh, repricing changed old rows) instead of refreshing touched days.
    cube_rebuild: bool,
    /// Bumped on every tool/model filter change — a load that started under an
    /// older value has stale detail rows and must re-fetch them.
    filter_gen: u64,
    /// Overview width tier (1..=`pages::WIDTH_TIERS`) — driven by the width
    /// ruler's Metrics events (4 until the first measurement lands).
    overview_cols: usize,
    /// Page-switch in progress — the new page's top-level blocks spring in
    /// (each with its own damping/velocity) while the old page is pushed out
    /// on a second layer; all of it runs on the compositor (`gpu_slide`).
    /// `None` when at rest.
    flight: Option<Flight>,
    flight_seq: u64,
    /// Two page layers take turns showing the page: `rest_idx` is the one
    /// showing it at rest; a flight mounts the new page in the other, slides
    /// both, then the old one unmounts (see `view`). Each layer has its
    /// composition host and one host per block wrapper (first `MAX_SLIDE`).
    layer_hosts: [LayerHost; 2],
    block_hosts: [[LayerHost; MAX_SLIDE]; 2],
    rest_idx: usize,
    /// Blocks in the current page's last build (how many block hosts must be
    /// ready before a flight can start).
    block_count: std::cell::Cell<usize>,
    /// Close prompt dialog open (title-bar X swallowed by close_hook).
    close_prompt: bool,
    /// "记住我的选择" checkbox inside the close prompt — persists whichever
    /// button the user then picks into `config.close_action`.
    close_remember: bool,
    /// 1-DIP full-width ruler panel on the overview page; its surface
    /// metrics report the real content width for adaptive grids.
    ruler: ElementRef<SwapChainPanel>,
    update: UpdateState,
    /// Bumped whenever the auto-check chain is re-armed — a timer carrying an
    /// older value belongs to a cancelled chain and is ignored.
    update_gen: u64,
    /// Bumped whenever the periodic refresh timer is re-armed or cancelled.
    refresh_gen: u64,
    /// Bumped whenever the source file watcher is re-armed or cancelled.
    watch_gen: u64,
    update_banner_dismissed: bool,
    /// An update-check request is in flight (auto checks leave `update`
    /// untouched, so `Checking` alone can't guard against overlap).
    update_checking: bool,
    /// Tag the banner was last shown for — a newer tag re-shows it.
    update_seen_tag: Option<String>,
    /// Last download/install failure — shown on the banner and settings row.
    update_error: Option<String>,
    /// LAN devices: this PC + paired devices (`devices::load`).
    devices: Option<devices::DevicesInfo>,
    /// Devices found over Bonjour (`Find devices`).
    nearby: Vec<globaltokentracker_core::peer::discovery::Found>,
    peer_syncing: bool,
    /// The running sync was asked for (button / pairing), so it shows.
    peer_sync_shown: bool,
    discovering: bool,
    /// Last device-level failure (sync / discover / forget), as a code or text.
    peer_error: Option<String>,
    /// Devices page "Add device" form.
    pair: devices::PairForm,
    /// Forget confirmation open for (device id, name).
    forget_target: Option<(String, String)>,
    /// Statistics device filter (persisted in ui.json as `devices`).
    device_filter: Option<Vec<String>>,
    /// Settings "LAN sharing": the running share and what it shows.
    share: devices::ShareState,
    /// Bumped whenever the peer-sync timer chain is re-armed.
    peer_gen: u64,
}

/// Which filter-strip picker is open — `Tools`/`Models` are multi-select
/// checkbox lists, `Refresh` is single-select cadence radios.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    Tools,
    Models,
    /// LAN devices (shown once another device is paired).
    Devices,
    Refresh,
}

pub enum Msg {
    Scheduling(scheduling::Action),
    Loaded(LoadOutcome),
    Failed(String),
    Tick(u64),
    Rescan,
    WatchFired(u64),
    WatchFallback(u64),
    Tray(tray::TrayAction),
    Nav(Option<String>),
    DetailPage(i64),
    DetailLoaded(Arc<Vec<EventRow>>, u64, i64),
    /// A light page-table query finished (`Page` says which slot to free).
    PageData(Page, Result<PageData, String>),
    ToggleEdit,
    MoveWidget(String, String, i32),
    HideWidget(String, String, bool),
    /// Pointer x (canvas-local DIPs) over the trend chart.
    TrendHover(f64),
    /// Dwell timer fired for bar `usize` — arms the tooltip if still hovering.
    TrendTip(usize),
    TrendLeave,
    /// Pointer (canvas-local DIPs) over the activity heatmap.
    HeatHover(f64, f64),
    /// Dwell timer fired for heatmap day `usize`.
    HeatTip(usize),
    HeatLeave,
    /// Heatmap metric picked — tokens|cost|calls|duration.
    SetHeatMetric(&'static str),
    /// Prices page "刷新价目" — pull every price feed now.
    RefreshPrices,
    /// Trend card: moving-average overlay toggle.
    SetTrendLine(bool),
    /// Share-donut pointer hover: (column index, hovered slice or None).
    DonutHover(u8, Option<usize>),
    /// Statistics range changed — resolved to `Range` at the selector so
    /// localized labels never leak into state handling.
    SetRange(Range),
    /// "自定义" selector item picked — adopt stored bounds (else last 7 days).
    PickCustomRange,
    /// Inline calendar: a day was clicked (civil year, month, day).
    CalPick(i16, i8, i8),
    /// Inline calendar: show the previous (-1) or next (+1) month.
    CalMonth(i8),
    /// Inline calendar quick chip ("昨天"|"本周"|"本月"|"上月"|"近 90 天").
    CalQuick(&'static str),
    /// Hour dial: an hour slot (0..=23) of the single selected day clicked.
    ClockPick(i8),
    /// Hour dial: back to the whole selected day.
    ClockFullDay,
    /// Tool checkbox toggled (app name, new checked state).
    ToggleApp(String, bool),
    /// Bulk tool-scope set from the filter flyout — `None` = all tools,
    /// `Some(vec![])` = deliberately empty view.
    SetApps(Option<Vec<String>>),
    /// Model checkbox toggled (display-model name, new checked state).
    ToggleModel(String, bool),
    /// Bulk model-scope set — `None` = all models, `Some(vec![])` = empty view.
    SetModels(Option<Vec<String>>),
    /// Refresh-cadence pick from the picker flyout (seconds).
    SetRefreshSecs(u64),
    /// Filter-strip pill clicked — opens its overlay, or closes it when the
    /// same one is already open; a different picker's overlay replaces it.
    ToggleMenu(MenuKind),
    /// Pointer pressed outside an open picker (click-away backdrop / a
    /// filter-strip label) — light-dismiss.
    CloseMenu,
    /// Quota group header clicked — fold/unfold the app's quota windows.
    ToggleQuotaGroup(String),
    /// Width-ruler observer: overview grids should use this many columns.
    SetOverviewCols(usize),
    /// Table-card ruler: virtualized rows must span this many DIPs.
    SetTableWidth(f64),
    /// Prices page: the search box text changed.
    PriceQuery(String),
    /// Prices page: "only models the sources disagree on" toggled.
    PriceDisputed(bool),
    /// Post-slide stagger: let the next Overview chart mount.
    CanvasStage,
    /// Flight `id`: new page is mounted — start the compositor animations
    /// (polled until every host's visual has resolved).
    NavGo(u64),
    /// Flight `id` has run its course — drop the leaving layer.
    NavSettled(u64),
    /// Event sink for RadioButton uncheck transitions — nothing to do.
    Noop,
    /// Settings: window theme — "system" | "light" | "dark".
    SetThemeMode(&'static str),
    /// Settings: accent override — "" restores the theme default.
    SetAccent(&'static str),
    /// Settings: D2D chart font family — "" restores Segoe UI. `String`
    /// because the picker lists system fonts discovered at runtime.
    SetFontFamily(String),
    /// Settings: body font size (pt); title/h2/label derive from it.
    SetFontSize(f64),
    /// Settings: UI language — "zh" | "en".
    SetLang(&'static str),
    /// Settings: display currency for money figures (ISO code; "" = USD).
    SetDisplayCurrency(String),
    RefreshFx,
    SpeedGroup(bool),
    SpeedMetric(SpeedMetric),
    SpeedPage(usize),
    FxDone(Result<globaltokentracker_core::fx::FxStatus, String>),
    /// Settings: Run-key launch-at-login toggle; arg is the switch's new state.
    SetAutostart(bool),
    SetExternalInterface(bool),
    /// Settings: close-button behavior — "" = ask, "quit", "tray".
    SetCloseAction(&'static str),
    /// Title-bar X swallowed by the window subclass — open the ask dialog
    /// (or apply the remembered action directly).
    CloseRequested,
    /// Close prompt dismissed — arg says which button ended it.
    CloseDialogResult(ContentDialogResult),
    /// "记住我的选择" checkbox toggled inside the close prompt.
    CloseRemember(bool),
    /// Background quota poll finished (rows written, channel errors).
    QuotaDone(usize, Vec<String>),
    /// Background price-source fetch finished — repriced>0 triggers one
    /// follow-up scan so newly-priced events show their USD.
    PricesDone(Result<globaltokentracker_core::pricing::RefreshReport, String>),
    /// Settings "立即检查" (manual) or the scheduled check (auto).
    CheckUpdate {
        manual: bool,
    },
    /// Auto-check timer of chain `gen` fired.
    UpdateTimer(u64),
    UpdateChecked {
        channel: globaltokentracker_core::update::Channel,
        manual: bool,
        res: Result<Option<globaltokentracker_core::update::Release>, String>,
    },
    /// "立即更新": download + verify the installer, then run it.
    StartUpdate,
    UpdateDownloaded(
        globaltokentracker_core::update::Release,
        Result<PathBuf, String>,
    ),
    /// Settings: update channel — "stable" | "alpha".
    SetUpdateChannel(&'static str),
    SetUpdateAuto(bool),
    DismissUpdateBanner,
    // ---- LAN devices
    DevicesLoaded(Result<devices::DevicesInfo, String>),
    /// Pull paired devices — `if_due`: core's 5-minute throttle.
    SyncPeers {
        if_due: bool,
    },
    PeersSynced(Result<Option<globaltokentracker_core::peer::sync::SyncReport>, String>),
    /// Peer-sync timer of chain `gen` fired.
    PeerTick(u64),
    Discover,
    Discovered(Result<Vec<globaltokentracker_core::peer::discovery::Found>, String>),
    PairAddr(String),
    PairCode(String),
    /// "Use this address" on a nearby device.
    PairUse(String),
    PairSubmit,
    Paired(Result<globaltokentracker_core::peer::pairing::Paired, String>),
    /// Forget… clicked (device id, name) — opens the confirmation.
    ForgetAsk(String, String),
    ForgetResult(ContentDialogResult),
    SetPeerEnabled(String, bool),
    /// A forget / include toggle finished.
    PeerOpDone(Result<(), String>),
    /// Device checkbox in the filter strip (device id, checked).
    ToggleDevice(String, bool),
    /// Bulk device scope — `None` = all devices, `Some(vec![])` = none.
    SetDevices(Option<Vec<String>>),
    /// Settings: share this PC on the LAN.
    SetShareLan(bool),
    /// While sharing: follow the code on offer (renewed after each pairing)
    /// and the devices that can see this PC.
    ShareTick(u64),
    ShareStarted(Result<Box<globaltokentracker_core::peer::share::Sharing>, String>),
    ShareStopped,
    ShareNewCode,
    ViewersLoaded(Result<Vec<globaltokentracker_core::peer::trust::Viewer>, String>),
    Revoke(String),
}

const DETAIL_PAGE_SIZE: i64 = 200;
/// Effect / element keys for the two page layers and their block hosts.
const LAYER_HOST_KEYS: [&str; 2] = ["host-l0", "host-l1"];
const LAYER_KEYS: [&str; 2] = ["layer0", "layer1"];
const BLOCK_HOST_KEYS: [[&str; MAX_SLIDE]; 2] = [
    [
        "host-0b0", "host-0b1", "host-0b2", "host-0b3", "host-0b4", "host-0b5", "host-0b6",
        "host-0b7", "host-0b8", "host-0b9",
    ],
    [
        "host-1b0", "host-1b1", "host-1b2", "host-1b3", "host-1b4", "host-1b5", "host-1b6",
        "host-1b7", "host-1b8", "host-1b9",
    ],
];
/// Month the custom-range calendar opens on: the one holding the range's last
/// day (today's for anything else).
fn cal_month_of(r: Range) -> (i16, i8) {
    let d = match r {
        Range::Custom { end_ms, .. } => globaltokentracker_core::viewmodel::local_date(end_ms - 1),
        _ => jiff::Zoned::now().date(),
    };
    (d.year(), d.month())
}

/// Spec §6.9: quota polling is low-frequency by design.
const QUOTA_POLL_SECS: u64 = 30 * 60;

/// `GTT_DEBUG=1` → diagnostic stderr (invisible for normal GUI launches).
pub(crate) fn diag_enabled() -> bool {
    std::env::var_os("GTT_DEBUG").is_some()
}

macro_rules! diag {
    ($($t:tt)*) => {
        if crate::diag_enabled() {
            eprintln!($($t)*);
        }
    };
}
pub(crate) use diag;

/// Give `eprintln!`/`diag!` somewhere to land in a GUI-subsystem build:
/// attach to the invoker's console when present, else allocate a fresh one
/// (GTT_DEBUG double-click debugging). No-op when the console handle is
/// already valid (console-subsystem dev build).
#[cfg(windows)]
fn diag_console() {
    use std::ptr;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole, GetStdHandle, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };
    #[allow(unsafe_code)]
    unsafe {
        if !GetStdHandle(STD_ERROR_HANDLE).is_null() {
            return; // already have a console (dev build / console launch)
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 && AllocConsole() == 0 {
            return; // no console anywhere and can't allocate — stay silent
        }
        let mut name: Vec<u16> = "CONOUT$".encode_utf16().chain(Some(0)).collect();
        let h = CreateFileW(
            name.as_mut_ptr(),
            0x8000_0000 | 0x4000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        );
        if !h.is_null() && h != -1isize as _ {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
            let _ = SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
}

fn db_path() -> PathBuf {
    default_db_path()
}

/// Window icon as a real file. `AppWindow.SetIcon` does NOT resolve bare
/// resource-ID strings for an unpackaged exe (verified: the window ended up
/// with the generic pane glyph), so the embedded ICO is materialized once
/// into the data dir — identical for dev runs and installed copies.
pub(crate) fn window_icon_path() -> &'static str {
    static P: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let dir = db_path()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let p = dir.join("icon.ico");
        if !p.exists() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&p, include_bytes!("../../../assets/icon.ico"));
        }
        p.to_string_lossy().into_owned()
    })
    .as_str()
}

/// What one background load needs to know.
struct LoadReq {
    range: Range,
    apps: Option<Vec<String>>,
    models: Option<Vec<String>>,
    /// LAN device scope (`None` = all devices).
    devices: Option<Vec<String>>,
    force_prices: bool,
    /// True when the caller knows visible data must be rebuilt even if the
    /// scan lands nothing (filter/range/page change, reprice, quota poll).
    force_views: bool,
    /// Aggregates from the previous load (`None` → build from scratch: first
    /// load, manual refresh, repricing).
    prev_cube: Option<Arc<Cube>>,
    filter_gen: u64,
    /// Run the file scan first. Off for the startup load: the ledger already
    /// holds everything from the last session, so show it now and scan after.
    scan: bool,
}

fn load_all(req: LoadReq) -> Result<LoadOutcome, String> {
    let LoadReq {
        range,
        apps,
        models,
        devices,
        force_prices,
        force_views,
        prev_cube,
        filter_gen,
        scan,
    } = req;
    let store = Store::open(&db_path()).map_err(|e| e.to_string())?;
    let (store, report, scan_ms) = if scan {
        let engine = Engine::new(store).map_err(|e| e.to_string())?;
        let t = std::time::Instant::now();
        let report = engine.scan_once();
        (engine.store, report, t.elapsed().as_millis())
    } else {
        (store, Ok(globaltokentracker_core::ScanReport::default()), 0)
    };
    // A failed scan can't prove "nothing changed" — rebuild views anyway.
    let changed = report
        .as_ref()
        .map(|r| r.events_ingested > 0 || r.quotas > 0)
        .unwrap_or(true);
    // Price refresh runs as its own background task (see PricesDone) so a
    // slow network never gates first paint or a refresh tick. `price_due`
    // fires once per launch (force_prices) and whenever >12h stale; the
    // app_state attempt stamp throttles failures to the same TTL.
    let price_due =
        force_prices || globaltokentracker_core::pricing::prices_stale(&store).unwrap_or(false);
    // A local midnight since the cube was built makes its "today" stale.
    let stale_day = prev_cube.as_ref().is_some_and(|c| c.is_day_stale());
    if !force_views && !changed && !stale_day {
        return Ok(LoadOutcome::Unchanged { scan_ms, price_due });
    }
    // Aggregates: only the local days this pass wrote are recomputed (plus
    // today/yesterday, for writers we don't hear about); anything the scan
    // can't vouch for → one full pass (~70ms on a 63k-event ledger).
    let t_cube = std::time::Instant::now();
    let cube = match (&prev_cube, &report) {
        (Some(c), Ok(r)) if !r.rollup_full => c.refreshed(&store, &r.rollup_days),
        _ => Cube::build(&store),
    }
    .map_err(|e| e.to_string())?;
    let cube = Arc::new(cube);
    diag!(
        "[cube] {} groups, {}ms ({})",
        cube.group_count(),
        t_cube.elapsed().as_millis(),
        if prev_cube.is_some() {
            "refresh"
        } else {
            "build"
        }
    );
    let vm = store
        .overview_from_cube_for(
            &cube,
            range,
            apps.as_deref(),
            models.as_deref(),
            devices.as_deref(),
        )
        .map_err(|e| e.to_string())?;
    // Rows only — the total comes from the cube (no COUNT(*) over the ledger).
    let rows = store
        .events_page(DETAIL_PAGE_SIZE, 0, apps.as_deref(), models.as_deref())
        .map_err(|e| e.to_string())?;
    let total = cube.event_count(apps.as_deref(), models.as_deref());
    let fx = globaltokentracker_core::fx::status(&store);
    Ok(LoadOutcome::Fresh(Box::new(Snapshot {
        vm,
        cube,
        filter_gen,
        detail: DetailBundle {
            rows: Arc::new(rows),
            total,
            page: 0,
        },
        price_due,
        scan_ms,
        scanned: scan,
        fx,
    })))
}

/// Light per-page table query — opens the ledger, reads one table, done.
/// Deliberately does NOT scan: the old path re-ran the whole filesystem scan
/// (~300ms) plus overview/detail aggregation just to fetch a page's table,
/// landing mid-slide and dragging the animation.
fn load_page(page: Page) -> Result<PageData, String> {
    let store = Store::open(&db_path()).map_err(|e| e.to_string())?;
    match page {
        Page::Sources => store
            .source_health()
            .map(PageData::Sources)
            .map_err(|e| e.to_string()),
        Page::Prices => {
            let rows = store.price_rows(5000).map_err(|e| e.to_string())?;
            Ok(PageData::Prices(PriceTable {
                rows: Arc::new(rows),
                synced_at: globaltokentracker_core::pricing::last_live_sync(&store).unwrap_or(None),
            }))
        }
        _ => Err("not a table page".into()),
    }
}

/// First line of an error, capped — fits the Prices page summary row.
fn short_error(e: &str) -> String {
    let line = e.lines().next().unwrap_or(e);
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        out.push('…');
    }
    out
}

/// Arms one periodic-refresh timer of generation `gen_id`. `secs == 0` (仅文件变更) skips arming —
/// the file watcher still live-refreshes on source changes.
fn arm_refresh(context: &ComponentContext<Shell>, gen_id: u64, secs: u64) {
    if secs == 0 {
        return;
    }
    context.spawn_background(move |_| {
        power::worker("gtt-timer");
        std::thread::sleep(std::time::Duration::from_secs(secs));
        Msg::Tick(gen_id)
    });
}

/// Union of adapter watch roots that exist right now (tool absent → dir absent).
fn source_roots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for a in adapters::registry() {
        for r in a.watch_roots() {
            if r.is_dir() && !out.contains(&r) {
                out.push(r);
            }
        }
    }
    out
}

/// Live refresh: block on notify events, debounce, then report once.
fn arm_watcher(context: &ComponentContext<Shell>, gen_id: u64) {
    let roots = source_roots();
    if roots.is_empty() {
        return;
    }
    context.spawn_background(move |token| {
        power::worker("gtt-watch");
        diag!(
            "[watch] armed (gen {}) on {} roots: {:?}",
            gen_id,
            roots.len(),
            roots
        );
        if watch::wait_for_change(&roots, &token) {
            Msg::WatchFired(gen_id)
        } else {
            // Watch failed/cancelled — fall back to a slow poll so changes are
            // still picked up eventually in file-watch mode.
            std::thread::sleep(std::time::Duration::from_secs(120));
            Msg::WatchFallback(gen_id)
        }
    });
}

/// Arms one update-check timer of chain `gen`.
fn arm_update(context: &ComponentContext<Shell>, gen_id: u64, secs: u64) {
    context.spawn_background(move |_| {
        power::worker("gtt-update");
        std::thread::sleep(std::time::Duration::from_secs(secs));
        Msg::UpdateTimer(gen_id)
    });
}

/// First peer sync after launch, then every `PEER_SYNC_SECS` the timer asks
/// core whether one is due (every 5 minutes, or at once after a device has
/// paired with this PC — so its usage shows up within seconds).
const PEER_FIRST_SYNC_SECS: u64 = 20;
const PEER_SYNC_SECS: u64 = 15;

/// Arms one peer-sync timer of chain `gen_id`.
fn arm_peer_sync(context: &ComponentContext<Shell>, gen_id: u64, secs: u64) {
    context.spawn_background(move |_| {
        power::worker("gtt-peer-timer");
        std::thread::sleep(std::time::Duration::from_secs(secs));
        Msg::PeerTick(gen_id)
    });
}

/// Share status follow-up while sharing (see `Msg::ShareTick`).
fn arm_share_tick(context: &ComponentContext<Shell>, tick: u64) {
    context.spawn_background(move |_| {
        power::worker("gtt-share-tick");
        std::thread::sleep(std::time::Duration::from_secs(2));
        Msg::ShareTick(tick)
    });
}

/// One blocking tray-event poll per arm; re-armed on every message.
fn arm_tray(context: &ComponentContext<Shell>) {
    context.spawn_background(|_| {
        power::worker("gtt-tray");
        Msg::Tray(tray::next_action())
    });
}

impl Component for Shell {
    type Input = ();
    type Message = Msg;

    fn create(_input: &(), context: &ComponentContext<Self>) -> Self {
        let mut config = UiConfig::load();
        i18n::set_lang(i18n::Lang::from_config(&config.lang));
        // The registry is authoritative — ui.json only mirrors the last write
        // (a fresh install or manual removal clears the flag honestly).
        config.autostart = autostart::enabled();
        let interface = globaltokentracker_core::external_interface::read(
            &globaltokentracker_core::external_interface::settings_path(),
        );
        let external_interface_enabled = interface.as_ref().is_ok_and(|settings| settings.enabled);
        let external_interface_error = interface
            .err()
            .map(|_| "接口设置读取失败，外部协同已关闭".to_string());
        let range = Range::from_config(&config.range, config.range_start_ms, config.range_end_ms);
        let app_filter = config.apps.clone();
        let model_filter = config.models.clone();
        let device_filter = config.devices.clone();
        let page = match std::env::var("GTT_PAGE").as_deref() {
            Ok("speed") => Page::Speed,
            Ok("detail") => Page::Detail,
            Ok("quota") => Page::Quota,
            Ok("devices") => Page::Devices,
            Ok("sources") => Page::Sources,
            Ok("scheduling") => Page::Scheduling,
            Ok("prices") => Page::Prices,
            Ok("settings") => Page::Settings,
            _ => Page::Overview,
        };
        if page == Page::Scheduling {
            context.spawn_background(|_| {
                Msg::Scheduling(scheduling::Action::Received(
                    globaltokentracker_core::scheduler::call(
                        "router_scheduling",
                        serde_json::json!({}),
                    )
                    .map_err(|e| e.to_string()),
                ))
            });
        }
        // force_prices=true: one refresh attempt on every launch (per spec:
        // 每次打开软件自动获取一次), off the UI thread. Subsequent scans only
        // refresh when >12h stale.
        context.spawn_background(move |_| {
            // The startup snapshot is what the user is waiting for: named but
            // NOT efficiency-marked, so it runs on the performance cores (the
            // cube build took ~170ms on an E-core vs ~70ms on a P-core).
            power::name_thread("gtt-scan");
            match load_all(LoadReq {
                range,
                apps: app_filter,
                models: model_filter,
                devices: device_filter,
                force_prices: true,
                force_views: true,
                prev_cube: None,
                filter_gen: 0,
                scan: false,
            }) {
                Ok(s) => Msg::Loaded(s),
                Err(e) => Msg::Failed(e),
            }
        });
        // The watcher is the refresh source only in 仅文件变更 mode; in
        // timer mode per-file writes would defeat the configured cadence.
        let refresh_gen = 0u64;
        let watch_gen = 0u64;
        if config.refresh_secs == 0 {
            arm_watcher(context, watch_gen);
        }
        let tray = tray::install();
        if tray.is_some() {
            arm_tray(context);
            // `--minimized` (autostart): hide once the window exists — the
            // background poll tolerates the WinUI window not being up yet.
            // No tray → stay visible; hidden without tray would be a zombie.
            if std::env::args().any(|a| a == "--minimized") {
                context.spawn_background(|_| {
                    power::worker("gtt-minhide");
                    for _ in 0..20 {
                        if tray::try_hide_main_window() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(400));
                    }
                    Msg::Noop
                });
            }
        }
        let theme = Theme::resolve(&config.theme, theme::is_light(&config.window_theme));
        // OTLP receiver: dedicated blocking thread (never the reactor pool).
        // Port busy or GTT_NO_OTEL → file-based sources only.
        let _otel = globaltokentracker_core::otel::spawn(db_path());
        if config.update_auto {
            arm_update(context, 0, updater::FIRST_CHECK_SECS);
        }
        // LAN devices: the registry for the Devices page / filter, a sync
        // once due, and sharing if the user left it on.
        context.spawn_background(|_| {
            power::worker("gtt-peers");
            Msg::DevicesLoaded(devices::load(&db_path()))
        });
        arm_peer_sync(context, 0, PEER_FIRST_SYNC_SECS);
        if config.share_lan {
            context.spawn_background(|_| {
                power::worker("gtt-share");
                Msg::ShareStarted(devices::start_sharing(&db_path()).map(Box::new))
            });
        }
        let share = devices::ShareState {
            busy: config.share_lan,
            ..Default::default()
        };
        Self {
            scheduler: scheduling::State::default(),
            snap: None,
            sources: None,
            prices: None,
            price_query: String::new(),
            price_disputed: false,
            price_shown: Arc::default(),
            page_loading: [false; 2],
            page_again: [false; 2],
            prefetched: false,
            // 1180 client - 2x24 page padding - card padding/border.
            table_w: 1180.0 - 48.0 - 2.0 * theme.pad - 2.0,
            table_rulers: [ElementRef::new(), ElementRef::new()],
            canvas_ready: usize::MAX,
            page,
            scanning: true,
            pending_rescan: false,
            last_error: None,
            refresh_gen,
            watch_gen,
            app_filter: config.apps.clone(),
            model_filter: config.models.clone(),
            device_filter: config.devices.clone(),
            config,
            external_interface_enabled,
            external_interface_error,
            range,
            theme,
            editing: std::env::var("GTT_EDIT").is_ok(),
            tray,
            trend: widgets::TrendHandle::default(),
            heat: widgets::HeatHandle::default(),
            cal_month: cal_month_of(range),
            cal_anchor: None,
            clock_anchor: None,
            clock_dial: widgets::ClockDialHandle::default(),
            donuts: std::array::from_fn(|_| widgets::DonutHandle::default()),
            quota_at: None,
            open_menu: None,
            quota_collapsed: std::collections::BTreeSet::new(),
            prices_refreshing: false,
            price_refresh_error: None,
            fx_refreshing: false,
            fx_refresh_error: None,
            speed_by_app: false,
            speed_metric: SpeedMetric::Output,
            speed_page: 0,
            views_stale: true,
            cube_rebuild: false,
            filter_gen: 0,
            overview_cols: 4,
            flight: None,
            flight_seq: 0,
            layer_hosts: std::array::from_fn(|_| LayerHost::default()),
            block_hosts: std::array::from_fn(|_| std::array::from_fn(|_| LayerHost::default())),
            rest_idx: 0,
            block_count: std::cell::Cell::new(0),
            close_prompt: false,
            close_remember: false,
            ruler: ElementRef::new(),
            update: UpdateState::Idle,
            update_gen: 0,
            update_banner_dismissed: false,
            update_checking: false,
            update_seen_tag: None,
            update_error: None,
            devices: None,
            nearby: Vec::new(),
            peer_syncing: false,
            peer_sync_shown: false,
            discovering: false,
            peer_error: None,
            pair: devices::PairForm::default(),
            forget_target: None,
            share,
            peer_gen: 0,
        }
    }

    fn update(&mut self, message: Msg, context: &ComponentContext<Self>) {
        match message {
            Msg::Scheduling(action) => self.scheduler.update(action, context),
            Msg::Loaded(outcome) => {
                let is_real_scan = match &outcome {
                    LoadOutcome::Fresh(s) => s.scanned,
                    LoadOutcome::Unchanged { .. } => true,
                };
                let mut fresh = false;
                let mut startup_snapshot = false;
                let price_due = match outcome {
                    LoadOutcome::Fresh(s) => {
                        diag!("[scan] loaded, pending_rescan={}", self.pending_rescan);
                        let price_due = s.price_due;
                        fresh = true;
                        diag!(
                            "[startup] snapshot applied at {}ms (scanned={})",
                            START.get().map_or(0, |t| t.elapsed().as_millis()),
                            s.scanned
                        );
                        if !s.scanned {
                            // Built without a scan: the snapshot is current as
                            // of the ledger, but a scan still has to follow.
                            startup_snapshot = true;
                            self.views_stale = false;
                        }
                        self.snap = Some(*s);
                        // The load ran against the range/filters captured when
                        // it started; the user may have moved on since —
                        // re-aim at the current ones (a cube fold).
                        let (apps, models) = (self.app_filter.clone(), self.model_filter.clone());
                        let devices = self.device_filter.clone();
                        let range = self.range;
                        let gen_now = self.filter_gen;
                        let mut detail_stale = false;
                        if let Some(sn) = self.snap.as_mut()
                            && (sn.vm.range != range || sn.filter_gen != gen_now)
                        {
                            sn.vm.apply_cube_for(
                                &sn.cube,
                                range,
                                apps.as_deref(),
                                models.as_deref(),
                                devices.as_deref(),
                            );
                            sn.detail.total =
                                sn.cube.event_count(apps.as_deref(), models.as_deref());
                            detail_stale = sn.filter_gen != gen_now;
                            sn.filter_gen = gen_now;
                        }
                        self.prune_filters();
                        if detail_stale {
                            self.load_detail_page(0, context);
                        }
                        // Test hook: GTT_NAVTEST=<nav label> fires one page
                        // switch after first paint — scripted input can't
                        // reach the content island for real verification.
                        if let Ok(spec) = std::env::var("GTT_NAVTEST") {
                            static NAVFIRED: std::sync::atomic::AtomicBool =
                                std::sync::atomic::AtomicBool::new(false);
                            if !NAVFIRED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                                // "label@ms" — alternates label↔总览 every
                                // ms, so any capture burst lands inside a
                                // flight window regardless of scan speed.
                                let mut parts = spec.split('@');
                                let label = parts.next().unwrap_or("配额").to_string();
                                let ms = parts
                                    .next()
                                    .and_then(|m| m.parse::<u64>().ok())
                                    .unwrap_or(1200);
                                let alt = parts.next().unwrap_or("总览").to_string();
                                for k in 0..8u64 {
                                    let to = if k % 2 == 0 { &label } else { &alt }.clone();
                                    context.spawn_background(move |_| {
                                        power::worker("gtt-navtest");
                                        std::thread::sleep(std::time::Duration::from_millis(
                                            ms * (k + 1),
                                        ));
                                        Msg::Nav(Some(to))
                                    });
                                }
                            }
                        }
                        if let Some(tray) = &self.tray {
                            let total = self
                                .snap
                                .as_ref()
                                .map(|s| fmt::tokens_total(&s.vm.today))
                                .unwrap_or(0);
                            let _ = tray.set_tooltip(Some(tf!(
                                "GlobalTokenTracker — 今日 {}",
                                fmt::tokens_exact(total)
                            )));
                        }
                        price_due
                    }
                    LoadOutcome::Unchanged { scan_ms, price_due } => {
                        if let Some(old) = &mut self.snap {
                            old.scan_ms = scan_ms;
                        }
                        price_due
                    }
                };
                self.last_error = None;
                self.scanning = false;
                // Tables load off their own light queries, never the scan:
                // prefetch both once (so the first visit is instant), then
                // keep the open one current when new data lands.
                if self.snap.is_some() && !self.prefetched {
                    self.prefetched = true;
                    self.load_page_data(Page::Sources, context);
                    self.load_page_data(Page::Prices, context);
                } else if fresh {
                    self.load_page_data(self.page, context);
                }
                self.poll_quota_if_stale(context);
                // Detached price-source fetch: triggered here (post-load) so
                // network latency never delays the snapshot we just painted.
                if price_due {
                    self.spawn_price_refresh(context);
                }
                if startup_snapshot {
                    // The first frame is out; now catch up with whatever the
                    // tools wrote since (its Loaded arms the refresh timer).
                    self.start_scan(context);
                } else if self.pending_rescan {
                    self.pending_rescan = false;
                    self.start_scan(context);
                } else if self.config.refresh_secs > 0 {
                    // Only re-arm timer if this load was an actual disk scan!
                    // (Fast reload_views has scanned=false; leaving views_stale or filters changed
                    // must never multiply or restart the 30s cadence).
                    if is_real_scan {
                        self.refresh_gen += 1;
                        arm_refresh(context, self.refresh_gen, self.config.refresh_secs);
                    }
                }
            }
            Msg::Failed(e) => {
                self.last_error = Some(e);
                self.scanning = false;
                // The failed load may have owed the UI a forced rebuild.
                self.views_stale = true;
                if self.pending_rescan {
                    self.pending_rescan = false;
                    self.start_scan(context);
                } else if self.config.refresh_secs > 0 {
                    self.refresh_gen += 1;
                    arm_refresh(context, self.refresh_gen, self.config.refresh_secs);
                }
            }
            Msg::Tick(gen_id) => {
                if gen_id != self.refresh_gen || self.config.refresh_secs == 0 {
                    diag!(
                        "[timer] dropped tick: gen {} (current {}), refresh_secs={}",
                        gen_id,
                        self.refresh_gen,
                        self.config.refresh_secs
                    );
                } else if self.scanning {
                    self.pending_rescan = true;
                } else {
                    self.start_scan(context);
                }
            }
            Msg::Rescan => {
                // Manual refresh dismisses an open picker — the user moved on.
                self.open_menu = None;
                self.views_stale = true;
                // A manual refresh is also the "trust nothing" button: rebuild
                // the aggregates from the raw events.
                self.cube_rebuild = true;
                globaltokentracker_core::adapters::forget_scan_memos();
                self.sync_peers(false, context);
                // Invalidate in-flight timer so manual refresh resets the countdown cleanly
                if self.config.refresh_secs > 0 {
                    self.refresh_gen += 1;
                }
                self.start_scan(context);
            }
            Msg::SetRange(r) => {
                self.open_menu = None;
                self.cal_anchor = None;
                self.clock_anchor = None;
                self.set_range(r, context);
            }
            Msg::PickCustomRange => {
                self.open_menu = None;
                let (s, e) = match self.range {
                    Range::Custom { start_ms, end_ms } => (start_ms, end_ms),
                    _ => self
                        .config
                        .range_start_ms
                        .zip(self.config.range_end_ms)
                        .unwrap_or_else(|| (day_start_ms(6), day_start_ms(-1))),
                };
                let r = Range::custom(s, e);
                self.cal_month = cal_month_of(r);
                self.cal_anchor = None;
                self.clock_anchor = None;
                self.set_range(r, context);
            }
            Msg::CalPick(y, m, d) => {
                let Ok(day) = jiff::civil::Date::new(y, m, d) else {
                    return;
                };
                if day > jiff::Zoned::now().date() {
                    return;
                }
                self.clock_anchor = None;
                let r = match self.cal_anchor.take() {
                    None => {
                        self.cal_anchor = Some(day);
                        Range::custom_days(day, day)
                    }
                    Some(first) => Range::custom_days(first, day),
                };
                self.set_range(r, context);
            }
            Msg::ClockPick(h) => {
                let Some(day) = self.single_day() else {
                    return;
                };
                self.clock_dial.pick_optimistic(h, 23);
                let r = match self.clock_anchor.take() {
                    None => {
                        self.clock_anchor = Some(h);
                        Range::custom_hours(day, h, h)
                    }
                    Some(first) => Range::custom_hours(day, first, h),
                };
                self.set_range(r, context);
            }
            Msg::ClockFullDay => {
                if let Some(day) = self.single_day() {
                    self.clock_anchor = None;
                    self.clock_dial.reset_full_day();
                    self.set_range(Range::custom_days(day, day), context);
                }
            }
            Msg::CalMonth(delta) => {
                let (y, m) = self.cal_month;
                let now = jiff::Zoned::now().date();
                if let Ok(first) = jiff::civil::Date::new(y, m, 1)
                    && let Ok(next) = first.checked_add(jiff::Span::new().months(i32::from(delta)))
                    && (next.year(), next.month()) <= (now.year(), now.month())
                {
                    self.cal_month = (next.year(), next.month());
                }
            }
            Msg::CalQuick(key) => {
                let today = jiff::Zoned::now().date();
                let days = |n: i64| today.saturating_sub(jiff::Span::new().days(n));
                let (first, last) = match key {
                    "昨天" => (days(1), days(1)),
                    // Monday-first, matching the calendar grid.
                    "本周" => (
                        days(i64::from(today.weekday().to_monday_zero_offset())),
                        today,
                    ),
                    "本月" => (today.first_of_month(), today),
                    "上月" => {
                        let f = today
                            .first_of_month()
                            .saturating_sub(jiff::Span::new().days(1));
                        (f.first_of_month(), f)
                    }
                    _ => (days(89), today),
                };
                let r = Range::custom_days(first, last);
                self.cal_month = cal_month_of(r);
                self.cal_anchor = None;
                self.clock_anchor = None;
                self.set_range(r, context);
            }
            Msg::ToggleApp(app, on) => {
                let all: Vec<String> = self
                    .snap
                    .as_ref()
                    .map(|s| s.vm.apps.clone())
                    .unwrap_or_default();
                // Checked set: explicit filter, else every known tool.
                let mut set: std::collections::BTreeSet<String> = self
                    .app_filter
                    .clone()
                    .unwrap_or_else(|| all.clone())
                    .into_iter()
                    .collect();
                if on {
                    set.insert(app);
                } else {
                    set.remove(&app);
                }
                // Full coverage collapses back to None (no filter) so the
                // persisted config stays clean.
                self.app_filter = if all.iter().all(|a| set.contains(a)) {
                    None
                } else {
                    Some(set.into_iter().collect())
                };
                self.config.apps = self.app_filter.clone();
                self.config.save();
                self.filter_gen += 1;
                self.refresh_views(context, true);
            }
            Msg::SetApps(filter) => {
                self.app_filter = filter;
                self.config.apps = self.app_filter.clone();
                self.config.save();
                self.filter_gen += 1;
                self.refresh_views(context, true);
            }
            Msg::ToggleModel(model, on) => {
                let all: Vec<String> = self
                    .snap
                    .as_ref()
                    .map(|s| s.vm.models.clone())
                    .unwrap_or_default();
                // Same collapse rule as tools: full coverage → None.
                let mut set: std::collections::BTreeSet<String> = self
                    .model_filter
                    .clone()
                    .unwrap_or_else(|| all.clone())
                    .into_iter()
                    .collect();
                if on {
                    set.insert(model);
                } else {
                    set.remove(&model);
                }
                self.model_filter = if all.iter().all(|m| set.contains(m)) {
                    None
                } else {
                    Some(set.into_iter().collect())
                };
                self.config.models = self.model_filter.clone();
                self.config.save();
                self.filter_gen += 1;
                self.refresh_views(context, true);
            }
            Msg::SetModels(filter) => {
                self.model_filter = filter;
                self.config.models = self.model_filter.clone();
                self.config.save();
                self.filter_gen += 1;
                self.refresh_views(context, true);
            }
            Msg::SetRefreshSecs(secs) => {
                if REFRESH_OPTIONS.iter().any(|(s, _)| *s == secs)
                    && secs != self.config.refresh_secs
                {
                    self.config.refresh_secs = secs;
                    self.config.save();
                    self.refresh_gen += 1;
                    self.watch_gen += 1;
                    if secs == 0 {
                        // Entering 仅文件变更: the watcher (which lapses in
                        // timer mode) becomes the refresh source again.
                        arm_watcher(context, self.watch_gen);
                    } else if !self.scanning {
                        // In timer mode: kick one timer now so the new cadence
                        // starts without waiting for the next scan to end.
                        arm_refresh(context, self.refresh_gen, secs);
                    }
                }
                // Single-select semantics: a pick light-dismisses the panel.
                self.open_menu = None;
            }
            Msg::CloseMenu => self.open_menu = None,
            Msg::ToggleMenu(kind) => {
                self.open_menu = if self.open_menu == Some(kind) {
                    None
                } else {
                    Some(kind)
                };
            }
            Msg::ToggleQuotaGroup(app) => {
                if !self.quota_collapsed.remove(&app) {
                    self.quota_collapsed.insert(app);
                }
            }
            Msg::SetThemeMode(v) => {
                self.config.window_theme = v.to_string();
                self.config.save();
                // `window_visuals` is re-published every view; the chart
                // colors (light/dark sets) are re-resolved here.
                self.theme = self.resolve_theme();
            }
            Msg::SetAccent(v) => {
                self.config.theme.accent = if v.is_empty() {
                    None
                } else {
                    Some(v.to_string())
                };
                self.theme = self.resolve_theme();
                self.config.save();
            }
            Msg::SetFontFamily(v) => {
                self.config.theme.font_family = if v.is_empty() { None } else { Some(v) };
                self.theme = self.resolve_theme();
                self.config.save();
            }
            Msg::SetFontSize(body) => {
                // Slider range is 9–18; title/h2/label keep their offsets.
                let body = body.clamp(9.0, 18.0);
                let t = &mut self.config.theme;
                t.body_size = Some(body);
                t.title_size = Some(body + 10.0);
                t.h2_size = Some(body + 2.0);
                t.label_size = Some(body - 1.0);
                self.theme = self.resolve_theme();
                self.config.save();
            }
            Msg::SetLang(v) => {
                self.config.lang = v.to_string();
                self.config.save();
                i18n::set_lang(i18n::Lang::from_config(v));
            }
            Msg::SpeedGroup(by_app) => {
                self.speed_by_app = by_app;
                self.speed_page = 0;
            }
            Msg::SpeedMetric(metric) => {
                self.speed_metric = metric;
                self.speed_page = 0;
            }
            Msg::SpeedPage(page) => self.speed_page = page,
            Msg::RefreshFx => {
                if !self.fx_refreshing {
                    self.fx_refreshing = true;
                    self.fx_refresh_error = None;
                    context.spawn_background(|_| {
                        power::worker("gtt-fx-manual");
                        Msg::FxDone(
                            Store::open(&db_path())
                                .and_then(|s| globaltokentracker_core::fx::refresh(&s))
                                .map_err(|e| e.to_string()),
                        )
                    });
                }
            }
            Msg::FxDone(result) => {
                self.fx_refreshing = false;
                match result {
                    Ok(st) => {
                        globaltokentracker_core::viewmodel::fmt::set_display(
                            &st.display,
                            st.usd_to,
                        );
                        if let Some(s) = &mut self.snap {
                            s.fx = st;
                        }
                    }
                    Err(e) => self.fx_refresh_error = Some(short_error(&e)),
                }
            }
            Msg::SetDisplayCurrency(code) => match Store::open(&db_path()) {
                Ok(store) => {
                    if globaltokentracker_core::fx::set_display_currency(&store, &code).is_ok() {
                        let st = globaltokentracker_core::fx::status(&store);
                        globaltokentracker_core::viewmodel::fmt::set_display(
                            &st.display,
                            st.usd_to,
                        );
                        if let Some(s) = &mut self.snap {
                            s.fx = st;
                        }
                    }
                }
                Err(e) => diag!("[fx] ledger open failed: {e}"),
            },
            Msg::SetAutostart(on) => {
                // Registry write may fail (policy/AV) — mirror the real
                // outcome so the toggle reflects the truth, not the intent.
                let got = autostart::set(on);
                diag!("[settings] autostart want={on} got={got}");
                self.config.autostart = got;
                self.config.save();
            }
            Msg::SetExternalInterface(on) => {
                let path = globaltokentracker_core::external_interface::settings_path();
                match globaltokentracker_core::external_interface::write(&path, on) {
                    Ok(settings) => {
                        self.external_interface_enabled = settings.enabled;
                        self.external_interface_error = None;
                    }
                    Err(_) => {
                        self.external_interface_enabled =
                            globaltokentracker_core::external_interface::read(&path)
                                .is_ok_and(|settings| settings.enabled);
                        self.external_interface_error =
                            Some("接口设置保存失败，已显示实际状态".to_string());
                    }
                }
            }
            Msg::CloseRequested => match self.config.close_action.as_str() {
                "quit" => self.quit_now(context),
                // "tray" remembered but the icon failed to install → hiding
                // would strand the process with no way back; ask instead.
                "tray" if self.tray.is_some() => tray::hide_main_window(),
                _ => self.close_prompt = true,
            },
            Msg::CloseDialogResult(res) => {
                self.close_prompt = false;
                // Consume the checkbox — a dismissed dialog must not leave
                // "remember" armed for the next open.
                let remember = self.close_remember;
                self.close_remember = false;
                match res {
                    ContentDialogResult::Primary => {
                        if remember {
                            self.config.close_action = "quit".into();
                            self.config.save();
                        }
                        self.quit_now(context);
                    }
                    ContentDialogResult::Secondary => {
                        if remember {
                            self.config.close_action = "tray".into();
                            self.config.save();
                        }
                        if self.tray.is_some() {
                            tray::hide_main_window();
                        } else {
                            self.quit_now(context);
                        }
                    }
                    _ => {}
                }
            }
            Msg::CloseRemember(on) => self.close_remember = on,
            Msg::SetCloseAction(v) => {
                self.config.close_action = v.to_string();
                self.config.save();
            }
            Msg::Noop => {}
            Msg::CheckUpdate { manual } => {
                if matches!(self.update, UpdateState::Downloading(_)) {
                    return;
                }
                if manual {
                    self.update = UpdateState::Checking;
                }
                if self.update_checking {
                    return;
                }
                self.update_checking = true;
                let channel = Channel::from_key(&self.config.update_channel);
                context.spawn_background(move |_| {
                    power::worker("gtt-update");
                    Msg::UpdateChecked {
                        manual,
                        channel,
                        res: globaltokentracker_core::update::check(channel)
                            .map_err(|e| format!("{e:#}")),
                    }
                });
            }
            Msg::UpdateTimer(g) => {
                if g != self.update_gen || !self.config.update_auto {
                    return;
                }
                arm_update(context, g, updater::CHECK_INTERVAL_SECS);
                self.update(Msg::CheckUpdate { manual: false }, context);
            }
            Msg::UpdateChecked {
                manual,
                channel,
                res,
            } => {
                self.update_checking = false;
                // A check that was in flight when the user hit 立即更新 must
                // not clobber the Downloading state.
                if matches!(self.update, UpdateState::Downloading(_)) {
                    return;
                }
                if channel != Channel::from_key(&self.config.update_channel) {
                    // Answer for a channel the user has since left.
                    let pending = matches!(self.update, UpdateState::Checking);
                    if pending || self.config.update_auto {
                        self.update(Msg::CheckUpdate { manual: pending }, context);
                    }
                    return;
                }
                // A manual request that arrived while an auto check was in
                // flight left `Checking` behind — treat its result as manual.
                let manual = manual || matches!(self.update, UpdateState::Checking);
                self.update_error = None;
                match res {
                    Ok(Some(rel)) => {
                        diag!("[update] available {}", rel.tag);
                        if self.update_seen_tag.as_deref() != Some(rel.tag.as_str()) {
                            self.update_banner_dismissed = false;
                            self.update_seen_tag = Some(rel.tag.clone());
                        }
                        self.update = UpdateState::Available(rel);
                    }
                    Ok(None) => {
                        diag!("[update] up to date");
                        self.update = UpdateState::UpToDate;
                    }
                    Err(e) => {
                        diag!("[update] check failed: {e}");
                        if manual {
                            self.update = UpdateState::Failed(e);
                        }
                    }
                }
            }
            Msg::StartUpdate => {
                if let UpdateState::Available(rel) = &self.update {
                    let rel = rel.clone();
                    self.update_error = None;
                    self.update = UpdateState::Downloading(rel.clone());
                    context.spawn_background(move |_| {
                        power::worker("gtt-update");
                        let res = globaltokentracker_core::update::download(&rel)
                            .map_err(|e| format!("{e:#}"));
                        Msg::UpdateDownloaded(rel, res)
                    });
                }
            }
            Msg::UpdateDownloaded(rel, res) => {
                let err = match res {
                    Ok(path) => match updater::spawn_installer(&path) {
                        Ok(()) => {
                            self.quit_now(context);
                            return;
                        }
                        Err(e) => e.to_string(),
                    },
                    Err(e) => e,
                };
                diag!("[update] install failed: {err}");
                self.update = UpdateState::Available(rel);
                self.update_error = Some(err);
            }
            Msg::SetUpdateChannel(v) => {
                self.config.update_channel = v.to_string();
                self.config.save();
                self.update(Msg::CheckUpdate { manual: true }, context);
            }
            Msg::SetUpdateAuto(on) => {
                self.config.update_auto = on;
                self.config.save();
                self.update_gen += 1;
                if on {
                    arm_update(context, self.update_gen, updater::FIRST_CHECK_SECS);
                }
            }
            Msg::DismissUpdateBanner => self.update_banner_dismissed = true,
            // ---- LAN devices
            Msg::DevicesLoaded(res) => match res {
                Ok(info) => {
                    // A forgotten device leaves the filter (an empty
                    // remainder means "all" again).
                    if let Some(f) = &mut self.device_filter {
                        let before = f.len();
                        f.retain(|id| {
                            *id == info.local_id || info.peers.iter().any(|p| p.device_id == *id)
                        });
                        if f.len() != before {
                            if f.is_empty() {
                                self.device_filter = None;
                            }
                            self.config.devices = self.device_filter.clone();
                            self.config.save();
                        }
                    }
                    self.devices = Some(info);
                }
                Err(e) => self.peer_error = Some(e),
            },
            Msg::SyncPeers { if_due } => self.sync_peers(if_due, context),
            Msg::PeersSynced(res) => {
                self.peer_syncing = false;
                match res {
                    Ok(report) => {
                        self.peer_error = None;
                        if report.is_some_and(|r| r.changed) {
                            // Peer rows changed: the cube must be rebuilt.
                            self.cube_rebuild = true;
                            self.views_stale = true;
                            self.reload_views(context);
                        }
                    }
                    Err(e) => self.peer_error = Some(e),
                }
                self.load_devices(context);
            }
            Msg::PeerTick(gen_id) => {
                if gen_id == self.peer_gen {
                    self.sync_peers(true, context);
                    arm_peer_sync(context, gen_id, PEER_SYNC_SECS);
                }
            }
            Msg::Discover => {
                if !self.discovering {
                    self.discovering = true;
                    context.spawn_background(|_| {
                        power::worker("gtt-discover");
                        Msg::Discovered(devices::discover(&db_path()))
                    });
                }
            }
            Msg::Discovered(res) => {
                self.discovering = false;
                match res {
                    Ok(found) => {
                        self.nearby = found;
                        self.peer_error = None;
                    }
                    Err(e) => self.peer_error = Some(e),
                }
            }
            Msg::PairAddr(a) => self.pair.addr = a,
            Msg::PairCode(c) => self.pair.code = c,
            Msg::PairUse(a) => {
                self.pair.addr = a;
                self.pair.error = None;
            }
            Msg::PairSubmit => {
                if self.pair.ready() {
                    self.pair.busy = true;
                    self.pair.error = None;
                    self.pair.paired = None;
                    let (addr, code) = (self.pair.addr.clone(), self.pair.digits());
                    context.spawn_background(move |_| {
                        power::worker("gtt-pair");
                        Msg::Paired(devices::pair(&db_path(), &addr, &code))
                    });
                }
            }
            Msg::Paired(res) => {
                self.pair.busy = false;
                self.pair.code.clear();
                match res {
                    Ok(paired) => {
                        let device = paired.device;
                        self.pair.paired = Some(device.name);
                        self.pair.addr.clear();
                        self.nearby.retain(|n| n.device_id != device.device_id);
                        self.load_devices(context);
                        // Pull the new device at once.
                        self.sync_peers(false, context);
                        // Mutual: the other device reads this PC from now on,
                        // which needs this PC to be sharing.
                        if paired.mutual && !self.config.share_lan {
                            self.set_share_lan(true, context);
                        }
                    }
                    Err(code) => self.pair.error = Some(code),
                }
            }
            Msg::ForgetAsk(id, name) => self.forget_target = Some((id, name)),
            Msg::ForgetResult(result) => {
                if let Some((id, _)) = self.forget_target.take()
                    && result == ContentDialogResult::Primary
                {
                    context.spawn_background(move |_| {
                        power::worker("gtt-forget");
                        Msg::PeerOpDone(devices::forget(&db_path(), &id))
                    });
                }
            }
            Msg::SetPeerEnabled(id, on) => {
                context.spawn_background(move |_| {
                    power::worker("gtt-peer-enable");
                    Msg::PeerOpDone(devices::set_enabled(&db_path(), &id, on))
                });
            }
            Msg::PeerOpDone(res) => {
                if let Err(e) = res {
                    self.peer_error = Some(e);
                }
                self.load_devices(context);
                self.cube_rebuild = true;
                self.views_stale = true;
                self.reload_views(context);
            }
            Msg::ToggleDevice(id, on) => {
                let all: Vec<String> = self
                    .snap
                    .as_ref()
                    .map(|s| s.vm.devices.iter().map(|d| d.device_id.clone()).collect())
                    .unwrap_or_default();
                let mut set: std::collections::BTreeSet<String> = self
                    .device_filter
                    .clone()
                    .unwrap_or_else(|| all.clone())
                    .into_iter()
                    .collect();
                if on {
                    set.insert(id);
                } else {
                    set.remove(&id);
                }
                let next = if all.iter().all(|d| set.contains(d)) {
                    None
                } else {
                    Some(set.into_iter().collect())
                };
                self.set_device_filter(next, context);
            }
            Msg::SetDevices(f) => self.set_device_filter(f, context),
            Msg::SetShareLan(on) => self.set_share_lan(on, context),
            Msg::ShareStarted(res) => {
                self.share.busy = false;
                match res {
                    // Switched off while starting: stop it again.
                    Ok(running) if !self.config.share_lan => {
                        context.spawn_background(move |_| {
                            running.stop();
                            Msg::ShareStopped
                        });
                    }
                    Ok(running) => {
                        self.share.running = Some(*running);
                        self.share.refresh_status();
                        self.load_viewers(context);
                        self.share.tick += 1;
                        arm_share_tick(context, self.share.tick);
                    }
                    Err(e) => self.share.error = Some(e),
                }
            }
            Msg::ShareTick(tick) => {
                if tick == self.share.tick && self.share.running.is_some() {
                    let before = self.share.status.as_ref().and_then(|s| s.code.clone());
                    self.share.refresh_status();
                    let after = self.share.status.as_ref().and_then(|s| s.code.clone());
                    // A new code means a device just paired with this PC.
                    if before != after {
                        self.load_viewers(context);
                        self.load_devices(context);
                    }
                    arm_share_tick(context, tick);
                }
            }
            Msg::ShareStopped => {
                self.share.busy = false;
                self.share.refresh_status();
            }
            Msg::ShareNewCode => {
                if let Some(running) = &self.share.running
                    && let Err(e) = running.new_code()
                {
                    self.share.error = Some(e.to_string());
                }
                self.share.refresh_status();
                // A viewer may have just paired with the previous code.
                self.load_viewers(context);
            }
            Msg::ViewersLoaded(res) => match res {
                Ok(v) => self.share.viewers = v,
                Err(e) => self.share.error = Some(e),
            },
            Msg::Revoke(id) => {
                context.spawn_background(move |_| {
                    power::worker("gtt-revoke");
                    let db = db_path();
                    match devices::revoke(&db, &id) {
                        Ok(()) => Msg::ViewersLoaded(devices::viewers(&db)),
                        Err(e) => Msg::ViewersLoaded(Err(devices::message(&e))),
                    }
                });
            }
            Msg::NavGo(id) => self.nav_go(id, context),
            Msg::NavSettled(id) => {
                if self.flight.as_ref().is_some_and(|f| f.id == id)
                    && let Some(f) = self.flight.take()
                {
                    diag!(
                        "[nav] flight settled after {}ms",
                        f.t0.elapsed().as_millis()
                    );
                    self.commit_flight(false);
                }
            }
            // Width-ruler metrics → reflow column count changed. The
            // observer already dedupes, so landing here always rebuilds.
            Msg::SetOverviewCols(n) => self.overview_cols = n.clamp(1, pages::WIDTH_TIERS),
            Msg::SetTableWidth(w) => self.table_w = w,
            Msg::PriceQuery(q) => {
                self.price_query = q;
                self.refresh_price_view();
                diag!(
                    "[prices] query {:?} → {} rows",
                    self.price_query,
                    self.price_shown.len()
                );
            }
            Msg::PriceDisputed(on) => {
                self.price_disputed = on;
                self.refresh_price_view();
            }
            Msg::CanvasStage => {
                // Not before the animation has started: the incoming page is
                // still hidden and its visuals unresolved.
                if self
                    .flight
                    .as_ref()
                    .is_none_or(|f| f.phase == Phase::Running)
                {
                    self.canvas_ready = self.canvas_ready.saturating_add(1);
                    self.arm_canvas_stage(context, 16);
                }
            }
            Msg::WatchFired(gen_id) => {
                diag!("[watch] fired (gen {}), scanning={}", gen_id, self.scanning);
                // Only 仅文件变更 mode scans on file events — in timer mode
                // the next Tick picks up everything, and any in-flight
                // watcher lapses (not re-armed).
                if self.config.refresh_secs == 0 && gen_id == self.watch_gen {
                    self.watch_gen += 1;
                    arm_watcher(context, self.watch_gen);
                    if self.scanning {
                        self.pending_rescan = true;
                    } else {
                        self.start_scan(context);
                    }
                }
            }
            Msg::WatchFallback(gen_id) => {
                diag!(
                    "[watch] fallback fired (gen {}), scanning={}",
                    gen_id,
                    self.scanning
                );
                if self.config.refresh_secs == 0 && gen_id == self.watch_gen {
                    self.watch_gen += 1;
                    arm_watcher(context, self.watch_gen);
                    if !self.scanning {
                        self.start_scan(context);
                    }
                }
            }
            Msg::Tray(action) => {
                match action {
                    tray::TrayAction::Focus => {
                        tray::focus_main_window();
                    }
                    tray::TrayAction::Hide => {
                        tray::hide_main_window();
                    }
                    tray::TrayAction::Quit => {
                        self.quit_now(context);
                    }
                    tray::TrayAction::None => {}
                }
                if self.tray.is_some() {
                    arm_tray(context);
                }
            }
            Msg::Nav(tag) => {
                self.open_menu = None;
                let prev = self.page;
                self.page = match tag.as_deref() {
                    Some("速度" | "Speed" | "speed") => Page::Speed,
                    Some("明细" | "Details" | "detail") => Page::Detail,
                    Some("配额" | "Quota" | "quota") => Page::Quota,
                    Some("设备" | "Devices" | "devices") => Page::Devices,
                    Some("数据源" | "Sources" | "sources") => Page::Sources,
                    Some("调度" | "Scheduling" | "scheduling") => Page::Scheduling,
                    Some("价格" | "Prices" | "prices") => Page::Prices,
                    Some("设置" | "Settings" | "settings") => Page::Settings,
                    Some("总览" | "Overview" | "overview") => Page::Overview,
                    // None or an unrecognized label → keep the current page;
                    // a cleared selector must not teleport the user.
                    _ => prev,
                };
                if self.page != prev {
                    diag!("[nav] {prev:?} → {:?}", self.page);
                    self.start_flight(prev, context);
                }
                // Sources/Prices tables: the prefetched copy renders at once
                // (stale-while-revalidate); this light query only tops it up.
                // No scan — that used to land mid-slide and stall the flight.
                if self.page != prev {
                    self.load_page_data(self.page, context);
                    match self.page {
                        Page::Devices => self.load_devices(context),
                        Page::Settings => self.load_viewers(context),
                        Page::Scheduling => {
                            self.scheduler.update(scheduling::Action::Load, context);
                        }
                        _ => {}
                    }
                }
            }
            Msg::DetailPage(page) => {
                self.open_menu = None;
                self.load_detail_page(page, context);
            }
            Msg::DetailLoaded(rows, total, page) => {
                if let Some(s) = &mut self.snap {
                    s.detail = DetailBundle { rows, total, page };
                }
            }
            Msg::PageData(page, res) => {
                if let Some(slot) = lazy_slot(page) {
                    self.page_loading[slot] = false;
                    // Requested again while this query ran → one more pass.
                    if std::mem::take(&mut self.page_again[slot]) {
                        self.load_page_data(page, context);
                    }
                }
                // A reload that found the same data changes nothing: replacing
                // it re-rendered every realized row while the page slid in.
                match res {
                    Ok(PageData::Sources(rows)) => {
                        if self.sources.as_ref() != Some(&rows) {
                            self.sources = Some(rows);
                        }
                    }
                    Ok(PageData::Prices(t)) => {
                        let same = self.prices.as_ref().is_some_and(|old| {
                            old.synced_at == t.synced_at && *old.rows == *t.rows
                        });
                        if !same {
                            self.prices = Some(t);
                            self.refresh_price_view();
                        }
                    }
                    Err(e) => {
                        diag!("[page] {page:?} load failed: {e}");
                    }
                }
            }
            Msg::ToggleEdit => {
                self.open_menu = None;
                self.editing = !self.editing;
            }
            Msg::MoveWidget(page, id, delta) => {
                self.config
                    .move_widget(&page, &id, &widgets::registry_ids(), delta);
            }
            Msg::HideWidget(page, id, hidden) => {
                self.config.set_hidden(&page, &id, hidden);
            }
            Msg::TrendHover(x) => {
                diag!("[trend] hover x={x}");
                let sh = &self.trend.shared;
                let (w, n) = (sh.width.get(), sh.count.get());
                // Index under the pointer; unchanged → no repaint churn.
                let idx = if w > 0.0 && n > 0 {
                    Some(((x as f32 / (w / n as f32)) as usize).min(n - 1))
                } else {
                    None
                };
                if idx != sh.hover.get() {
                    sh.hover.set(idx);
                    sh.tip.set(None);
                    sh.pending.set(idx);
                    // Dwell arm: tooltip shows only if the pointer is still on
                    // the same bar when the timer lands (~450ms, Fluent-ish).
                    if let Some(i) = idx {
                        context.spawn_background(move |_| {
                            std::thread::sleep(std::time::Duration::from_millis(450));
                            Msg::TrendTip(i)
                        });
                    }
                    self.trend.inv.invalidate();
                }
            }
            Msg::TrendTip(i) => {
                let sh = &self.trend.shared;
                if sh.pending.get() == Some(i) && sh.hover.get() == Some(i) {
                    sh.tip.set(Some(i));
                    self.trend.inv.invalidate();
                }
            }
            Msg::TrendLeave => {
                let sh = &self.trend.shared;
                sh.pending.set(None);
                sh.tip.set(None);
                if sh.hover.take().is_some() {
                    self.trend.inv.invalidate();
                }
            }
            Msg::HeatHover(x, y) => {
                let sh = &self.heat.shared;
                let idx = sh.cell_at(x as f32, y as f32);
                if idx != sh.hover.get() {
                    sh.hover.set(idx);
                    sh.tip.set(None);
                    sh.pending.set(idx);
                    if let Some(i) = idx {
                        context.spawn_background(move |_| {
                            std::thread::sleep(std::time::Duration::from_millis(450));
                            Msg::HeatTip(i)
                        });
                    }
                    self.heat.inv.invalidate();
                }
            }
            Msg::HeatTip(i) => {
                let sh = &self.heat.shared;
                if sh.pending.get() == Some(i) && sh.hover.get() == Some(i) {
                    sh.tip.set(Some(i));
                    self.heat.inv.invalidate();
                }
            }
            Msg::HeatLeave => {
                let sh = &self.heat.shared;
                sh.pending.set(None);
                sh.tip.set(None);
                if sh.hover.take().is_some() {
                    self.heat.inv.invalidate();
                }
            }
            Msg::SetTrendLine(on) => {
                self.config.trend_line = on;
                self.config.save();
                self.trend.inv.invalidate();
            }
            Msg::RefreshPrices => self.spawn_price_refresh(context),
            Msg::SetHeatMetric(key) => {
                self.config.heat_metric = key.to_string();
                self.config.save();
                self.heat.inv.invalidate();
            }
            Msg::DonutHover(k, idx) => {
                if let Some(h) = self.donuts.get_mut(k as usize) {
                    // Unchanged → no repaint churn during pointer jitter.
                    if h.shared.hover.get() != idx {
                        h.shared.hover.set(idx);
                        h.inv.invalidate();
                    }
                }
            }
            Msg::QuotaDone(n, errs) => {
                diag!("[quota] {} rows, {} errors", n, errs.len());
                if n > 0 {
                    // New quota rows land outside the scan pipeline — force
                    // the next refresh to rebuild views for them.
                    self.views_stale = true;
                }
                for e in errs {
                    diag!("[quota] {e}");
                }
            }
            Msg::PricesDone(res) => {
                self.prices_refreshing = false;
                match res {
                    Ok(r) => {
                        if r.sources.is_empty() && !r.failed.is_empty() {
                            self.price_refresh_error = Some(short_error(&r.failed[0]));
                        }
                        diag!(
                            "[prices] synced: {} repriced={} rules_repass={}",
                            r.summary(),
                            r.repriced,
                            r.rules_repass
                        );
                        for f in &r.failed {
                            diag!("[prices] feed failed: {f}");
                        }
                        // A successful sync rewrites the price table and its
                        // sync stamp; repriced>0 additionally changes
                        // visible USD.
                        self.views_stale = true;
                        self.load_page_data(Page::Prices, context);
                        if r.repriced > 0 || r.rules_repass {
                            // Costs of events on arbitrary days changed.
                            self.cube_rebuild = true;
                            self.start_scan(context);
                        }
                    }
                    Err(e) => {
                        diag!("[prices] refresh failed: {e}");
                        self.price_refresh_error = Some(short_error(&e));
                    }
                }
            }
        }
    }

    fn view(&self, _input: &(), context: &mut ViewContext<Self>) -> View {
        // Publish the render locale before any `tr`/`tf!` resolves text.
        i18n::set_lang(i18n::Lang::from_config(&self.config.lang));
        // WM_CLOSE → Msg::CloseRequested (idempotent once the HWND exists).
        close_hook::ensure_installed(&context.sender());
        context.window_title("GlobalTokenTracker");
        context.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .client_size(1180.0, 780.0)
                .theme(match self.config.window_theme.as_str() {
                    "light" => WindowTheme::Light,
                    "dark" => WindowTheme::Dark,
                    _ => WindowTheme::System,
                })
                // Real .ico path materialized beside the ledger — the
                // embedded resource still drives Explorer/shortcut icons.
                .icon(window_icon_path()),
        );
        let snap = self.snap.as_ref();
        let theme = &self.theme;

        // Two page layers take turns. At rest one (`rest_idx`) shows the page.
        // A flight mounts the new page in the OTHER layer — hidden until its
        // composition visuals resolve — while the old page stays exactly where
        // it is (the same mounted tree: real charts, scroll position intact)
        // and the two slide as one push. Afterwards the old layer unmounts and
        // the new one simply becomes the resting layer, so a settled page is
        // never re-mounted (canvas swapchains would flash). Layers and the
        // blocks of each page are composition hosts: their motion runs on the
        // compositor, not through this view.
        for (i, host) in self.layer_hosts.iter().enumerate() {
            host.attach(context, LAYER_HOST_KEYS[i]);
            for (j, b) in self.block_hosts[i].iter().enumerate() {
                b.attach(context, BLOCK_HOST_KEYS[i][j]);
            }
        }
        let rest = self.rest_idx;
        let layer = |idx: usize, page: View, hidden: bool| {
            KeyedView::new(
                LAYER_KEYS[idx],
                Grid::new()
                    .element_ref(&self.layer_hosts[idx].r)
                    .grid_row(0)
                    // Layout never changes during a flight — the compositor
                    // moves the visuals on top of it. Until the animation has
                    // started (`Phase::Prepare`) the incoming layer is simply
                    // not shown, so the new page can't flash at rest first;
                    // it is revealed in the same commit the animation starts
                    // in. (Not a fade: opacity goes 0 → 1 in one step.)
                    .opacity(if hidden { 0.0 } else { 1.0 })
                    .keyed_children([KeyedView::new("page", page)]),
            )
        };
        let mut layers: Vec<KeyedView> = Vec::with_capacity(2);
        match &self.flight {
            None => {
                let page = self.page_view(self.page, rest, snap, context, self.canvas_ready);
                layers.push(layer(rest, page, false));
            }
            Some(f) => {
                let old = self.page_view(f.from, rest, snap, context, f.from_ready);
                layers.push(layer(rest, old, false));
                let inc = 1 - rest;
                let new = self.page_view(self.page, inc, snap, context, self.canvas_ready);
                layers.push(layer(inc, new, f.phase == Phase::Prepare));
            }
        }
        let content: View = Grid::new()
            .rows([GridLength::STAR])
            .grid_row(2)
            .keyed_children(layers);

        let item = |label: &'static str, page: Page| {
            SelectorBarItem::new()
                .text(label)
                .is_selected(self.page == page)
        };
        // Navigation lives in its own client row below the native caption
        // area. Pointer gestures here must never drag or maximize the window.
        let nav = SelectorBar::new()
            .on_selected_text_changed(context.callback(Msg::Nav))
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .grid_row(1)
            .collection_slot(
                SelectorBarSlot::Items,
                [
                    KeyedView::new("overview", item(t!("总览"), Page::Overview)),
                    KeyedView::new("speed", item(t!("速度"), Page::Speed)),
                    KeyedView::new("detail", item(t!("明细"), Page::Detail)),
                    KeyedView::new("quota", item(t!("配额"), Page::Quota)),
                    KeyedView::new("devices", item(t!("设备"), Page::Devices)),
                    KeyedView::new("sources", item(t!("数据源"), Page::Sources)),
                    KeyedView::new("scheduling", item(t!("调度"), Page::Scheduling)),
                    KeyedView::new("prices", item(t!("价格"), Page::Prices)),
                    KeyedView::new("settings", item(t!("设置"), Page::Settings)),
                ],
            );
        // The top strip is reserved for branding, native window controls and
        // window dragging. Page navigation is outside this entire strip.
        let brand = StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(10.0)
            .vertical_alignment(VerticalAlignment::Center)
            .horizontal_alignment(HorizontalAlignment::Left)
            .margin(Thickness::new(12.0, 0.0, 0.0, 0.0))
            .grid_row(0)
            .children((
                // Embedded PNG (assets/icon-64.png) keeps the titlebar logo
                // identical to the window/tray icon with zero runtime files.
                ImageIcon::new()
                    .source_data(EncodedImage::from_static(include_bytes!(
                        "../../../assets/icon-64.png"
                    )))
                    .width(18.0)
                    .height(18.0),
                TextBlock::new()
                    .text("GlobalTokenTracker")
                    .font_size(13.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .vertical_alignment(VerticalAlignment::Center),
            ));
        // Filter chrome is a pinned strip between title bar and scrolling
        // page on the data pages (overview/detail); it collapses elsewhere.
        let chrome_state = ChromeState {
            apps: &self.app_filter,
            models: &self.model_filter,
            devices: &self.device_filter,
            refresh_secs: self.config.refresh_secs,
            open: self.open_menu,
        };
        let chrome: View = match (self.page, snap) {
            (Page::Overview | Page::Speed | Page::Detail, Some(s)) => {
                filter_chrome(s, theme, &chrome_state, context)
            }
            _ => Border::new().into(),
        };
        let mut chrome_rows: Vec<KeyedView> = Vec::with_capacity(2);
        if let UpdateState::Available(rel) | UpdateState::Downloading(rel) = &self.update
            && !self.update_banner_dismissed
        {
            chrome_rows.push(KeyedView::new(
                "update",
                update_banner(
                    theme,
                    &self.update,
                    rel,
                    self.update_error.as_deref(),
                    context,
                ),
            ));
        }
        chrome_rows.push(KeyedView::new("filters", chrome));
        let chrome = StackPanel::new().grid_row(2).keyed_children(chrome_rows);
        // Dropdown overlay renders last so its card floats above the page;
        // closed → an empty background-less Border (XAML skips hit-testing
        // null-background elements, so it never swallows clicks).
        let overlay: View = match (self.page, snap, self.open_menu) {
            (Page::Overview | Page::Speed | Page::Detail, Some(s), Some(kind)) => {
                dropdown_overlay(s, theme, &chrome_state, kind, context)
            }
            _ => Border::new().grid_row(3).into(),
        };
        // Close prompt: quit vs hide-to-tray, with a remember checkbox.
        // The secondary button is disabled when no tray icon exists (hiding
        // would strand the process with no way back).
        let mut dlg_rows: Vec<View> = vec![
            TextBlock::new()
                .text(tr("要彻底退出，还是隐藏到托盘继续后台统计？"))
                .font_size(theme.body_size)
                .text_wrapping(windows_reactor::TextWrapping::Wrap)
                .into(),
        ];
        if self.tray.is_none() {
            dlg_rows.push(
                TextBlock::new()
                    .text(tr("托盘图标不可用"))
                    .font_size(theme.label_size)
                    .foreground(theme.subtle)
                    .into(),
            );
        }
        dlg_rows.push(
            CheckBox::new()
                .is_checked(self.close_remember)
                .on_is_checked_changed(context.callback(Msg::CloseRemember))
                .content(
                    TextBlock::new()
                        .text(tr("记住我的选择"))
                        .font_size(theme.body_size),
                ),
        );
        let close_dialog: View = ContentDialog::new()
            .is_open(self.close_prompt)
            .title(tr("关闭 GlobalTokenTracker"))
            .primary_button_text(tr("彻底退出"))
            .secondary_button_text(tr("隐藏到托盘"))
            .is_secondary_button_enabled(self.tray.is_some())
            .close_button_text(tr("取消"))
            .on_closed(context.callback(Msg::CloseDialogResult))
            .content(
                StackPanel::new()
                    .spacing(12.0)
                    .keyed_children(keyed(dlg_rows)),
            );
        // "pagehost" stays mounted across navs — the exit slide is driven by
        // the "leave" layer above, so this frame needs no transition chrome.
        // Root must be a Grid: a vertical StackPanel offers children infinite
        // height, which makes the page ScrollViewer measure at full content
        // size and never scroll. Star row bounds the scroll area.
        // Click-away backdrop: while a picker is open, a transparent layer over
        // the chrome row + page swallows the press and closes the menu. It sits
        // ABOVE the page but BELOW the chrome and the dropdown card, so the
        // pickers themselves (switch menus with one click) and the card keep
        // working. The Transparent (not null) background is what makes it
        // hit-testable.
        let mut root: Vec<KeyedView> = vec![
            KeyedView::new(
                "titlebar",
                TitleBar::new()
                    .preferred_height(WindowTitleBarHeight::Standard)
                    .grid_row(0),
            ),
            KeyedView::new(
                "nav",
                Border::new()
                    .grid_row(1)
                    .min_height(44.0)
                    .border_brush(theme.divider)
                    .border_thickness(Thickness::new(0.0, 0.0, 0.0, 1.0))
                    .content(
                        ScrollViewer::new()
                            .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
                            .vertical_scroll_bar_visibility(ScrollBarVisibility::Disabled)
                            .content(nav),
                    ),
            ),
            KeyedView::new("brand", brand),
            KeyedView::new(
                "pagehost",
                Border::new()
                    .grid_row(3)
                    .background(
                        theme
                            .page_bg
                            .unwrap_or(Brush::Theme(ThemeBrush::SolidBackground)),
                    )
                    .border_brush(theme.divider)
                    .border_thickness(Thickness::new(0.0, 1.0, 0.0, 0.0))
                    .content(content),
            ),
        ];
        if self.open_menu.is_some()
            && matches!(self.page, Page::Overview | Page::Speed | Page::Detail)
        {
            root.push(KeyedView::new(
                "backdrop",
                Border::new()
                    .grid_row(2)
                    .grid_row_span(2)
                    .background(Brush::Solid(Color::argb(0, 0, 0, 0)))
                    .on_pointer_pressed(context.callback(|_: PointerEventInfo| Msg::CloseMenu)),
            ));
        }
        root.push(KeyedView::new("chrome", chrome));
        root.push(KeyedView::new("overlay", overlay));
        root.push(KeyedView::new("closedlg", close_dialog));
        // Forget-device confirmation (destructive: its synced rows go too).
        let forget_name = self
            .forget_target
            .as_ref()
            .map(|(_, n)| n.clone())
            .unwrap_or_default();
        root.push(KeyedView::new(
            "forgetdlg",
            ContentDialog::new()
                .is_open(self.forget_target.is_some())
                .title(tf!("忘记“{}”？", forget_name))
                .primary_button_text(tr("忘记设备"))
                .close_button_text(tr("取消"))
                .on_closed(context.callback(Msg::ForgetResult))
                .content(
                    TextBlock::new()
                        .text(tr(
                            "将从本机删除它已同步的用量。如需再次查看，请用新的配对码重新配对。",
                        ))
                        .font_size(theme.body_size)
                        .text_wrapping(windows_reactor::TextWrapping::Wrap),
                ),
        ));
        Grid::new()
            .rows([
                GridLength::Auto,
                GridLength::Auto,
                GridLength::Auto,
                GridLength::STAR,
            ])
            .keyed_children(root)
    }
}

impl Shell {
    /// Run a full price refresh off the UI thread; no-op while one is in flight.
    fn spawn_price_refresh(&mut self, context: &ComponentContext<Self>) {
        if self.prices_refreshing {
            return;
        }
        self.prices_refreshing = true;
        self.price_refresh_error = None;
        context.spawn_background(|_| {
            power::worker("gtt-prices");
            Msg::PricesDone(
                Store::open(&db_path())
                    .and_then(|s| globaltokentracker_core::pricing::refresh(&s))
                    .map_err(|e| e.to_string()),
            )
        });
    }

    /// Theme tokens for the current config and window theme.
    fn resolve_theme(&self) -> Theme {
        Theme::resolve(
            &self.config.theme,
            theme::is_light(&self.config.window_theme),
        )
    }

    /// Programmatic quit — `allow_next_close` lets the close we requested
    /// pass our own WM_CLOSE swallow (without it the subclass eats it).
    fn quit_now(&mut self, context: &ComponentContext<Self>) {
        close_hook::allow_next_close();
        let _ = context.window().request_close();
    }

    /// Raw top-level blocks of one page, assembled by `page_view`/`frame_page`.
    /// `canvas_ready`: how many Overview charts may be mounted (see
    /// `OverviewArgs`).
    fn page_blocks(
        &self,
        page: Page,
        snap: Option<&Snapshot>,
        context: &mut ViewContext<Self>,
        canvas_ready: usize,
    ) -> Vec<View> {
        let theme = &self.theme;
        match page {
            Page::Overview => overview_page(
                snap,
                theme,
                context,
                &OverviewArgs {
                    scanning: self.scanning,
                    config: &self.config,
                    editing: self.editing,
                    trend: &self.trend,
                    heat: &self.heat,
                    clock_dial: &self.clock_dial,
                    cols: self.overview_cols,
                    ruler: &self.ruler,
                    donuts: &self.donuts,
                    cal: pages::CalView {
                        month: self.cal_month,
                        anchor: self.cal_anchor,
                        clock_anchor: self.clock_anchor,
                    },
                    canvas_ready,
                },
            ),
            Page::Speed => speed_page(
                snap,
                theme,
                context,
                &SpeedArgs {
                    by_app: self.speed_by_app,
                    metric: self.speed_metric,
                    page: self.speed_page,
                    cols: self.overview_cols,
                    cal: CalView {
                        month: self.cal_month,
                        anchor: self.cal_anchor,
                        clock_anchor: self.clock_anchor,
                    },
                    clock_dial: &self.clock_dial,
                },
            ),
            Page::Detail => detail_page(
                snap,
                theme,
                context,
                &TableArgs {
                    width: self.table_w,
                    ruler: &self.table_rulers[0],
                },
            ),
            Page::Quota => quota_page(snap, theme, &self.quota_collapsed, context),
            Page::Devices => devices_page(
                snap,
                theme,
                context,
                &DevicesArgs {
                    info: self.devices.as_ref(),
                    nearby: &self.nearby,
                    syncing: self.peer_syncing && self.peer_sync_shown,
                    discovering: self.discovering,
                    error: self.peer_error.as_deref(),
                    pair: &self.pair,
                },
            ),
            Page::Sources => sources_page(self.sources.as_deref(), theme),
            Page::Scheduling => scheduling::page(&self.scheduler, theme, context),
            Page::Prices => prices_page(
                self.prices.as_ref(),
                theme,
                context,
                &TableArgs {
                    width: self.table_w,
                    ruler: &self.table_rulers[1],
                },
                &PriceSearch {
                    query: &self.price_query,
                    disputed: self.price_disputed,
                    shown: &self.price_shown,
                },
                &PriceRefresh {
                    active: self.prices_refreshing,
                    error: self.price_refresh_error.as_deref(),
                },
            ),
            Page::Settings => settings_page(
                &self.config,
                &SettingsArgs {
                    update: &self.update,
                    update_error: self.update_error.as_deref(),
                    external_interface_enabled: self.external_interface_enabled,
                    external_interface_error: self.external_interface_error.as_deref(),
                    share: &self.share,
                    fx: self.snap.as_ref().map(|s| s.fx.clone()).unwrap_or_default(),
                    fx_refreshing: self.fx_refreshing,
                    fx_error: self.fx_refresh_error.as_deref(),
                },
                theme,
                context,
            ),
        }
    }

    /// Top-level block spacing — Overview/Settings use the section gap,
    /// the list pages pack tighter (10 DIP, their historical rhythm).
    fn page_gap(&self, page: Page) -> f64 {
        match page {
            Page::Overview | Page::Speed | Page::Settings | Page::Devices => self.theme.section_gap,
            _ => 10.0,
        }
    }

    /// Assemble one page for page layer `layer` — its blocks bind to that
    /// layer's block hosts.
    fn page_view(
        &self,
        page: Page,
        layer: usize,
        snap: Option<&Snapshot>,
        context: &mut ViewContext<Self>,
        canvas_ready: usize,
    ) -> View {
        let blocks = self.page_blocks(page, snap, context, canvas_ready);
        if page == self.page {
            self.block_count.set(blocks.len());
        }
        let frame = if page == Page::Scheduling && self.scheduler.is_workspace() {
            pages::canvas_frame_page
        } else if page == Page::Scheduling {
            pages::bounded_frame_page
        } else {
            pages::frame_page
        };
        frame(
            &self.theme,
            self.page_gap(page),
            blocks,
            &Slide {
                hosts: &self.block_hosts[layer],
            },
        )
    }

    /// Begin a page switch. Needs the resting layer's composition visual (its
    /// width is the slide distance); until that has resolved — the very first
    /// moments after launch — or when the GPU slide is off, the page just
    /// switches instantly.
    fn start_flight(&mut self, from: Page, context: &ComponentContext<Self>) {
        // A flight still gliding: the page that was arriving becomes the
        // resting one (snapped home) and this switch starts from it.
        if self.flight.take().is_some() {
            self.commit_flight(true);
        }
        let rest = self.rest_idx;
        let inc = 1 - rest;
        let w = self.layer_hosts[rest].width().filter(|w| *w >= 1.0);
        let Some(w) = w.filter(|_| gpu_slide::enabled()) else {
            diag!("[nav] gpu slide unavailable — instant switch");
            self.canvas_ready = usize::MAX;
            return;
        };
        // Forward nav → new page enters from the right and the old one is
        // pushed out to the left; backward flips the direction.
        let dir = if (self.page as u8) > (from as u8) {
            1.0
        } else {
            -1.0
        };
        // The incoming layer is a fresh element: forget stale visuals so
        // `nav_go` waits for its own.
        self.layer_hosts[inc].reset();
        self.block_hosts[inc].iter().for_each(LayerHost::reset);
        let from_ready = std::mem::replace(&mut self.canvas_ready, 0);
        self.flight_seq += 1;
        let id = self.flight_seq;
        self.flight = Some(Flight {
            id,
            from,
            dir,
            w,
            phase: Phase::Prepare,
            from_ready,
            waited: 0,
            t0: std::time::Instant::now(),
        });
        self.arm_nav_go(id, context);
    }

    /// The arriving page becomes the resting layer; the leaving layer's
    /// element unmounts on the next `view()`. `snap`: it may still be
    /// gliding (interrupted flight) — pull it home.
    fn commit_flight(&mut self, snap: bool) {
        let old = self.rest_idx;
        let new = 1 - old;
        if snap {
            self.layer_hosts[new].snap_home();
            self.block_hosts[new].iter().for_each(LayerHost::snap_home);
        }
        self.layer_hosts[old].reset();
        self.block_hosts[old].iter().for_each(LayerHost::reset);
        self.rest_idx = new;
    }

    /// Recompute the rows the prices page shows from the search state.
    fn refresh_price_view(&mut self) {
        self.price_shown = match &self.prices {
            None => Arc::default(),
            Some(t) if self.price_query.trim().is_empty() && !self.price_disputed => t.rows.clone(),
            Some(t) => Arc::new(filter_prices(
                &t.rows,
                &self.price_query,
                self.price_disputed,
            )),
        };
    }

    /// One frame's wait before checking the new page's visuals.
    fn arm_nav_go(&self, id: u64, context: &ComponentContext<Self>) {
        context.spawn_background(move |_| {
            power::name_thread("gtt-anim");
            std::thread::sleep(std::time::Duration::from_millis(16));
            Msg::NavGo(id)
        });
    }

    /// The new page is mounted: start the compositor animations. Visuals of
    /// freshly mounted elements resolve one composition commit after mount,
    /// so poll a few frames for them; whatever is still missing then simply
    /// rides its layer (no per-block spring) rather than delaying the slide.
    fn nav_go(&mut self, id: u64, context: &ComponentContext<Self>) {
        let n = self.block_count.get().min(MAX_SLIDE);
        let (rest, inc) = (self.rest_idx, 1 - self.rest_idx);
        let Some(f) = self.flight.as_mut().filter(|f| f.id == id) else {
            return;
        };
        if f.phase != Phase::Prepare {
            return;
        }
        f.waited += 1;
        let ready = self.layer_hosts[inc].visual().is_some()
            && self.layer_hosts[rest].visual().is_some()
            && self.block_hosts[inc][..n]
                .iter()
                .all(|h| h.visual().is_some());
        if !ready && f.waited < 8 {
            self.arm_nav_go(id, context);
            return;
        }
        let (dir, w) = (f.dir, f.w);
        let total = std::time::Duration::from_secs_f64(gpu_slide::FLIGHT_S);
        // Start the incoming layer first: without it moving there is nothing
        // to show, so give up and switch instantly.
        if !self.layer_hosts[inc].slide_x(&gpu_slide::enter_frames(dir, w), total) {
            diag!("[nav] incoming layer not animatable — instant switch");
            self.flight = None;
            self.commit_flight(false);
            self.canvas_ready = usize::MAX;
            return;
        }
        let left = self.layer_hosts[rest].slide_x(&gpu_slide::leave_frames(dir, w), total);
        let blocks = (0..n)
            .filter(|&i| self.block_hosts[inc][i].slide_x(&gpu_slide::block_frames(i, dir), total))
            .count();
        f.phase = Phase::Running;
        diag!(
            "[nav] gpu flight: w={w:.0} leave={left} blocks={blocks}/{n} after {}ms",
            f.t0.elapsed().as_millis()
        );
        // Charts mount in the slide's back half, one per frame: the page is
        // nearly in place by then, and the compositor keeps the motion smooth
        // while the UI thread pays for the GPU devices — so no single turn eats
        // all five, and nothing waits for the flight to end.
        self.arm_canvas_stage(context, gpu_slide::CHARTS_AFTER_MS);
        context.spawn_background(move |_| {
            power::name_thread("gtt-anim");
            std::thread::sleep(std::time::Duration::from_secs_f64(
                gpu_slide::SETTLE_AFTER_S,
            ));
            Msg::NavSettled(id)
        });
    }

    /// Low-frequency vendor quota poll (spec §6.9) — `GTT_NO_QUOTA` disables.
    /// Errors are logged via diag only; the quota page shows what landed.
    fn poll_quota_if_stale(&mut self, context: &ComponentContext<Self>) {
        let stale = self
            .quota_at
            .map(|t| t.elapsed().as_secs() > QUOTA_POLL_SECS)
            .unwrap_or(true);
        if !stale || std::env::var_os("GTT_NO_QUOTA").is_some() {
            return;
        }
        self.quota_at = Some(std::time::Instant::now());
        context.spawn_background(|_| {
            power::worker("gtt-quota");
            let mut n = 0usize;
            let mut errs = Vec::new();
            match Store::open(&db_path()) {
                Ok(store) => {
                    for o in globaltokentracker_core::quota::poll_all() {
                        if let Some(e) = o.error {
                            errs.push(format!("{}: {e}", o.app));
                        }
                        for q in o.quotas {
                            if matches!(store.insert_quota(&q), Ok(true)) {
                                n += 1;
                            }
                        }
                    }
                }
                Err(e) => errs.push(e.to_string()),
            }
            Msg::QuotaDone(n, errs)
        });
    }

    /// Apply + persist a range pick, then reload aggregates (totals/by_app
    /// are indexed; the rescan path is cheap).
    /// The local day a custom window sits in, when it stays inside one —
    /// the only case the hour dial is live.
    fn single_day(&self) -> Option<jiff::civil::Date> {
        use globaltokentracker_core::viewmodel::local_date;
        match self.range {
            Range::Custom { start_ms, .. } if self.range.hourly() => Some(local_date(start_ms)),
            _ => None,
        }
    }

    fn set_range(&mut self, r: Range, context: &ComponentContext<Self>) {
        if r == self.range {
            return;
        }
        self.range = r;
        self.config.range = r.key().to_string();
        if let Range::Custom { start_ms, end_ms } = r {
            self.config.range_start_ms = Some(start_ms);
            self.config.range_end_ms = Some(end_ms);
            if let Some(day) = self.single_day() {
                let span = pages::hour_span(day, start_ms, end_ms);
                self.clock_dial.sync(Some(span), self.clock_anchor);
            } else {
                self.clock_dial.sync(None, None);
            }
        } else {
            self.clock_dial.sync(None, None);
        }
        self.config.save();
        self.refresh_views(context, false);
    }

    fn start_scan(&mut self, context: &ComponentContext<Self>) {
        if !self.scanning {
            diag!("[scan] start");
            self.scanning = true;
            let force_views = std::mem::take(&mut self.views_stale);
            let prev_cube = if std::mem::take(&mut self.cube_rebuild) {
                None
            } else {
                self.snap.as_ref().map(|s| Arc::clone(&s.cube))
            };
            let req = LoadReq {
                range: self.range,
                apps: self.app_filter.clone(),
                models: self.model_filter.clone(),
                devices: self.device_filter.clone(),
                force_prices: false,
                force_views,
                prev_cube,
                filter_gen: self.filter_gen,
                scan: true,
            };
            context.spawn_background(move |_| {
                power::worker("gtt-scan");
                match load_all(req) {
                    Ok(s) => Msg::Loaded(s),
                    Err(e) => Msg::Failed(e),
                }
            });
        }
    }

    fn reload_views(&mut self, context: &ComponentContext<Self>) {
        if !self.scanning {
            diag!("[reload] fast view reload without disk scan");
            self.scanning = true;
            let force_views = std::mem::take(&mut self.views_stale);
            let prev_cube = if std::mem::take(&mut self.cube_rebuild) {
                None
            } else {
                self.snap.as_ref().map(|s| Arc::clone(&s.cube))
            };
            let req = LoadReq {
                range: self.range,
                apps: self.app_filter.clone(),
                models: self.model_filter.clone(),
                devices: self.device_filter.clone(),
                force_prices: false,
                force_views,
                prev_cube,
                filter_gen: self.filter_gen,
                scan: false,
            };
            context.spawn_background(move |_| {
                power::worker("gtt-reload");
                match load_all(req) {
                    Ok(s) => Msg::Loaded(s),
                    Err(e) => Msg::Failed(e),
                }
            });
        }
    }

    /// The statistics range or the tool/model scope changed: re-fold the
    /// numbers from the in-memory cube right here on the UI thread — no file
    /// scan, no SQL, no background hop. (This used to be a full scan plus ten
    /// aggregate queries: 400–550ms.) Only the detail rows still come from the
    /// ledger, as a one-row-page query.
    fn refresh_views(&mut self, context: &ComponentContext<Self>, filters_changed: bool) {
        let (apps, models) = (self.app_filter.clone(), self.model_filter.clone());
        let devices = self.device_filter.clone();
        let range = self.range;
        let t_fold = std::time::Instant::now();
        let applied = self.snap.as_mut().is_some_and(|s| {
            let ok = s.vm.apply_cube_for(
                &s.cube,
                range,
                apps.as_deref(),
                models.as_deref(),
                devices.as_deref(),
            );
            if ok && filters_changed {
                s.detail.total = s.cube.event_count(apps.as_deref(), models.as_deref());
            }
            ok
        });
        diag!(
            "[view] {} → {:?} in {}us (cube: {applied})",
            if filters_changed { "filters" } else { "range" },
            range,
            t_fold.elapsed().as_micros(),
        );
        if applied {
            if filters_changed {
                self.prune_filters();
                self.load_detail_page(0, context);
            }
        } else {
            // Nothing loaded yet, or the cube can't answer this window exactly
            // (a custom edge off the day boundary): the SQL path, in the
            // background. Fast reload without rescanning all files on disk!
            diag!("[view] cube declined — fast reload without disk scan");
            self.views_stale = true;
            self.scanning = false;
            self.reload_views(context);
        }
    }

    /// Tool names / models can vanish from the ledger (pruned data) or fall
    /// out of scope when the tool selection narrows; keep the persisted
    /// filters honest — drop dead names and collapse back to `None` on full
    /// coverage.
    fn prune_filters(&mut self) {
        let Some((apps, models)) = self
            .snap
            .as_ref()
            .map(|s| (s.vm.apps.clone(), s.vm.models.clone()))
        else {
            return;
        };
        if let Some(f) = &mut self.app_filter {
            f.retain(|a| apps.contains(a));
            if apps.iter().all(|a| f.contains(a)) {
                self.app_filter = None;
            }
            if self.app_filter != self.config.apps {
                self.config.apps = self.app_filter.clone();
                self.config.save();
            }
        }
        if let Some(f) = &mut self.model_filter {
            f.retain(|m| models.contains(m));
            if models.iter().all(|m| f.contains(m)) {
                self.model_filter = None;
            }
            if self.model_filter != self.config.models {
                self.config.models = self.model_filter.clone();
                self.config.save();
            }
        }
    }

    /// Fetch one page of detail rows under the current filters. The total
    /// comes from the cube, so the background query is just the page.
    fn load_detail_page(&self, page: i64, context: &ComponentContext<Self>) {
        let apps = self.app_filter.clone();
        let models = self.model_filter.clone();
        let total = self
            .snap
            .as_ref()
            .map(|s| s.cube.event_count(apps.as_deref(), models.as_deref()));
        context.spawn_background(move |_| {
            power::worker("gtt-detail");
            let res = Store::open(&db_path()).and_then(|s| {
                let rows = s.events_page(
                    DETAIL_PAGE_SIZE,
                    page * DETAIL_PAGE_SIZE,
                    apps.as_deref(),
                    models.as_deref(),
                )?;
                let total = match total {
                    Some(t) => t,
                    None => s.event_count(apps.as_deref(), models.as_deref())?,
                };
                Ok((rows, total))
            });
            match res {
                Ok((rows, total)) => Msg::DetailLoaded(Arc::new(rows), total, page),
                Err(e) => Msg::Failed(e.to_string()),
            }
        });
    }

    /// Schedule the next chart-mount step in `delay_ms`, or finish the
    /// stagger (all charts allowed, and always so off the Overview page —
    /// nothing to mount).
    fn arm_canvas_stage(&mut self, context: &ComponentContext<Self>, delay_ms: u64) {
        // trend + up to 6 share donuts + speed donut + heatmap
        const CHARTS: usize = 9;
        if self.page != Page::Overview || self.canvas_ready >= CHARTS {
            self.canvas_ready = usize::MAX;
            return;
        }
        context.spawn_background(move |_| {
            power::name_thread("gtt-anim");
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            Msg::CanvasStage
        });
    }

    /// Kick a light table query for a lazy page. One in flight per page; a
    /// request that arrives meanwhile re-runs once on completion, so the
    /// table always ends up reflecting the newest ledger state.
    fn load_devices(&self, context: &ComponentContext<Self>) {
        context.spawn_background(|_| {
            power::worker("gtt-peers");
            Msg::DevicesLoaded(devices::load(&db_path()))
        });
    }

    fn load_viewers(&self, context: &ComponentContext<Self>) {
        context.spawn_background(|_| {
            power::worker("gtt-viewers");
            Msg::ViewersLoaded(devices::viewers(&db_path()))
        });
    }

    /// Pull paired devices in the background (one at a time; nothing to do
    /// without paired devices).
    /// Turn LAN sharing on or off (Settings switch, or after a mutual pairing).
    fn set_share_lan(&mut self, on: bool, context: &ComponentContext<Self>) {
        self.config.share_lan = on;
        self.config.save();
        self.share.error = None;
        if on && self.share.running.is_none() && !self.share.busy {
            self.share.busy = true;
            context.spawn_background(|_| {
                power::worker("gtt-share");
                Msg::ShareStarted(devices::start_sharing(&db_path()).map(Box::new))
            });
        } else if !on && let Some(running) = self.share.running.take() {
            self.share.busy = true;
            self.share.refresh_status();
            context.spawn_background(move |_| {
                power::worker("gtt-share-stop");
                running.stop();
                Msg::ShareStopped
            });
        }
    }

    /// `if_due`: the background knock — core decides whether anything runs
    /// (interval elapsed, or a device just paired with this one), so it does
    /// not wait for a known peer and shows no progress.
    fn sync_peers(&mut self, if_due: bool, context: &ComponentContext<Self>) {
        let has_peers = self.devices.as_ref().is_some_and(|d| !d.peers.is_empty());
        if self.peer_syncing || (!if_due && !has_peers) {
            return;
        }
        self.peer_syncing = true;
        self.peer_sync_shown = !if_due;
        context.spawn_background(move |_| {
            power::worker("gtt-peer-sync");
            Msg::PeersSynced(devices::sync(&db_path(), if_due))
        });
    }

    /// The statistics device scope changed: persist it and re-fold.
    fn set_device_filter(&mut self, filter: Option<Vec<String>>, context: &ComponentContext<Self>) {
        if filter == self.device_filter {
            return;
        }
        self.device_filter = filter;
        self.config.devices = self.device_filter.clone();
        self.config.save();
        self.filter_gen += 1;
        self.refresh_views(context, true);
    }

    fn load_page_data(&mut self, page: Page, context: &ComponentContext<Self>) {
        let Some(slot) = lazy_slot(page) else {
            return;
        };
        if self.page_loading[slot] {
            self.page_again[slot] = true;
            return;
        }
        self.page_loading[slot] = true;
        context.spawn_background(move |_| {
            power::worker("gtt-page");
            Msg::PageData(page, load_page(page))
        });
    }
}

/// Process start — the reference for the `[startup]` diagnostics.
#[cfg(windows)]
struct SingleInstanceGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
#[allow(unsafe_code)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn ensure_single_instance() -> Option<SingleInstanceGuard> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let test_instance = if cfg!(debug_assertions) {
        std::env::var("GTT_UI_TEST_INSTANCE").ok().filter(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
    } else {
        None
    };
    let mutex_name = test_instance.map_or_else(
        || "Local\\GlobalTokenTracker_SingleInstance_Mutex".to_owned(),
        |id| format!("Local\\GlobalTokenTracker_Test_{id}"),
    );
    let name: Vec<u16> = format!("{mutex_name}\0").encode_utf16().collect();

    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if tray::focus_existing_window() {
                CloseHandle(handle);
                std::process::exit(0);
            }
            diag!("[startup] existing instance mutex held but no window on desktop — continuing");
        }
        Some(SingleInstanceGuard { handle })
    }
}

#[cfg(not(windows))]
fn ensure_single_instance() -> Option<()> {
    Some(())
}

/// Process start — the reference for the `[startup]` diagnostics.
static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn main() {
    let _instance_guard = ensure_single_instance();
    START.get_or_init(std::time::Instant::now);
    #[cfg(windows)]
    if diag_enabled() {
        diag_console();
    }
    // Stowed WinRT exceptions produce zero stderr; a Rust panic (e.g. inside a
    // spawn_background closure) lands in this hook instead.
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::capture();
        let msg = format!("PANIC: {info}\n{bt}");
        let _ = std::fs::write("gtt_panic.log", &msg);
        eprintln!("{msg}");
    }));
    if let Err(e) = App::run_component::<Shell>(()) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}
