//! Native dispatch page and demand-rendered Direct2D workflow editor.

use crate::{Msg, Shell, theme::Theme, widgets as w};
use globaltokentracker_core::scheduler::{self as model, Route, Settings, Snapshot, Workflow};
use serde_json::{Value, json};
use windows_reactor::*;

/// Sub-pages, in SelectorBar order: (state id, label).
const TABS: [(&str, &str); 3] = [
    ("workflow", "工作流画布"),
    ("sources", "来源与模型池"),
    ("profiles", "调用配置"),
];

// MenuFlyout labels. Native menu items report only their label, so the same
// constants build the menus and resolve the pick in `State::menu_action`.
const M_PLAN: &str = "运行前预检";
const M_PARAMS: &str = "工作流设置";
const M_LOAD: &str = "刷新状态";
const M_RESCAN: &str = "重新扫描来源";
const M_START: &str = "启动协调器服务";
const M_RELOAD: &str = "放弃修改并重载";
const M_MASTER: &str = "母节点 · Codex 接口";
const M_FIT: &str = "适应全图";
const M_ZOOM_IN: &str = "放大";
const M_ZOOM_OUT: &str = "缩小";
const M_ZOOM_RESET: &str = "实际大小（100%）";
const M_PAN_ON: &str = "平移工具";
const M_PAN_OFF: &str = "退出平移工具";
const M_MODE_CONNECT: &str = "切换到连线模式";
const M_MODE_MOVE: &str = "切换到移动模式";
const M_RUN_SHOW: &str = "显示输入与结果";
const M_RUN_HIDE: &str = "隐藏输入与结果";

pub enum Action {
    Load,
    Refresh,
    Start,
    Save,
    Reload,
    Check(String),
    Received(Result<Value, String>),
    StopDrag,
    CaptureLost,
    EditNode,
    Menu(Box<Action>),
    /// A native MenuFlyout item was picked; the label is resolved against the
    /// current state (see `State::menu_action`) so the callback stays a ZST
    /// and the flyout is not rebuilt on every render.
    MenuPick(String),
    /// SelectorBar reports the selected item's text, not its key.
    TabNamed(String),
    CloseMenu,
    ClearSelection,
    ClearFlash(u64),
    Enabled(bool),
    Directed(bool),
    Source(String, bool),
    Query(String),
    Ttl(String),
    Pool(String),
    RemovePool(String),
    MovePool(usize, i32),
    New,
    Select(usize),
    Name(String),
    Budget(String),
    Parallel(String),
    Supervision(bool),
    AddMaster,
    DisconnectNode,
    AddTemplate(usize),
    ApplyTemplate(usize),
    Role(String),
    RemoveWorkflow,
    TaskEditor(String),
    ApplyTaskEditor,
    CallPreset(usize),
    ProfileEditor(String),
    ApplyProfile,
    Tab(&'static str),
    Section(&'static str, bool),
    Plan,
    PickTemplate(Option<usize>),
    ShowParams,
    ToggleRun,
    TogglePan,
    ToggleConnectMode,
    Fit,
    CloseInspector,
    Acknowledge(bool),
    Collect(String),
    AddFallback(String),
    ClearFallback,
    NodeTitle(String),
    Tags(String),
    Param(String, String),
    SelectNode(usize),
    RemoveNode,
    MoveNode(i32),
    Chain,
    Unlink,
    Press(PointerEventInfo),
    Move(PointerEventInfo),
    Release(PointerEventInfo),
    Zoom(f64),
    Palette(i32),
    WorkflowId(String),
    Input(String),
    Run,
    Poll(String),
    Cancel,
    Detail(String),
}

#[derive(Clone)]
enum Drag {
    Model(usize),
    Node(usize, f64, f64),
    Edge(usize),
    Pan(f64, f64),
}

#[derive(Default)]
struct Gesture {
    current: Option<Drag>,
    capture_lost: bool,
}

impl Gesture {
    fn begin(&mut self, drag: Drag) {
        self.current = Some(drag);
        self.capture_lost = false;
    }
    fn cancel(&mut self) {
        self.current = None;
        self.capture_lost = false;
    }
    fn lost(&mut self) {
        self.capture_lost = true;
    }
    fn movement(&mut self) -> Option<Drag> {
        if self.capture_lost {
            self.cancel();
            None
        } else {
            self.current.clone()
        }
    }
    fn release(&mut self) -> Option<Drag> {
        // WinUI releases capture before enqueuing PointerReleased. Keep the
        // drop until that queued release; a move or cancellation cancels loss.
        self.capture_lost = false;
        self.current.take()
    }
}

pub struct State {
    pub draft: Settings,
    snapshot: Snapshot,
    pub loaded: bool,
    busy: bool,
    saving: bool,
    dirty: bool,
    error: Option<String>,
    notice: String,
    query: String,
    ttl: String,
    selected: usize,
    node: Option<usize>,
    drag: Gesture,
    pan: (f64, f64),
    zoom: f64,
    palette: usize,
    workflow_id: String,
    input: String,
    run_id: String,
    detail: Option<Value>,
    report_detail: Option<Value>,
    detail_text: String,
    report_text: String,
    inv: windows_canvas::Invalidator,
    templates: Vec<model::Template>,
    probe_supervision: bool,
    task_editor: String,
    profile_editor: String,
    tab: &'static str,
    template_index: usize,
    show_params: bool,
    show_node_details: bool,
    pointer: (f64, f64),
    hover: Option<usize>,
    flash: Option<(usize, u64, std::time::Instant)>,
    flash_sequence: u64,
    open_menu: Option<&'static str>,
    /// Canvas-space anchor (pointer position) for the node context menu.
    menu_at: (f64, f64),
    /// Inspector sections the user unfolded (collapsed by default).
    expanded: std::collections::BTreeSet<&'static str>,
    show_run: bool,
    pan_tool: bool,
    /// Canvas gesture mode. Move: dragging a node moves it. Connect: dragging
    /// from anywhere on a node draws an edge. One gesture per mode — the old
    /// 20 DIP port strip beside the move area made each misfire as the other.
    connect_mode: bool,
    /// Last left-press on empty canvas, for double-click detection (pointer
    /// events carry no click count).
    last_blank_press: Option<(std::time::Instant, (f64, f64))>,
    viewport: std::rc::Rc<std::cell::Cell<(f64, f64)>>,
    submit_key: String,
    submit_signature: String,
    acknowledge: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            draft: Settings::default(),
            snapshot: Snapshot::default(),
            loaded: false,
            busy: false,
            saving: false,
            dirty: false,
            error: None,
            notice: String::new(),
            query: String::new(),
            ttl: "120".into(),
            selected: 0,
            node: None,
            drag: Gesture::default(),
            pan: (30.0, 40.0),
            zoom: 1.0,
            palette: 0,
            workflow_id: String::new(),
            input: String::new(),
            run_id: String::new(),
            detail: None,
            report_detail: None,
            detail_text: String::new(),
            report_text: String::new(),
            inv: windows_canvas::Invalidator::new(),
            templates: model::templates().unwrap_or_default(),
            probe_supervision: true,
            task_editor: String::new(),
            profile_editor: "{\"preset\":\"inherit\"}".into(),
            tab: "workflow",
            template_index: 0,
            show_params: false,
            show_node_details: false,
            pointer: (0.0, 0.0),
            hover: None,
            flash: None,
            flash_sequence: 0,
            open_menu: None,
            expanded: std::collections::BTreeSet::new(),
            show_run: false,
            pan_tool: false,
            connect_mode: false,
            last_blank_press: None,
            viewport: std::rc::Rc::new(std::cell::Cell::new((800.0, 500.0))),
            submit_key: String::new(),
            submit_signature: String::new(),
            acknowledge: false,
            menu_at: (0.0, 0.0),
        }
    }
}

impl State {
    fn request(&mut self, method: &'static str, params: Value, ctx: &ComponentContext<Shell>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        ctx.spawn_background(move |_| {
            Msg::Scheduling(Action::Received(
                (if method == "router_start" {
                    model::ensure_running().and_then(|()| {
                        model::call("router_scheduling", json!({"action":"refresh"}))
                    })
                } else {
                    model::call(method, params)
                })
                .map_err(|e| e.to_string()),
            ))
        });
    }
    fn active_run(&self) -> bool {
        self.detail
            .as_ref()
            .is_some_and(|d| matches!(d["state"].as_str(), Some("running" | "cancelling")))
    }
    fn run_disabled_reason(&self) -> Option<&'static str> {
        if !self.loaded {
            return Some("尚未连接调度服务；请在更多中启动服务或刷新状态。");
        }
        if self.busy {
            return Some("正在处理请求，请稍候。");
        }
        if self.active_run() {
            return Some("已有工作流正在运行，可查看结果或停止。");
        }
        let Some(w) = self.workflow().filter(|w| !w.nodes.is_empty()) else {
            return Some("先从模型库拖入模型，或通过添加节点选择职责模板。");
        };
        if self.dirty {
            return Some("请先保存修改，再运行工作流。");
        }
        if w.nodes
            .iter()
            .any(|n| n.node_type == "model" && n.route_id.is_empty())
        {
            return Some("请先为模型节点选择模型，或从模型库拖入节点框。");
        }
        if self.draft.mode == "directed"
            && self
                .draft
                .pool
                .iter()
                .all(|r| r.expires_at <= model::now_ms())
        {
            return Some("定向模型池为空或已到期，请在来源与模型池中选择模型。");
        }
        if !self.draft.enabled {
            return Some("调度总开关已关闭，请在来源与模型池中检查。");
        }
        if w.luna_supervision
            && !self
                .snapshot
                .supervisors
                .iter()
                .any(|v| v["workflow_id"].as_str() == Some(self.workflow_id.as_str()))
        {
            return Some("需要同一工作流的在线 Luna；可在工作流设置中关闭监工。");
        }
        None
    }
    fn reposition(&mut self, index: usize, x: f64, y: f64) {
        if let Some(node) = self.workflow_mut().and_then(|w| w.nodes.get_mut(index)) {
            let (x, y) = (x.clamp(-10000.0, 10000.0), y.clamp(-10000.0, 10000.0));
            if (node.x - x).abs() > 0.01 || (node.y - y).abs() > 0.01 {
                node.x = x;
                node.y = y;
                self.dirty = true;
            }
        }
    }
    fn pulse(&mut self, ctx: &ComponentContext<Shell>) {
        if let Some(node) = self.node {
            self.flash_sequence += 1;
            let id = self.flash_sequence;
            self.flash = Some((node, id, std::time::Instant::now()));
            ctx.spawn_background(move |_| {
                std::thread::sleep(std::time::Duration::from_millis(220));
                Msg::Scheduling(Action::ClearFlash(id))
            });
        }
    }
    fn routes(&self) -> Vec<&Route> {
        let q = self.query.to_lowercase();
        self.snapshot
            .routes
            .iter()
            .filter(|r| {
                q.is_empty()
                    || format!("{} {} {}", r.source, r.model, r.effort)
                        .to_lowercase()
                        .contains(&q)
            })
            .collect()
    }
    /// Model-palette rows per page: the strip draws 42-DIP rows under a fixed
    /// 42-DIP header, so the page grows with the canvas height instead of
    /// stranding the lower half of a tall window. Floor 10 keeps the shared
    /// page index inside the sources table's (10-row) range; the viewport
    /// reports 0 until the first canvas frame, which also lands on 10.
    /// Resolves a picked MenuFlyout label to the action it stands for. Labels
    /// that depend on state (toggles) are matched in both spellings, so a pick
    /// that lands just after a re-render still resolves.
    fn menu_action(&self, label: &str) -> Option<Action> {
        Some(match label {
            M_PLAN => Action::Plan,
            M_PARAMS => Action::ShowParams,
            M_LOAD => Action::Load,
            M_RESCAN => Action::Refresh,
            M_START => Action::Start,
            M_RELOAD => Action::Reload,
            M_MASTER => Action::AddMaster,
            M_FIT => Action::Fit,
            M_ZOOM_IN => Action::Zoom(0.1),
            M_ZOOM_OUT => Action::Zoom(-0.1),
            M_ZOOM_RESET => Action::Zoom(1.0 - self.zoom),
            M_PAN_ON | M_PAN_OFF => Action::TogglePan,
            M_MODE_CONNECT | M_MODE_MOVE => Action::ToggleConnectMode,
            M_RUN_SHOW | M_RUN_HIDE => Action::ToggleRun,
            _ => Action::AddTemplate(self.templates.iter().position(|t| t.title == label)?),
        })
    }
    /// Whether a drag may move the view. Connect mode locks it — left, right
    /// and middle drags alike — so a connection that misses its target never
    /// turns into a pan. Fit and zoom (View menu) still adjust the view.
    fn can_pan(&self) -> bool {
        !self.connect_mode
    }
    fn toggle_connect_mode(&mut self) {
        self.connect_mode = !self.connect_mode;
        // The pan tool would swallow every press and hide the new mode.
        self.pan_tool = false;
        self.drag.cancel();
    }
    /// Records left-presses on empty canvas and reports the second one of a
    /// double-click. Any other press (node, palette, right/middle button)
    /// resets the sequence, so "click node, click blank" never toggles.
    fn blank_double_click(&mut self, p: PointerEventInfo) -> bool {
        let (x, y) = self.point(p);
        let blank = !p.is_right_button_pressed
            && !p.is_middle_button_pressed
            && p.x >= 210.0
            && self.workflow().and_then(|w| w.hit(x, y)).is_none();
        if !blank {
            self.last_blank_press = None;
            return false;
        }
        let now = std::time::Instant::now();
        if is_double_click(self.last_blank_press, now, (p.x, p.y)) {
            self.last_blank_press = None;
            true
        } else {
            self.last_blank_press = Some((now, (p.x, p.y)));
            false
        }
    }
    fn palette_page_size(&self) -> usize {
        let rows = (self.viewport.get().1 - 50.0) / 42.0;
        (rows.floor().max(0.0) as usize).clamp(10, 60)
    }
    pub fn is_workspace(&self) -> bool {
        self.tab == "workflow"
    }
    fn workflow(&self) -> Option<&Workflow> {
        self.draft.workflows.get(self.selected)
    }
    fn workflow_mut(&mut self) -> Option<&mut Workflow> {
        self.draft.workflows.get_mut(self.selected)
    }
    fn add_pool(&mut self, route_id: String) {
        let mins = self.ttl.parse::<u64>().unwrap_or(120).clamp(1, 10080);
        let expiry = model::now_ms() + mins * 60_000;
        if let Some(row) = self.draft.pool.iter_mut().find(|r| r.route_id == route_id) {
            row.expires_at = expiry;
        } else if self.draft.pool.len() < 128 {
            self.draft.pool.push(model::PoolEntry {
                route_id,
                expires_at: expiry,
                tags: Vec::new(),
            });
        }
        self.dirty = true;
    }
    fn point(&self, p: PointerEventInfo) -> (f64, f64) {
        (
            (p.x - 210.0 - self.pan.0) / self.zoom,
            (p.y - self.pan.1) / self.zoom,
        )
    }
    pub fn update(&mut self, action: Action, ctx: &ComponentContext<Shell>) {
        let sync_editor = matches!(
            &action,
            Action::SelectNode(_)
                | Action::Press(_)
                | Action::Release(_)
                | Action::Param(_, _)
                | Action::AddTemplate(_)
                | Action::ApplyTemplate(_)
        );
        if (self.saving && !matches!(&action, Action::Received(_) | Action::ClearFlash(_)))
            || (self.busy
                && matches!(
                    &action,
                    Action::Load
                        | Action::Refresh
                        | Action::Start
                        | Action::Save
                        | Action::Reload
                        | Action::Check(_)
                        | Action::Run
                        | Action::Poll(_)
                        | Action::Detail(_)
                        | Action::Cancel
                ))
        {
            return;
        }
        match action {
            Action::Load => self.request("router_scheduling", json!({}), ctx),
            Action::Refresh => self.request("router_scheduling", json!({"action":"refresh"}), ctx),
            Action::Start => self.request("router_start", json!({}), ctx),
            Action::Check(id) => self.request(
                "router_scheduling",
                json!({"action":"check","route_id":id,"workflow_id":self.workflow_id,"luna_supervision":self.workflow().map_or(self.probe_supervision,|w|w.luna_supervision)}),
                ctx,
            ),
            Action::StopDrag => self.drag.cancel(),
            Action::CaptureLost => self.drag.lost(),
            Action::Menu(next) => {self.open_menu=None;self.update(*next,ctx);},
            Action::MenuPick(label) => {if let Some(next)=self.menu_action(&label){self.update(next,ctx);}},
            Action::TabNamed(label) => {if let Some((id,_))=TABS.iter().find(|(_,l)|*l==label){self.open_menu=None;self.tab=id;}},
            Action::CloseMenu => self.open_menu=None,
            Action::EditNode => {self.show_node_details=true;self.show_params=false;},
            Action::ClearSelection => {self.node=None;self.show_node_details=false;self.drag.cancel();},
            Action::ClearFlash(id) => {if self.flash.as_ref().is_some_and(|f|f.1==id) {self.flash=None;}},
            Action::PickTemplate(v) => {if let Some(i)=v {self.template_index=i.min(self.templates.len().saturating_sub(1));}},
            Action::ShowParams => {self.tab="workflow";self.show_params=true;self.show_node_details=false;},
            Action::ToggleRun => {self.tab="workflow";self.show_run = !self.show_run;},
            // The pan tool belongs to move mode: turning it on leaves connect
            // mode instead of silently re-enabling drags that move the view.
            Action::TogglePan => { self.pan_tool = !self.pan_tool; if self.pan_tool { self.connect_mode = false; } self.drag.cancel(); },
            Action::ToggleConnectMode => self.toggle_connect_mode(),
            Action::Fit => { let (width,height)=self.viewport.get(); if let Some(w)=self.workflow() { let transform=w.fit_view((width-210.0).max(100.0),height);self.pan=transform.0;self.zoom=transform.1; } },
            Action::CloseInspector => {self.show_params=false;self.show_node_details=false;},
            Action::AddFallback(id) => {if let Some(i)=self.node && let Some(n)=self.workflow_mut().and_then(|w|w.nodes.get_mut(i)) && n.node_type=="model" && id!=n.route_id && !n.fallback_routes.contains(&id) && n.fallback_routes.len()<8 {n.fallback_routes.push(id);self.dirty=true;}}
            Action::ClearFallback => {if let Some(i)=self.node && let Some(n)=self.workflow_mut().and_then(|w|w.nodes.get_mut(i)){n.fallback_routes.clear();self.dirty=true;}}
            Action::Acknowledge(v) => self.acknowledge=v,
            Action::Collect(id) => self.request("router_collect",json!({"job_id":id,"max_chars":12000}),ctx),
            Action::Tab(tab) => {self.open_menu=None;self.tab=tab;},
            Action::Section(id, open) => { if open { self.expanded.insert(id); } else { self.expanded.remove(&id); } }
            Action::CallPreset(i) => { if let Some(p)=self.snapshot.call_presets.get(i){self.draft.call_profile=p["value"].clone();self.profile_editor=serde_json::to_string_pretty(&self.draft.call_profile).unwrap_or_default();self.dirty=true;} }
            Action::ProfileEditor(v) => self.profile_editor=v,
            Action::ApplyProfile => match serde_json::from_str::<Value>(&self.profile_editor){Ok(v) if v.is_object()=>{self.draft.call_profile=v;self.dirty=true;},_=>self.error=Some("调用配置需要合法的 JSON 对象".into())},
            Action::Plan => {self.show_run=true;self.tab="workflow";if let Some(w)=self.workflow(){self.request("router_workflow_plan",json!({"definition":w,"workflow_id":self.workflow_id}),ctx);}},
            Action::Reload => {
                self.dirty = false;self.node=None;self.show_node_details=false;self.drag.cancel();
                self.request("router_scheduling", json!({}), ctx);
            }
            Action::Save => {
                if self.draft.workflows.iter().any(|w| !w.nodes.is_empty() && w.order().is_err()) {
                    self.error = Some("工作流依赖无效，请修正后保存".into());
                    return;
                }
                self.saving = true;
                self.request("router_scheduling", json!({"action":"save","value":self.draft,"expected_revision":self.draft.revision}), ctx);
            }
            Action::Received(value) => {
                self.busy = false;
                match value {
                    Err(error) => {
                        self.error = Some(error);
                        self.saving = false;
                    }
                    Ok(value) if value.get("settings").is_some() => {
                        match serde_json::from_value::<Snapshot>(value) {
                            Err(_) => self.error = Some("调度响应格式不兼容，请更新协调器".into()),
                            Ok(snapshot) => {
                                if !self.dirty || self.saving {
                                    self.draft = snapshot.settings.clone();
                                    self.profile_editor=serde_json::to_string_pretty(&self.draft.call_profile).unwrap_or_default();
                                    self.dirty = false;
                                }
                                if self.workflow_id.is_empty() {
                                    self.workflow_id = snapshot
                                        .supervisors
                                        .first()
                                        .and_then(|s| s["workflow_id"].as_str())
                                        .unwrap_or("default")
                                        .into();
                                }
                                if let Some(check) = &snapshot.check {
                                    self.notice = check.to_string();
                                }
                                if self.run_id.is_empty()
                                    && let Some(id) = snapshot.active_run(&self.workflow_id)
                                {
                                    self.run_id = id.into();
                                    self.show_run = true;
                                    let id = self.run_id.clone();
                                    ctx.spawn_background(move |_| Msg::Scheduling(Action::Poll(id)));
                                }
                                self.snapshot = snapshot;
                                self.loaded = true;
                                self.saving = false;
                            }
                        }
                    }
                        Ok(value) => {
                            if value.get("results").is_some(){self.report_text=bounded_pretty(&value);self.report_detail=Some(value);self.inv.invalidate();return;}
                        if let Some(id) = value["run_id"].as_str() {
                            self.run_id = id.into();
                        }
                        self.notice = value["state"].as_str().unwrap_or("已返回").into();
                        let active =
                            matches!(value["state"].as_str(), Some("running" | "cancelling"));
                        self.detail_text = bounded_pretty(&value);
                        self.detail = Some(value);
                        if active && !self.run_id.is_empty() {
                            let id = self.run_id.clone();
                            ctx.spawn_background(move |_| {
                                std::thread::sleep(std::time::Duration::from_secs(1));
                                Msg::Scheduling(Action::Poll(id))
                            });
                        }
                    }
                }
                self.inv.invalidate();
            }
            Action::Enabled(v) => {
                if self.draft.enabled != v { self.draft.enabled = v; self.dirty = true; }
            }
            Action::Directed(v) => {
                let mode=if v { "directed" } else { "auto" };
                if self.draft.mode != mode {self.draft.mode=mode.into(); self.dirty=true;}
            }
            Action::Source(id, v) => {
                if self.draft.sources.get(&id).copied().unwrap_or(true)!=v { self.draft.sources.insert(id, v); self.dirty = true; }
            }
            Action::Query(v) => {
                self.query = v;
                self.palette = 0;
            }
            Action::Ttl(v) => self.ttl = v,
            Action::Pool(id) => self.add_pool(id),
            Action::RemovePool(id) => {
                self.draft.pool.retain(|r| r.route_id != id);
                self.dirty = true;
            }
            Action::MovePool(i, delta) => {
                let next = i.saturating_add_signed(delta as isize);
                if next < self.draft.pool.len() {
                    self.draft.pool.swap(i, next);
                    self.dirty = true;
                }
            }
            Action::New => {
                if self.draft.workflows.len() < 16 {
                    self.draft
                        .workflows
                        .push(Workflow::new(format!("workflow-{}", model::now_ms())));
                    self.selected = self.draft.workflows.len() - 1;
                    self.node = None;
                    self.dirty = true;
                }
            }
            Action::Select(i) => {
                self.selected = i;
                self.node = None;self.show_node_details=false;
            }
            Action::Name(v) => {
                if let Some(w) = self.workflow_mut()
                    && w.name != v { w.name = v; self.dirty = true; }
            }
            Action::Budget(v) => {
                if let (Some(w), Ok(b)) = (self.workflow_mut(), v.parse())
                    && w.token_budget != b { w.token_budget = b; self.dirty = true; }
            }
            Action::Parallel(v) => {
                if let (Some(w), Ok(p)) = (self.workflow_mut(), v.parse())
                    && w.max_parallel != p { w.max_parallel = p; self.dirty = true; }
            }
            Action::SelectNode(i) => self.node = Some(i),
            Action::Supervision(value) => { self.probe_supervision=value; if let Some(w)=self.workflow_mut()&& w.luna_supervision!=value {w.luna_supervision=value;self.dirty=true;} }
            Action::RemoveWorkflow => {if self.selected<self.draft.workflows.len(){self.draft.workflows.remove(self.selected);self.selected=0;self.node=None;self.dirty=true;}}
            Action::AddTemplate(i) => { if let Some(t)=self.templates.get(i).cloned(){
                let route=self.draft.pool.iter().find(|p|p.expires_at>model::now_ms()).and_then(|p|self.snapshot.routes.iter().find(|r|r.route_id==p.route_id)).cloned();
                if self.workflow().is_none() && self.draft.workflows.len()<16 {self.draft.workflows.push(Workflow::new(format!("workflow-{}",model::now_ms())));self.selected=self.draft.workflows.len()-1;}
                if let Some(w)=self.workflow_mut(){match w.add_template(&t,route.as_ref()){Ok(n)=>{self.node=Some(n);self.dirty=true;},Err(e)=>self.error=Some(e.to_string())}}
            }}
            Action::ApplyTemplate(i) => {if let (Some(t),Some(n))=(self.templates.get(i).cloned(),self.node)&& let Some(w)=self.workflow_mut(){match w.apply_template(n,&t){Ok(())=>self.dirty=true,Err(e)=>self.error=Some(e.to_string())}}}
            Action::Role(v) => {if let Some(i)=self.node&& let Some(n)=self.workflow_mut().and_then(|w|w.nodes.get_mut(i))&& n.role!=v {n.role=v;self.dirty=true;}}
            Action::TaskEditor(v) => self.task_editor=v,
            Action::ApplyTaskEditor => { match serde_json::from_str::<Value>(&self.task_editor){Ok(value) if value.is_object()=>{if let Some(i)=self.node&& let Some(n)=self.workflow_mut().and_then(|w|w.nodes.get_mut(i)){n.task=value;self.dirty=true;}},_=>self.error=Some("节点参数需要合法的 JSON 对象".into())} }
            Action::NodeTitle(v) => {
                if let Some(i) = self.node
                    && let Some(n) = self.workflow_mut().and_then(|w| w.nodes.get_mut(i))
                        && n.title!=v {n.title = v; self.dirty = true;}
            }
            Action::Tags(v) => {
                if let Some(i) = self.node
                    && let Some(n) = self.workflow_mut().and_then(|w| w.nodes.get_mut(i)) {
                        let tags:Vec<String> = v
                            .split([',', '，'])
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                            .map(str::to_owned)
                            .collect();
                        if n.tags!=tags {n.tags=tags;self.dirty = true;}
                    }
            }
            Action::Param(key, v) => {
                if let Some(i)=self.node && let Some(n)=self.workflow_mut().and_then(|w|w.nodes.get_mut(i))
                    && model::edit_parameter(&mut n.task,&key,&v) { self.dirty=true; }
            }
            Action::RemoveNode => {
                if let Some(i) = self.node.take()
                    && let Some(w) = self.workflow_mut() {
                        w.remove(i);
                        self.dirty = true;
                    }
            }
            Action::AddMaster => {
                // Drop-in entry point at the canvas center; the workflow is
                // created on demand, mirroring a first model drop.
                if self.workflow().is_none() {
                    self.draft.workflows.push(Workflow::new(format!(
                        "workflow-{}",
                        model::now_ms()
                    )));
                    self.selected = self.draft.workflows.len() - 1;
                }
                let (width, height) = self.viewport.get();
                let x = ((width - 210.0) / 2.0 - self.pan.0) / self.zoom;
                let y = (height / 2.0 - self.pan.1) / self.zoom;
                if let Some(w) = self.workflow_mut() {
                    match w.add_master(x, y) {
                        Ok(i) => {
                            self.node = Some(i);
                            self.dirty = true;
                            self.pulse(ctx);
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
            }
            Action::DisconnectNode => {
                if let Some(i) = self.node
                    && let Some(w) = self.workflow_mut() {
                        w.disconnect(i);
                        self.dirty = true;
                    }
            }
            Action::MoveNode(delta) => {
                if let Some(i) = self.node
                    && let Some(w) = self.workflow_mut() {
                        let next = i.saturating_add_signed(delta as isize);
                        if next < w.nodes.len() {
                            w.nodes.swap(i, next);
                            self.node = Some(next);
                            self.dirty = true;
                        }
                    }
            }
            Action::Chain => {
                if let Some(w) = self.workflow_mut() {
                    w.chain();
                    self.dirty = true;
                }
            }
            Action::Unlink => {
                if let Some(w) = self.workflow_mut() {
                    w.edges.clear();
                    self.dirty = true;
                }
            }
            Action::Palette(delta) => {
                let pages = self.routes().len().saturating_sub(1) / self.palette_page_size();
                self.palette = self
                    .palette
                    .saturating_add_signed(delta as isize)
                    .min(pages);
            }
            Action::Zoom(delta) => { let (width,height)=self.viewport.get(); let anchor=((width-210.0)/2.0,height/2.0);let world=((anchor.0-self.pan.0)/self.zoom,(anchor.1-self.pan.1)/self.zoom);self.zoom=(self.zoom+delta).clamp(0.1,1.8);self.pan=(anchor.0-world.0*self.zoom,anchor.1-world.1*self.zoom); },
            Action::Press(p) => {
                self.pointer=(p.x,p.y);self.error=None;
                if self.blank_double_click(p) {
                    self.toggle_connect_mode();
                } else if p.is_right_button_pressed && p.x >= 210.0 {
                    // Right-press on a node opens the floating quick menu; on
                    // empty canvas it pans — in move mode only, see `can_pan`.
                    let (x, y) = self.point(p);
                    if let Some(w) = self.workflow()
                        && let Some(i) = w.hit(x, y)
                    {
                        self.node = Some(i);
                        self.menu_at = (p.x, p.y);
                        self.open_menu = Some("node");
                    } else if self.can_pan() {
                        self.drag.begin(Drag::Pan(p.x - self.pan.0, p.y - self.pan.1));
                    }
                } else if p.is_middle_button_pressed || (self.pan_tool && p.x >= 210.0) {
                    if self.can_pan() {
                        self.drag.begin(Drag::Pan(p.x - self.pan.0, p.y - self.pan.1));
                    }
                } else if p.x < 200.0 && p.y >= 42.0 {
                    // Only rows the page actually draws start a drag: the old
                    // unclamped index grabbed the NEXT page's first model when
                    // the click landed in the empty strip below the rows.
                    let page_size = self.palette_page_size();
                    let row = ((p.y - 42.0) / 42.0) as usize;
                    let drawn = self
                        .routes()
                        .len()
                        .saturating_sub(self.palette * page_size)
                        .min(page_size);
                    if row < drawn {
                        self.drag.begin(Drag::Model(self.palette * page_size + row));
                    }
                } else {
                    let (x, y) = self.point(p);
                    if let Some(w) = self.workflow()
                        && let Some(i) = w.hit(x, y) {
                            let n = &w.nodes[i];
                            // The mode decides, not where on the node the press
                            // landed: no hit strip to miss or to graze.
                            let drag = if self.connect_mode {
                                Drag::Edge(i)
                            } else {
                                Drag::Node(i, x - n.x, y - n.y)
                            };
                            self.node = Some(i);
                            self.drag.begin(drag);
                        } else if p.x >= 210.0 && self.can_pan() { self.drag.begin(Drag::Pan(p.x-self.pan.0,p.y-self.pan.1)); }
                }
            }
            Action::Move(p) => {
                let (x, y) = self.point(p);
                if self.drag.current.is_none(){let hover=if p.x>=210.0 {self.workflow().and_then(|w|w.hit(x,y))} else {None};if hover==self.hover{return;}self.hover=hover;}
                self.pointer=(p.x,p.y);
                match self.drag.movement() {
                    Some(Drag::Node(i,dx,dy)) => self.reposition(i,x-dx,y-dy),
                    Some(Drag::Pan(dx, dy)) => self.pan = (p.x - dx, p.y - dy),
                    _ => {}
                }
            }
            Action::Release(p) => {
                self.pointer=(p.x,p.y);
                let (x, y) = self.point(p);
                match self.drag.release() {
                    Some(Drag::Node(i,dx,dy)) => self.reposition(i,x-dx,y-dy),
                    Some(Drag::Pan(dx,dy)) => self.pan=(p.x-dx,p.y-dy),
                    Some(Drag::Model(index)) => {
                        if p.x < 210.0 || p.y < 0.0 || p.y > self.viewport.get().1 || p.x > self.viewport.get().0 { return; }
                        if let Some(route) = self.routes().get(index).map(|r| (*r).clone()) {
                            // The flash acknowledges a model landing on the
                            // canvas — not selection (every click-release used
                            // to pulse through the shared tail below).
                            let mut landed = false;
                            if p.x >= 210.0 {
                                if self.workflow().is_none() {
                                    self.draft.workflows.push(Workflow::new(format!(
                                        "workflow-{}",
                                        model::now_ms()
                                    )));
                                    self.selected = self.draft.workflows.len() - 1;
                                }
                                let hit = self.workflow().and_then(|w| w.hit(x, y));
                                if let Some(w) = self.workflow_mut() {
                                    if let Some(i) = hit {
                                        if w.nodes[i].node_type!="model" { self.error=Some("本地节点不绑定模型；请拖到空白处创建模型节点，或先套用模型职责模板".into()); return; }
                                        w.nodes[i].route_id.clone_from(&route.route_id);
                                        self.node = Some(i);
                                        landed = true;
                                    } else {
                                        match w.add_route(&route, x, y) {
                                            Ok(i) => { self.node = Some(i); landed = true; }
                                            Err(e) => self.error = Some(e.to_string()),
                                        }
                                    }
                                }
                            }
                            self.add_pool(route.route_id);
                            if landed {
                                self.pulse(ctx);
                            }
                        }
                    }
                    Some(Drag::Edge(from)) => {
                        let to = self
                            .workflow()
                            .and_then(|w| w.hit(x, y))
                            .filter(|to| *to != from);
                        if let (Some(w), Some(to)) = (self.workflow_mut(), to) {
                            if let Err(e) = w.connect(from, to) {
                                self.error = Some(e.to_string());
                            } else {
                                self.dirty = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Action::WorkflowId(v) => self.workflow_id = v,
            Action::Input(v) => self.input = v,
            Action::Run => {
                self.show_run=true;
                if self.dirty {
                    self.error = Some("请先保存工作流和模型池".into());
                } else if let Some(w) = self.workflow().cloned() {
                    let signature=serde_json::to_string(&json!({"definition":w,"input":self.input,"workflow":self.workflow_id})).unwrap_or_default();
                    let finished=self.detail.as_ref().and_then(|d|d["state"].as_str()).is_some_and(|s|matches!(s,"succeeded"|"failed"|"cancelled"|"interrupted"));
                    if self.submit_key.is_empty() || signature!=self.submit_signature || finished {self.submit_key=format!("desktop-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());self.submit_signature=signature;}
                    self.request("router_workflow_submit",json!({"definition":w,"input_text":self.input,"workflow_id":self.workflow_id,"request_id":self.submit_key,"acknowledge_unknown_risk":self.acknowledge}),ctx);
                }
            }
            Action::Poll(id) | Action::Detail(id) => {
                self.run_id.clone_from(&id);
                self.request("router_workflow_status", json!({"run_id":id}), ctx);
            }
            Action::Cancel => {
                self.request("router_workflow_cancel", json!({"run_id":self.run_id}), ctx);
            }
        }
        if sync_editor {
            self.task_editor = self
                .node
                .and_then(|i| self.workflow().and_then(|w| w.nodes.get(i)))
                .and_then(|n| serde_json::to_string_pretty(&n.task).ok())
                .unwrap_or_default();
        }
        self.inv.invalidate();
    }
}

/// Windows' default double-click time and a slop of a few pointer pixels:
/// the second press must land soon and close to the first.
fn is_double_click(
    previous: Option<(std::time::Instant, (f64, f64))>,
    now: std::time::Instant,
    at: (f64, f64),
) -> bool {
    previous.is_some_and(|(when, (px, py))| {
        now.saturating_duration_since(when) <= std::time::Duration::from_millis(500)
            && (at.0 - px).abs() <= 8.0
            && (at.1 - py).abs() <= 8.0
    })
}

fn bounded_pretty(value: &Value) -> String {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    if text.chars().count() > 24000 {
        text.chars().take(24000).collect::<String>() + "\n[界面摘录；完整报告请按作业 ID 分页回收]"
    } else {
        text
    }
}
/// Compact glyph button for table action columns — the column header carries
/// the meaning, so the row repeats a small glyph instead of a sentence (and
/// instead of the 20 DIP SymbolIcon, which made the buttons read as slabs).
fn glyph_button(
    theme: &Theme,
    glyph: &str,
    tip: &str,
    action: impl Fn() -> Action + 'static,
    ctx: &ViewContext<Shell>,
) -> View {
    let b: View = Button::new()
        .min_width(28.0)
        .min_height(28.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .on_click(ctx.callback(move |_| Msg::Scheduling(action())))
        .content(
            TextBlock::new()
                .text(glyph)
                .font_size(theme.body_size + 1.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center),
        );
    if tip.trim().is_empty() {
        b
    } else {
        b.tooltip(tip)
    }
}

/// Compact text button for table footers and toolbars.
fn small_button(
    label: &str,
    tip: &str,
    action: impl Fn() -> Action + 'static,
    ctx: &ViewContext<Shell>,
) -> View {
    let b: View = Button::new()
        .min_height(32.0)
        .on_click(ctx.callback(move |_| Msg::Scheduling(action())))
        .content(label);
    if tip.trim().is_empty() {
        b
    } else {
        b.tooltip(tip)
    }
}

/// Vertical stack with keyed children (same as `pages::vstack`).
fn vstack(spacing: f64, children: Vec<View>) -> View {
    StackPanel::new()
        .orientation(Orientation::Vertical)
        .spacing(spacing)
        .keyed_children(crate::pages::keyed(children))
}

/// Puts `v` at its cell's right edge. Horizontal alignment alone does not move
/// a control whose desired width exceeds the column (it pins left and
/// overflows), so the slack goes into a star spacer column instead — the
/// control's Auto column always ends flush with the cell's right edge.
fn right_cell(col: i32, v: impl Into<View>) -> View {
    Border::new()
        .grid_column(col)
        .vertical_alignment(VerticalAlignment::Center)
        .content(
            Grid::new()
                .columns([GridLength::STAR, GridLength::Auto])
                .column_spacing(8.0)
                .children([
                    Border::new().grid_column(0).into(),
                    Border::new()
                        .grid_column(1)
                        .vertical_alignment(VerticalAlignment::Center)
                        .content(v.into()),
                ]),
        )
}

/// A table cell that centers on its row; `align` places the content inside the
/// cell (Stretch fills it).
fn tcell_at(col: i32, align: HorizontalAlignment, v: impl Into<View>) -> View {
    Border::new()
        .grid_column(col)
        .vertical_alignment(VerticalAlignment::Center)
        .content(Border::new().horizontal_alignment(align).content(v.into()))
}

/// A table cell that centers on its row and fills the column.
fn tcell(col: i32, v: impl Into<View>) -> View {
    tcell_at(col, HorizontalAlignment::Stretch, v)
}

/// Cell at (col, row) for grids with more than one row.
fn rcell(col: i32, row: i32, v: impl Into<View>) -> View {
    Border::new()
        .grid_column(col)
        .grid_row(row)
        .vertical_alignment(VerticalAlignment::Center)
        .content(v.into())
}

/// Header row of a card-wide table; `cols` must match the body rows and each
/// label carries the alignment of the column it titles (so a centered icon
/// column has a centered caption, a right-aligned switch column a right one).
fn table_head(
    theme: &Theme,
    cols: Vec<GridLength>,
    labels: Vec<(&str, HorizontalAlignment)>,
) -> View {
    let cells: Vec<View> = labels
        .into_iter()
        .enumerate()
        .map(|(i, (l, align))| {
            tcell_at(
                i as i32,
                align,
                TextBlock::new()
                    .text(l)
                    .font_size(theme.label_size)
                    .foreground(theme.subtle),
            )
        })
        .collect();
    w::ruled_row(
        theme,
        Grid::new()
            .columns(cols)
            .column_spacing(12.0)
            .keyed_children(crate::pages::keyed(cells)),
    )
}

/// One body row: cells on the same columns as `table_head`, hairline under.
fn table_row(theme: &Theme, cols: Vec<GridLength>, cells: Vec<View>) -> View {
    w::ruled_row(
        theme,
        Grid::new()
            .columns(cols)
            .column_spacing(12.0)
            .keyed_children(crate::pages::keyed(cells)),
    )
}

fn label_actions(label: String, actions: Vec<View>) -> View {
    Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(12.0)
        .children((
            Border::new()
                .vertical_alignment(VerticalAlignment::Center)
                .content(
                    TextBlock::new()
                        .text(label)
                        .text_wrapping(TextWrapping::Wrap),
                ),
            right_cell(1, row(actions)),
        ))
}

fn help_for(label: &str) -> &'static str {
    match label {
        "运行工作流" => "按已保存的节点和连线调用模型，会占用工作流预算；可先在更多中预检。",
        "保存修改" | "保存" | "保存并应用" => {
            "保存节点、模型池和调用配置；保存本身不会发出模型请求。"
        }
        "停止" => "请求停止当前工作流。已发生的调用仍保留真实用量和结果。",
        "预检" | "运行前预检" => "检查模型路线、能力门槛和预算，不生成模型内容。",
        "工作流设置" => "调整名称、预算、并发和可选 Luna；对后续运行生效。",
        "来源与模型池" | "模型池" => {
            "查看来源连通性，选择本轮模型和有效期，设置优先顺序。"
        }
        "调用配置" | "调用配置档" => {
            "选择预设或编辑兼容、输出与等待参数；节点显式值优先。"
        }
        "启动服务" => "启动本机已安装的协调器，不安装软件或更改登录状态。",
        "刷新状态" => "重新读取现有状态，不做真实生成检测。",
        "重新扫描来源" => "重新扫描原生模型目录；目录存在不代表已经验证可调用。",
        "重载设置" => "放弃未保存修改，重新读取已保存设置。",
        "适应全图" | "适应" => "缩放和平移视口，使全部节点进入可见区域。",
        "编辑节点" => "打开当前选中节点的职责、提示词、标签和高级参数。",
        "输入与结果" => "填写本次输入，查看运行状态和节点报告。",
        "添加节点" => "选择母节点或职责模板；也可以直接从左侧模型库拖入。",
        "套用" => "将所选职责模板应用到当前节点，可继续修改职责和提示词。",
        "清空备用路线" => "仅移除本节点的备用模型，不影响原模型池。",
        "顺序串联" => "按节点顺序重建依赖链，将独立分支改为串行执行。",
        "清空连线" => "移除依赖关系，节点会成为独立分支。",
        _ => "",
    }
}
fn secondary(text: impl Into<String>) -> View {
    TextBlock::new()
        .text(text)
        .font_size(12.0)
        .text_wrapping(TextWrapping::Wrap)
        .opacity(0.72)
        .into()
}
fn button(label: &str, action: impl Fn() -> Action + 'static, ctx: &ViewContext<Shell>) -> View {
    // Stretch: inside a grid cell or a vertical stack the button fills its
    // column, so neighbouring buttons share edges (horizontal rows ignore it).
    let b: View = Button::new()
        .min_height(36.0)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .on_click(ctx.callback(move |_| Msg::Scheduling(action())))
        .content(label);
    match help_for(label) {
        "" => b,
        tip => b.tooltip(tip),
    }
}
/// One row of the node context sheet: icon column + label, styled like a
/// WinUI MenuFlyoutItem (Subtle button, 36 DIP, left-aligned). The long
/// description moves into the tooltip/automation name instead of a second
/// line on every row.
fn command(
    icon: Symbol,
    label: &str,
    description: &str,
    action: impl Fn() -> Action + 'static,
    ctx: &ViewContext<Shell>,
) -> View {
    Button::new()
        .style(ButtonStyle::Subtle)
        .min_height(36.0)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .horizontal_content_alignment(HorizontalAlignment::Stretch)
        .vertical_content_alignment(VerticalAlignment::Center)
        .automation_name(format!("{label}。{description}"))
        .on_click(ctx.callback(move |_| Msg::Scheduling(Action::Menu(Box::new(action())))))
        .content(
            Grid::new()
                .columns([GridLength::Pixel(20.0), GridLength::STAR])
                .column_spacing(12.0)
                .children((
                    Border::new()
                        .vertical_alignment(VerticalAlignment::Center)
                        .content(SymbolIcon::new().symbol(icon)),
                    Border::new()
                        .grid_column(1)
                        .vertical_alignment(VerticalAlignment::Center)
                        .content(TextBlock::new().text(label)),
                )),
        )
        .tooltip(description)
}

/// Native WinUI DropDownButton + MenuFlyout. The system owns placement,
/// light-dismiss, keyboard navigation and the open/close animation — the old
/// in-page sheets reimplemented all of that and only ever opened at one spot.
/// `None` entries are separators.
fn drop_menu(label: &str, entries: Vec<Option<(&str, bool)>>, ctx: &ViewContext<Shell>) -> View {
    let items = entries.into_iter().enumerate().map(|(i, e)| match e {
        None => MenuItem::separator(format!("sep-{i}")),
        Some((l, true)) => MenuItem::item(l, l),
        Some((l, false)) => MenuItem::disabled(l, l),
    });
    DropDownButton::new()
        .min_height(36.0)
        .content(TextBlock::new().text(label))
        .menu(Menu::new(
            items,
            ctx.callback(|label: String| Msg::Scheduling(Action::MenuPick(label))),
        ))
}

fn more_menu(state: &State, ctx: &ViewContext<Shell>) -> View {
    let idle = !state.busy;
    drop_menu(
        "更多",
        vec![
            Some((M_PLAN, idle && state.workflow().is_some())),
            Some((M_PARAMS, true)),
            None,
            Some((M_LOAD, idle)),
            Some((M_RESCAN, idle)),
            Some((M_START, idle)),
            None,
            Some((M_RELOAD, idle && state.dirty)),
        ],
        ctx,
    )
}

fn add_menu(state: &State, ctx: &ViewContext<Shell>) -> View {
    let mut entries = vec![Some((M_MASTER, true)), None];
    entries.extend(
        state
            .templates
            .iter()
            .map(|t| Some((t.title.as_str(), true))),
    );
    drop_menu("＋ 添加节点", entries, ctx)
}

fn view_menu(state: &State, ctx: &ViewContext<Shell>) -> View {
    drop_menu(
        "视图",
        vec![
            Some((M_FIT, true)),
            Some((M_ZOOM_IN, state.zoom < 1.79)),
            Some((M_ZOOM_OUT, state.zoom > 0.11)),
            Some((M_ZOOM_RESET, (state.zoom - 1.0).abs() > 0.001)),
            None,
            Some((
                if state.connect_mode {
                    M_MODE_MOVE
                } else {
                    M_MODE_CONNECT
                },
                true,
            )),
            Some((if state.pan_tool { M_PAN_OFF } else { M_PAN_ON }, true)),
            Some((
                if state.show_run {
                    M_RUN_HIDE
                } else {
                    M_RUN_SHOW
                },
                true,
            )),
        ],
        ctx,
    )
}

/// "?" button whose native Flyout carries the canvas guide: short labelled
/// lines on a two-column grid instead of a paragraph squeezed into a sheet.
fn help_button(theme: &Theme) -> View {
    let lines = [
        (
            "拖入模型",
            "从左侧模型库拖到空白处创建节点；拖到模型节点上替换绑定。",
        ),
        (
            "两种模式",
            "双击画布空白处在移动模式与连线模式间切换，右上角显示当前模式。",
        ),
        ("移动模式", "拖动节点移动它；空白处拖动平移画布。"),
        (
            "连线模式",
            "从任意节点拖到下游节点建立依赖，箭头指向下游。画布锁定，任何拖动都不会平移；适应全图与缩放仍可用。",
        ),
        ("节点菜单", "右键节点：编辑、断开连线、删除。"),
        (
            "运行",
            "连线决定依赖，独立分支并行。预检不生成内容；运行按已保存配置占用预算。",
        ),
    ];
    let mut cells: Vec<View> = Vec::new();
    for (i, (k, v)) in lines.iter().enumerate() {
        cells.push(rcell(
            0,
            i as i32,
            TextBlock::new().text(*k).font_weight(FontWeight::SEMI_BOLD),
        ));
        cells.push(rcell(
            1,
            i as i32,
            TextBlock::new()
                .text(*v)
                .foreground(theme.subtle)
                .text_wrapping(TextWrapping::Wrap),
        ));
    }
    let guide = Border::new().width(340.0).content(
        StackPanel::new().spacing(10.0).children((
            TextBlock::new()
                .text("画布操作说明")
                .font_size(theme.h2_size)
                .font_weight(FontWeight::SEMI_BOLD),
            Grid::new()
                .columns([GridLength::Auto, GridLength::STAR])
                .rows(lines.iter().map(|_| GridLength::Auto))
                .column_spacing(12.0)
                .row_spacing(8.0)
                .keyed_children(crate::pages::keyed(cells)),
        )),
    );
    Button::new()
        .min_width(36.0)
        .min_height(36.0)
        .automation_name("画布操作说明")
        .content(SymbolIcon::new().symbol(Symbol::Help))
        .flyout_with(Flyout::rich(guide).placement(FlyoutPlacement::BottomEdgeAlignedRight))
}

/// Width of the node context sheet: menu-sized, not panel-sized.
const NODE_MENU_W: f64 = 240.0;

/// Right-click sheet for a canvas node. The canvas is a custom surface, so
/// this one stays an in-page layer; it is styled as a MenuFlyout (subtle
/// rows, icon column, hairline separator) to match the native menus.
fn node_menu(state: &State, theme: &Theme, ctx: &ViewContext<Shell>) -> View {
    let title = state
        .node
        .and_then(|i| state.workflow().and_then(|w| w.nodes.get(i)))
        .map_or_else(|| "节点操作".to_owned(), |n| n.title.clone());
    let items: Vec<View> = vec![
        Border::new()
            .padding(Thickness::new(11.0, 4.0, 8.0, 4.0))
            .content(
                TextBlock::new()
                    .text(title)
                    .font_size(theme.label_size)
                    .foreground(theme.subtle)
                    .text_trimming(TextTrimming::CharacterEllipsis),
            ),
        command(
            Symbol::Edit,
            "编辑节点",
            help_for("编辑节点"),
            || Action::EditNode,
            ctx,
        ),
        command(
            Symbol::DisconnectDrive,
            "断开连线",
            "移除与该节点相关的全部上下游连线，保留节点。",
            || Action::DisconnectNode,
            ctx,
        ),
        Border::new()
            .height(1.0)
            .margin(Thickness::xy(0.0, 4.0))
            .background(theme.divider)
            .into(),
        command(
            Symbol::Delete,
            "删除节点",
            "从工作流移除该节点及其全部连线。",
            || Action::RemoveNode,
            ctx,
        ),
        command(
            Symbol::ClearSelection,
            "取消选择",
            "清除当前选中，保留节点与连线。",
            || Action::ClearSelection,
            ctx,
        ),
    ];
    let base = theme
        .page_bg
        .unwrap_or(Brush::Theme(ThemeBrush::SolidBackground));
    Grid::new()
        .width(NODE_MENU_W)
        .exit_transition(ExitTransition::fade(std::time::Duration::from_millis(120)))
        .children([
            Border::new()
                .background(base)
                .corner_radius(CornerRadius::uniform(theme.radius))
                .into(),
            Border::new()
                .background(theme.card_bg)
                .border_brush(theme.card_border)
                .border_thickness(Thickness::uniform(1.0))
                .corner_radius(CornerRadius::uniform(theme.radius))
                .padding(Thickness::uniform(4.0))
                .content(
                    StackPanel::new()
                        .spacing(2.0)
                        .keyed_children(crate::pages::keyed(items)),
                ),
        ])
}
fn field_help(label: &str) -> &'static str {
    match label {
        "节点职责（可自由调整）" => "说明这个节点负责什么、交付什么；用于生成任务指令。",
        "节点提示词" => "给出具体任务与验收条件，上游报告会随连线提供。",
        "分类标签（逗号分隔）" => "用于识别与组织节点，不改变模型权限。",
        "工作流 ID" => "启用 Luna 时应与在线监工登记的工作流一致。",
        "本次输入材料" => "本次运行的共享材料，连同上游证据传给模型。",
        "总 Token 预算" | "节点 Token 预算" => {
            "限制后续分配；原生隐藏用量在返回后计量，可能出现超预算报告。"
        }
        "并发目标" => "同时就绪的目标数，实际仍受来源与全局容量约束。",
        "最大输出 Tokens" => "包括模型隐藏思考；最终上限还受配置档和模型容量约束。",
        "首次响应期待（秒）" | "首次活动等待（秒）" => {
            "等待真实活动的时间窗口，不把重复心跳当作进度。"
        }
        "硬截止（秒）" => "本次调用的最终时间边界。",
        "停滞窗口（秒）" => "开始输出后的空闲等待，真实活动会刷新窗口。",
        "输出模式 fixed/adaptive/native" => {
            "留空继承；fixed 固定、adaptive 有界调整、native 使用已知原生容量。"
        }
        "报告字符上限" => "报告正文容量，不是模型输出 Token 数。",
        "调用配置 JSON" => "先选择预设，再按需覆盖参数；应用到草稿后仍需保存。",
        "高级节点参数 JSON" => "用于精确设置调用参数；普通职责和提示词可在上方编辑。",
        _ => "",
    }
}
fn line(
    label: &str,
    value: String,
    action: impl Fn(String) -> Action + 'static,
    ctx: &ViewContext<Shell>,
) -> View {
    let mut children: Vec<View> = vec![
        TextBlock::new().text(label).into(),
        TextBox::new()
            .accepts_return(
                label.contains("提示词")
                    || label.contains("职责")
                    || label.contains("材料")
                    || label.contains("JSON"),
            )
            .min_height(if label.contains("JSON") { 140.0 } else { 32.0 })
            // Never a sliver: a TextBox with no width in a horizontal stack
            // collapsed to the control's minimum.
            .min_width(120.0)
            .text(value)
            .on_text_changed(ctx.callback(move |v| Msg::Scheduling(action(v))))
            .into(),
    ];
    // Only fields that have help render the line: a zero-height stand-in still
    // paid the stack's 4 DIP gap on every row without a note.
    let help = field_help(label);
    if !help.is_empty() {
        children.push(secondary(help));
    }
    StackPanel::new()
        .spacing(4.0)
        .keyed_children(crate::pages::keyed(children))
}
/// Collapsible inspector group. Deliberately not a native `Expander`: that
/// control measures its content at the content's own width and centres it
/// (the Quota page's rows ended up 147 DIP wide in the middle of a 1030 DIP
/// page), which would squeeze these fields the same way.
fn section(
    theme: &Theme,
    id: &'static str,
    title: &str,
    description: &str,
    items: Vec<View>,
    open: bool,
    ctx: &ViewContext<Shell>,
) -> View {
    let head = Button::new()
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .horizontal_content_alignment(HorizontalAlignment::Stretch)
        .on_click(ctx.callback(move |_| Msg::Scheduling(Action::Section(id, !open))))
        .content(
            Grid::new()
                .columns([GridLength::STAR, GridLength::Auto])
                .column_spacing(8.0)
                .children((
                    StackPanel::new()
                        .spacing(3.0)
                        .children((TextBlock::new().text(title), secondary(description))),
                    Border::new()
                        .grid_column(1)
                        .vertical_alignment(VerticalAlignment::Center)
                        .content(
                            TextBlock::new()
                                .text(if open { "▴" } else { "▾" })
                                .font_size(theme.label_size)
                                .foreground(theme.subtle)
                                .vertical_alignment(VerticalAlignment::Center),
                        ),
                )),
        );
    let mut rows: Vec<View> = vec![head];
    if open {
        rows.push(
            StackPanel::new()
                .spacing(10.0)
                .keyed_children(crate::pages::keyed(items)),
        );
    }
    StackPanel::new()
        .spacing(8.0)
        .keyed_children(crate::pages::keyed(rows))
}
fn row(children: Vec<View>) -> View {
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(8.0)
        .keyed_children(children.into_iter().enumerate().map(|(i, child)| {
            KeyedView::new(
                i as u64,
                Border::new()
                    .vertical_alignment(VerticalAlignment::Center)
                    .content(child),
            )
        }))
}

/// Sub-page switcher. A SelectorBar on every sub-page (it used to be hidden in
/// 更多 on the canvas and three ordinary buttons elsewhere, so it read as
/// actions rather than "where am I").
fn tab_bar(state: &State, ctx: &ViewContext<Shell>) -> View {
    SelectorBar::new()
        .on_selected_text_changed(
            ctx.callback(|t: Option<String>| {
                Msg::Scheduling(Action::TabNamed(t.unwrap_or_default()))
            }),
        )
        .collection_slot(
            SelectorBarSlot::Items,
            TABS.iter().map(|(id, label)| {
                KeyedView::new(
                    *id,
                    SelectorBarItem::new()
                        .text(*label)
                        .is_selected(state.tab == *id),
                )
            }),
        )
}

/// Top band shared by all three sub-pages: switcher on the left; sync state,
/// 保存修改 and 更多 on the right, at the same place whichever page is shown.
/// 保存修改 carries the accent only while there is something to save, so the
/// page never shows two primary buttons at once (运行 takes it otherwise).
fn page_band(state: &State, theme: &Theme, ctx: &ViewContext<Shell>) -> View {
    let (status, brush) = if state.busy {
        ("处理中…", theme.subtle)
    } else if state.dirty {
        ("● 有未保存修改", theme.warn)
    } else {
        ("已同步", theme.subtle)
    };
    Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(16.0)
        .children((
            Border::new()
                .vertical_alignment(VerticalAlignment::Center)
                .horizontal_alignment(HorizontalAlignment::Left)
                .content(tab_bar(state, ctx)),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(row(vec![
                    TextBlock::new()
                        .text(status)
                        .font_size(theme.label_size)
                        .foreground(brush)
                        .margin(Thickness::new(0.0, 0.0, 4.0, 0.0))
                        .into(),
                    Button::new()
                        .style(if state.dirty {
                            ButtonStyle::Accent
                        } else {
                            ButtonStyle::Default
                        })
                        .min_height(36.0)
                        .is_enabled(state.dirty && !state.busy)
                        .on_click(ctx.callback(|_| Msg::Scheduling(Action::Save)))
                        .content("保存修改")
                        .tooltip(help_for("保存修改")),
                    more_menu(state, ctx),
                ])),
        ))
}

/// Service errors as a WinUI InfoBar, directly under the band on every
/// sub-page (the canvas showed them at the bottom, the other pages in a card).
fn error_bar(state: &State) -> Option<View> {
    state.error.as_ref().map(|error| {
        InfoBar::new()
            .severity(InfoBarSeverity::Error)
            .is_open(true)
            .is_closable(false)
            .title("调度服务")
            .message(error.clone())
            .into()
    })
}

pub fn page(state: &State, theme: &Theme, ctx: &mut ViewContext<Shell>) -> Vec<View> {
    if state.tab == "workflow" {
        return vec![workspace_view(state, theme, ctx)];
    }
    let mut blocks = vec![
        page_band(state, theme, ctx),
        secondary(if state.tab == "profiles" {
            "选择预设并按需覆盖；保存后作用于后续分配。"
        } else {
            "先选可用来源和模型，再回到工作流画布编排。"
        }),
    ];
    blocks.extend(error_bar(state));
    if !state.notice.is_empty() {
        blocks.push(w::card(
            theme,
            TextBlock::new()
                .text(&state.notice)
                .is_text_selection_enabled(true)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        ));
    }
    if state.tab == "profiles" {
        return profiles_blocks(blocks, state, theme, ctx);
    }
    // Scheduling switches in one card, rows on a shared label/switch grid —
    // the old inline row put two switches at whatever x the label text ended.
    blocks.push(w::section(
        theme,
        Symbol::Setting,
        "调度开关",
        vec![w::card(
            theme,
            vstack(
                6.0,
                vec![
                    w::ruled_row(
                        theme,
                        label_actions(
                            "允许模型调度".into(),
                            vec![crate::pages::switch(
                                theme,
                                state.draft.enabled,
                                ctx.callback(|v| Msg::Scheduling(Action::Enabled(v))),
                            )],
                        ),
                    ),
                    w::ruled_row(
                        theme,
                        label_actions(
                            "定向模式 · 仅使用模型池".into(),
                            vec![crate::pages::switch(
                                theme,
                                state.draft.mode == "directed",
                                ctx.callback(|v| Msg::Scheduling(Action::Directed(v))),
                            )],
                        ),
                    ),
                    secondary(if state.dirty {
                        "有未保存的修改；保存后作用于新派发，已发出的请求允许完成。"
                    } else {
                        "自动模式按能力选路；定向模式按池内顺序选路，池到期后停止派发。"
                    }),
                ],
            ),
        )],
    ));
    if state.tab == "sources" {
        // ---- 来源 ----
        // One card, one table: the switch column is the same x on every row
        // (the old loose rows staggered the toggles with the label length).
        let mut sources = std::collections::BTreeSet::new();
        for s in &state.snapshot.sources {
            if let Some(id) = s["id"].as_str() {
                sources.insert(id.to_owned());
            }
        }
        for r in &state.snapshot.routes {
            sources.insert(r.source.clone());
        }
        let src_cols = vec![GridLength::STAR, GridLength::Pixel(110.0)];
        let mut src_rows: Vec<View> = vec![table_head(
            theme,
            src_cols.clone(),
            vec![
                ("来源 · 模型 · 近期生成验证", HorizontalAlignment::Stretch),
                ("允许入池", HorizontalAlignment::Right),
            ],
        )];
        for source in sources {
            let count = state
                .snapshot
                .routes
                .iter()
                .filter(|r| r.source == source)
                .count();
            let good = state
                .snapshot
                .routes
                .iter()
                .filter(|r| r.source == source && r.connectivity == "verified")
                .count();
            let key = source.clone();
            let on = state.draft.sources.get(&source).copied().unwrap_or(true);
            src_rows.push(table_row(
                theme,
                src_cols.clone(),
                vec![
                    tcell(
                        0,
                        TextBlock::new()
                            .text(format!("{source} · {count} 模型 · {good} 近期生成验证"))
                            .text_wrapping(TextWrapping::Wrap),
                    ),
                    right_cell(
                        1,
                        crate::pages::switch(
                            theme,
                            on,
                            ctx.callback(move |v| Msg::Scheduling(Action::Source(key.clone(), v))),
                        ),
                    ),
                ],
            ));
        }
        if src_rows.len() == 1 {
            src_rows.push(secondary("尚未发现来源。请检查协调器是否已启动。"));
        }
        blocks.push(w::section(
            theme,
            Symbol::World,
            "来源",
            vec![w::card(theme, vstack(0.0, src_rows))],
        ));

        // ---- 模型与来源 ----
        let route_cols = vec![
            GridLength::STAR,
            GridLength::Pixel(40.0),
            GridLength::Pixel(40.0),
        ];
        let mut route_rows: Vec<View> = vec![
            // Filter fields: labels on their own row so both boxes share one
            // baseline (side by side in a stack they butted together and each
            // shrank to its minimum width).
            Border::new()
                .padding(Thickness::new(0.0, 2.0, 0.0, 10.0))
                .content(
                    Grid::new()
                        // Wide enough for the range label to stay on one line:
                        // wrapped to two lines it pushed the two boxes out of
                        // baseline and read as broken text.
                        .columns([GridLength::STAR, GridLength::Pixel(200.0)])
                        .rows([GridLength::Auto, GridLength::Auto])
                        .column_spacing(16.0)
                        .row_spacing(6.0)
                        .children([
                            rcell(
                                0,
                                0,
                                TextBlock::new()
                                    .text("搜索模型 / 来源")
                                    .vertical_alignment(VerticalAlignment::Bottom),
                            ),
                            rcell(
                                1,
                                0,
                                TextBlock::new()
                                    .text("有效期（分钟，1–10080）")
                                    .vertical_alignment(VerticalAlignment::Bottom),
                            ),
                            rcell(
                                0,
                                1,
                                TextBox::new()
                                    .placeholder_text("按模型或来源筛选下方列表")
                                    .min_height(32.0)
                                    .min_width(240.0)
                                    .text(state.query.clone())
                                    .on_text_changed(
                                        ctx.callback(|v: String| Msg::Scheduling(Action::Query(v))),
                                    ),
                            ),
                            rcell(
                                1,
                                1,
                                TextBox::new()
                                    .min_height(32.0)
                                    .text(state.ttl.clone())
                                    .on_text_changed(
                                        ctx.callback(|v: String| Msg::Scheduling(Action::Ttl(v))),
                                    ),
                            ),
                        ]),
                ),
            table_head(
                theme,
                route_cols.clone(),
                vec![
                    (
                        "来源 / 模型 · 连通性 · 最近错误",
                        HorizontalAlignment::Stretch,
                    ),
                    ("入池", HorizontalAlignment::Center),
                    ("检测", HorizontalAlignment::Center),
                ],
            ),
        ];
        let routes = state.routes();
        for r in routes.iter().skip(state.palette * 10).take(10) {
            let id = r.route_id.clone();
            let probe_id = id.clone();
            let status = format!(
                "{} / {} · {} · 最近错误 {}",
                r.source,
                r.model,
                r.connectivity,
                r.last_error.as_deref().unwrap_or("—")
            );
            route_rows.push(table_row(
                theme,
                route_cols.clone(),
                vec![
                    tcell(
                        0,
                        TextBlock::new()
                            .text(status)
                            .text_wrapping(TextWrapping::Wrap),
                    ),
                    tcell(
                        1,
                        glyph_button(
                            theme,
                            "＋",
                            "入池 / 续期：加入限时模型池（已在池则续期）",
                            {
                                let id = id.clone();
                                move || Action::Pool(id.clone())
                            },
                            ctx,
                        ),
                    ),
                    tcell(
                        2,
                        glyph_button(
                            theme,
                            "↻",
                            "生成检测：会产生少量用量",
                            {
                                let probe_id = probe_id.clone();
                                move || Action::Check(probe_id.clone())
                            },
                            ctx,
                        ),
                    ),
                ],
            ));
        }
        if routes.is_empty() {
            route_rows.push(secondary("没有匹配的模型。"));
        }
        // Footer: the pager on one line, the policy note under it, both
        // padded away from the last row (they used to sit flush on it).
        route_rows.push(
            Border::new()
                .padding(Thickness::new(0.0, 8.0, 0.0, 0.0))
                .content(row(vec![
                    small_button(
                        "上一组模型",
                        "查看上一组 10 个模型",
                        || Action::Palette(-1),
                        ctx,
                    ),
                    TextBlock::new()
                        .text(format!(
                            "{} / {}",
                            state.palette + 1,
                            routes.len().saturating_sub(1) / 10 + 1
                        ))
                        .font_size(theme.label_size)
                        .foreground(theme.subtle)
                        .min_width(64.0)
                        .horizontal_alignment(HorizontalAlignment::Center)
                        .vertical_alignment(VerticalAlignment::Center)
                        .into(),
                    small_button(
                        "下一组模型",
                        help_for("下一组模型"),
                        || Action::Palette(1),
                        ctx,
                    ),
                ])),
        );
        route_rows.push(
            Border::new()
                .padding(Thickness::new(0.0, 6.0, 0.0, 4.0))
                .content(secondary(
                    "扫描只读取模型目录；生成检测会产生少量用量，监工可按工作流选择，遵守共享探针预算。列表可见不代表已验证可用。",
                )),
        );
        blocks.push(w::section(
            theme,
            Symbol::Filter,
            "模型与来源",
            vec![w::card(theme, vstack(0.0, route_rows))],
        ));

        // ---- 限时模型池 ----
        let pool_cols = vec![
            GridLength::STAR,
            GridLength::Pixel(40.0),
            GridLength::Pixel(40.0),
            GridLength::Pixel(40.0),
        ];
        let mut pool_rows: Vec<View> = vec![table_head(
            theme,
            pool_cols.clone(),
            vec![
                ("顺序 · 模型 · 剩余", HorizontalAlignment::Stretch),
                ("上移", HorizontalAlignment::Center),
                ("下移", HorizontalAlignment::Center),
                ("移出", HorizontalAlignment::Center),
            ],
        )];
        for (i, p) in state.draft.pool.iter().enumerate() {
            let name = state
                .snapshot
                .routes
                .iter()
                .find(|r| r.route_id == p.route_id)
                .map_or(p.route_id.as_str(), |r| r.model.as_str());
            let left = p.expires_at.saturating_sub(model::now_ms()) / 60000;
            let id = p.route_id.clone();
            pool_rows.push(table_row(
                theme,
                pool_cols.clone(),
                vec![
                    tcell(
                        0,
                        TextBlock::new()
                            .text(format!(
                                "{} · {name} · {}",
                                i + 1,
                                if left == 0 {
                                    "已到期".into()
                                } else {
                                    format!("剩余 {left} 分钟")
                                }
                            ))
                            .text_wrapping(TextWrapping::Wrap),
                    ),
                    tcell(
                        1,
                        glyph_button(theme, "↑", "上移", move || Action::MovePool(i, -1), ctx),
                    ),
                    tcell(
                        2,
                        glyph_button(theme, "↓", "下移", move || Action::MovePool(i, 1), ctx),
                    ),
                    tcell(
                        3,
                        glyph_button(
                            theme,
                            "✕",
                            "移出模型池",
                            {
                                let id = id.clone();
                                move || Action::RemovePool(id.clone())
                            },
                            ctx,
                        ),
                    ),
                ],
            ));
        }
        if state.draft.pool.is_empty() {
            pool_rows.push(secondary("尚未加入任何模型。"));
        }
        blocks.push(w::section(
            theme,
            Symbol::Clock,
            "限时模型池",
            vec![w::card(theme, vstack(0.0, pool_rows))],
        ));
    }
    blocks
}

/// Equal-width cells, `cols` per row: every button in the grid shares one
/// width and one left edge (rows of natural-width buttons staggered with the
/// label lengths).
fn uniform_grid(cols: usize, spacing: f64, views: Vec<View>) -> View {
    let rows = views.len().div_ceil(cols.max(1));
    Grid::new()
        .columns((0..cols).map(|_| GridLength::STAR))
        .rows((0..rows).map(|_| GridLength::Auto))
        .column_spacing(spacing)
        .row_spacing(spacing)
        .keyed_children(views.into_iter().enumerate().map(|(i, v)| {
            KeyedView::new(
                i as u64,
                Border::new()
                    .grid_column((i % cols) as i32)
                    .grid_row((i / cols) as i32)
                    .content(v),
            )
        }))
}

fn profiles_blocks(
    mut blocks: Vec<View>,
    state: &State,
    theme: &Theme,
    ctx: &ViewContext<Shell>,
) -> Vec<View> {
    let presets: Vec<View> = state
        .snapshot
        .call_presets
        .iter()
        .enumerate()
        .map(|(index, p)| {
            Button::new()
                .min_height(36.0)
                .horizontal_alignment(HorizontalAlignment::Stretch)
                .on_click(ctx.callback(move |_| Msg::Scheduling(Action::CallPreset(index))))
                .content(p["title"].as_str().unwrap_or("预设"))
        })
        .collect();
    let mut preset_card: Vec<View> = vec![secondary(
        "节点显式参数优先于预设默认值；当前配置上限与模型原生上限共同生效。保存仅修改本协调器设置。",
    )];
    if presets.is_empty() {
        preset_card.push(w::empty_state(theme, "协调器未提供调用预设。"));
    } else {
        preset_card.push(uniform_grid(3, 8.0, presets));
    }
    blocks.push(w::section(
        theme,
        Symbol::List,
        "预设",
        vec![w::card(theme, vstack(12.0, preset_card))],
    ));
    blocks.push(w::section(
        theme,
        Symbol::Edit,
        "调用配置 JSON",
        vec![w::card(
            theme,
            vstack(
                10.0,
                vec![
                    line(
                        "调用配置 JSON",
                        state.profile_editor.clone(),
                        Action::ProfileEditor,
                        ctx,
                    ),
                    Border::new()
                        .horizontal_alignment(HorizontalAlignment::Right)
                        .content(button("应用到待保存设置", || Action::ApplyProfile, ctx)),
                ],
            ),
        )],
    ));
    blocks.push(w::section(
        theme,
        Symbol::Document,
        "当前生效值",
        vec![w::card(
            theme,
            TextBlock::new()
                .text(
                    serde_json::to_string_pretty(&state.snapshot.effective_profile)
                        .unwrap_or_default(),
                )
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .is_text_selection_enabled(true)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        )],
    ));
    blocks
}

/// Icon + label button content (Run / Stop): the glyph is a SymbolIcon on the
/// label's center line instead of a "▶" character riding on the text baseline.
fn icon_label(icon: Symbol, label: &str) -> View {
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(8.0)
        .children((
            // 16 DIP like WinUI's AppBarButton glyphs; the bare SymbolIcon is
            // 20 and outweighed the label next to it.
            Viewbox::new()
                .width(16.0)
                .height(16.0)
                .vertical_alignment(VerticalAlignment::Center)
                .slot(ViewboxSlot::Child, SymbolIcon::new().symbol(icon)),
            TextBlock::new()
                .text(label)
                .vertical_alignment(VerticalAlignment::Center),
        ))
}

fn workspace_view(state: &State, theme: &Theme, ctx: &mut ViewContext<Shell>) -> View {
    let reason = state.run_disabled_reason();
    // ---- 工作流工具栏：编辑在左，执行在右 ----
    // 运行 is the page's primary action unless there are unsaved edits — then
    // 保存修改 (in the band above) holds the accent and 运行 is disabled anyway.
    let mut run_actions = Vec::new();
    if state.active_run() {
        run_actions.push(
            Button::new()
                .min_height(36.0)
                .is_enabled(!state.busy)
                .on_click(ctx.callback(|_| Msg::Scheduling(Action::Cancel)))
                .content(icon_label(Symbol::Stop, "停止"))
                .tooltip(help_for("停止")),
        );
    }
    run_actions.push(
        Button::new()
            .style(if state.dirty {
                ButtonStyle::Default
            } else {
                ButtonStyle::Accent
            })
            .min_height(36.0)
            .is_enabled(reason.is_none())
            .on_click(ctx.callback(|_| Msg::Scheduling(Action::Run)))
            .content(icon_label(Symbol::Play, "运行工作流"))
            .tooltip(reason.unwrap_or(help_for("运行工作流"))),
    );
    let flow_bar = Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(16.0)
        .children((
            Border::new()
                .vertical_alignment(VerticalAlignment::Center)
                .content(row(vec![
                    ComboBox::new()
                        .placeholder_text("选择或添加工作流")
                        .items_source(state.draft.workflows.iter().map(|w| w.name.as_str()))
                        .selected_index(
                            (!state.draft.workflows.is_empty()).then_some(state.selected),
                        )
                        .min_height(36.0)
                        .width(240.0)
                        .on_selection_changed(ctx.callback(|v: Option<usize>| {
                            Msg::Scheduling(v.map_or(Action::StopDrag, Action::Select))
                        }))
                        .into(),
                    add_menu(state, ctx),
                    view_menu(state, ctx),
                    help_button(theme),
                ])),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(row(run_actions)),
        ));
    // ---- 模型库：搜索与翻页在左，画布概况在右 ----
    let pages = state.routes().len().saturating_sub(1) / state.palette_page_size() + 1;
    let library_bar = Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(16.0)
        .children((
            Border::new()
                .vertical_alignment(VerticalAlignment::Center)
                .content(row(vec![
                    TextBox::new()
                        .placeholder_text("搜索模型库（模型或来源）")
                        .text(&state.query)
                        .width(240.0)
                        .min_height(32.0)
                        .on_text_changed(ctx.callback(|v| Msg::Scheduling(Action::Query(v))))
                        .into(),
                    glyph_button(theme, "‹", "上一页模型", || Action::Palette(-1), ctx),
                    TextBlock::new()
                        .text(format!("{} / {pages}", state.palette + 1))
                        .font_size(theme.label_size)
                        .foreground(theme.subtle)
                        .min_width(44.0)
                        .horizontal_alignment(HorizontalAlignment::Center)
                        .into(),
                    glyph_button(theme, "›", "下一页模型", || Action::Palette(1), ctx),
                ])),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(
                    TextBlock::new()
                        .text(format!(
                            "{} 个节点 · 缩放 {:.0}%{}",
                            state.workflow().map_or(0, |w| w.nodes.len()),
                            state.zoom * 100.0,
                            match (state.connect_mode, state.pan_tool) {
                                (_, true) => " · 平移工具",
                                (true, false) => " · 连线模式",
                                (false, false) => " · 移动模式",
                            }
                        ))
                        .font_size(theme.label_size)
                        .foreground(theme.subtle),
                ),
        ));
    let side = state.show_node_details || state.show_params;
    let body = Grid::new()
        .columns([
            GridLength::STAR,
            GridLength::Pixel(if side { 320.0 } else { 0.0 }),
        ])
        .column_spacing(if side { 10.0 } else { 0.0 })
        .children((
            Border::new().content(canvas(state, theme, ctx)),
            Border::new().grid_column(1).content(if side {
                inspector(state, theme, ctx)
            } else {
                Border::new().into()
            }),
        ));
    // ---- 底部状态：选中节点的上下文，或运行不可用的原因 ----
    let selected = state
        .node
        .and_then(|i| state.workflow().and_then(|w| w.nodes.get(i)));
    let status: View = if let Some(node) = selected {
        Grid::new()
            .columns([GridLength::STAR, GridLength::Auto])
            .column_spacing(16.0)
            .min_height(40.0)
            .children((
                Border::new()
                    .vertical_alignment(VerticalAlignment::Center)
                    .content(
                    StackPanel::new().spacing(2.0).children((
                        TextBlock::new()
                            .text(format!("已选中：{}", node.title))
                            .font_weight(FontWeight::SEMI_BOLD)
                            .foreground(theme.accent)
                            .text_trimming(TextTrimming::CharacterEllipsis),
                        secondary(if state.connect_mode {
                            "连线模式：从节点拖到下游连线 · 双击空白处切回移动 · 右键打开节点菜单"
                        } else {
                            "移动模式：拖动节点移动 · 双击空白处切到连线 · 右键打开节点菜单"
                        }),
                    )),
                ),
                Border::new()
                    .grid_column(1)
                    .vertical_alignment(VerticalAlignment::Center)
                    .content(row(vec![
                        button("编辑节点", || Action::EditNode, ctx),
                        button("取消选择", || Action::ClearSelection, ctx),
                    ])),
            ))
    } else if let Some(reason) = reason {
        InfoBar::new()
            .severity(InfoBarSeverity::Informational)
            .is_open(true)
            .is_closable(false)
            .title("暂不能运行")
            .message(reason)
            .into()
    } else {
        secondary("就绪：可以运行工作流。运行前可在 更多 › 运行前预检 中检查路线与预算。")
    };
    // Bands, not one five-row grid: the toolbar keeps its natural height and
    // the node sheet lives in the pane below it, anchored by layout.
    let mut top = vec![page_band(state, theme, ctx)];
    top.extend(error_bar(state));
    top.push(flow_bar);
    top.push(library_bar);
    let toolbar = vstack(10.0, top);
    // Only the rows that have content: an empty Auto row still paid the grid's
    // row spacing. With the run pane open and nothing selected the status row
    // steps aside (the pane repeats the reason) so the canvas keeps its height.
    let mut rows: Vec<(&str, View)> = vec![("body", body)];
    if !state.show_run || selected.is_some() {
        rows.push(("status", status));
    }
    if state.show_run {
        rows.push(("run", run_panel(state, theme, ctx)));
    }
    let pane =
        Grid::new()
            .rows((0..rows.len()).map(|i| {
                if i == 0 {
                    GridLength::STAR
                } else {
                    GridLength::Auto
                }
            }))
            .row_spacing(10.0)
            .keyed_children(rows.into_iter().enumerate().map(|(i, (key, v))| {
                KeyedView::new(key, Border::new().grid_row(i as i32).content(v))
            }));
    let mut layers = vec![
        KeyedView::new("toolbar", toolbar),
        KeyedView::new("pane", Border::new().grid_row(1).content(pane)),
    ];
    if state.open_menu == Some("node") {
        layers.push(KeyedView::new(
            "dismiss",
            Border::new()
                .grid_row(1)
                .background(Brush::Solid(Color::argb(0, 0, 0, 0)))
                .on_pointer_pressed(ctx.callback(|_| Msg::Scheduling(Action::CloseMenu))),
        ));
        // Anchored where the node was right-pressed. Pointer coordinates are
        // canvas-local and the canvas is the pane cell's first row, so the
        // same (x, y) positions the layer inside that cell; clamp so a node
        // near an edge keeps the sheet on screen.
        let (px, py) = state.menu_at;
        let (vw, vh) = state.viewport.get();
        let left = px.clamp(0.0, (vw - NODE_MENU_W - 12.0).max(0.0));
        let top = py.clamp(0.0, (vh - 220.0).max(0.0));
        layers.push(KeyedView::new(
            "menu",
            Border::new()
                .grid_row(1)
                .horizontal_alignment(HorizontalAlignment::Left)
                .vertical_alignment(VerticalAlignment::Top)
                .margin(Thickness::new(left, top, 0.0, 0.0))
                .content(node_menu(state, theme, ctx)),
        ));
    }
    Grid::new()
        .rows([GridLength::Auto, GridLength::STAR])
        .row_spacing(10.0)
        .keyed_children(layers)
}

/// Title row of a side/bottom pane: title (+ optional caption) left, a
/// "收起" button pinned right — the inspector and the run pane share it, so
/// both close from the same corner with the same word.
fn pane_head(
    theme: &Theme,
    title: &str,
    caption: &str,
    close: impl Fn() -> Action + 'static,
    ctx: &ViewContext<Shell>,
) -> View {
    // Title and 收起 share one centered row; the caption runs full width under
    // it (squeezed beside the button it wrapped after a few characters).
    let head = Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(12.0)
        .children((
            Border::new()
                .vertical_alignment(VerticalAlignment::Center)
                .content(
                    TextBlock::new()
                        .text(title)
                        .font_size(theme.h2_size)
                        .font_weight(FontWeight::SEMI_BOLD)
                        .text_wrapping(TextWrapping::Wrap),
                ),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(
                    Button::new()
                        .style(ButtonStyle::Subtle)
                        .min_height(32.0)
                        .on_click(ctx.callback(move |_| Msg::Scheduling(close())))
                        .content(icon_label(Symbol::Cancel, "收起")),
                ),
        ));
    if caption.is_empty() {
        head
    } else {
        vstack(4.0, vec![head, secondary(caption)])
    }
}

fn run_panel(state: &State, theme: &Theme, ctx: &ViewContext<Shell>) -> View {
    // 预检和运行都会打开这张面板；标题行常驻"收起"，与检查器同一位置、同一个词。
    // 面板打开时底部状态行让位，"暂不能运行"的原因改在标题下说明。
    let caption = state
        .run_disabled_reason()
        .filter(|_| state.node.is_none())
        .map_or_else(String::new, |r| format!("暂不能运行：{r}"));
    let head = pane_head(
        theme,
        if state.active_run() {
            "输入与结果 · 运行中"
        } else {
            "输入与结果"
        },
        &caption,
        || Action::ToggleRun,
        ctx,
    );
    let mut blocks = vec![
        Grid::new()
            .columns([GridLength::Pixel(220.0), GridLength::STAR])
            .column_spacing(16.0)
            .children((
                Border::new()
                    .vertical_alignment(VerticalAlignment::Top)
                    .content(line(
                        "工作流 ID",
                        state.workflow_id.clone(),
                        Action::WorkflowId,
                        ctx,
                    )),
                Border::new()
                    .grid_column(1)
                    .vertical_alignment(VerticalAlignment::Top)
                    .content(line(
                        "本次输入材料",
                        state.input.clone(),
                        Action::Input,
                        ctx,
                    )),
            )),
        w::ruled_row(
            theme,
            label_actions(
                "新建未知结果的运行前，接受重复付费风险".into(),
                vec![crate::pages::switch(
                    theme,
                    state.acknowledge,
                    ctx.callback(|v| Msg::Scheduling(Action::Acknowledge(v))),
                )],
            ),
        ),
    ];
    let runs: Vec<View> = state
        .snapshot
        .runs
        .iter()
        .filter_map(|run| {
            let id = run["id"].as_str()?.to_owned();
            Some(button(
                &format!(
                    "{} · {}",
                    run["name"].as_str().unwrap_or("运行"),
                    run["state"].as_str().unwrap_or("未知")
                ),
                move || Action::Detail(id.clone()),
                ctx,
            ))
        })
        .collect();
    if !runs.is_empty() {
        blocks.push(secondary("最近运行 · 点击查看明细"));
        blocks.push(uniform_grid(3, 8.0, runs));
    }
    if let Some(detail) = &state.detail {
        if let Some(nodes) = detail["nodes"].as_object() {
            let reports: Vec<View> = nodes
                .iter()
                .filter_map(|(id, node)| {
                    let job = node["job_id"].as_str()?.to_owned();
                    Some(button(
                        &format!(
                            "节点 {id} · {} · 查看报告",
                            node["state"].as_str().unwrap_or("未知")
                        ),
                        move || Action::Collect(job.clone()),
                        ctx,
                    ))
                })
                .collect();
            if !reports.is_empty() {
                blocks.push(secondary("节点报告"));
                blocks.push(uniform_grid(2, 8.0, reports));
            }
        }
        blocks.push(
            TextBlock::new()
                .text(state.detail_text.clone())
                .font_size(theme.label_size)
                .is_text_selection_enabled(true)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
    }
    if state.report_detail.is_some() {
        blocks.push(
            TextBlock::new()
                .text(state.report_text.clone())
                .font_size(theme.label_size)
                .is_text_selection_enabled(true)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
    }
    // Vertical stack, not a Grid: grid children without an explicit row all
    // land in row 0, which stacked the header under the scroll content and
    // made clicks on 收起 land in the wide input field instead.
    w::card(
        theme,
        StackPanel::new().spacing(8.0).children((
            head,
            ScrollViewer::new()
                .height(170.0)
                .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
                .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                .content(
                    StackPanel::new()
                        .margin(Thickness::new(0.0, 0.0, 12.0, 0.0))
                        .spacing(10.0)
                        .keyed_children(crate::pages::keyed(blocks)),
                ),
        )),
    )
}

fn inspector(state: &State, theme: &Theme, ctx: &ViewContext<Shell>) -> View {
    let mut blocks = vec![pane_head(
        theme,
        if state.show_params {
            "工作流设置"
        } else {
            "节点设置"
        },
        if state.show_params {
            "配置本工作流的执行范围和保护条件。"
        } else {
            "修改当前选中节点的职责与交付要求。"
        },
        || Action::CloseInspector,
        ctx,
    )];
    if state.show_params {
        if let Some(w) = state.workflow() {
            blocks.push(line("工作流名称", w.name.clone(), Action::Name, ctx));
            // Two numbers side by side: they are short, and one column of
            // full-width boxes read as a form twice its real length.
            blocks.push(
                Grid::new()
                    .columns([GridLength::STAR, GridLength::STAR])
                    .column_spacing(10.0)
                    .children((
                        Border::new()
                            .vertical_alignment(VerticalAlignment::Top)
                            .content(line(
                                "总 Token 预算",
                                w.token_budget.to_string(),
                                Action::Budget,
                                ctx,
                            )),
                        Border::new()
                            .grid_column(1)
                            .vertical_alignment(VerticalAlignment::Top)
                            .content(line(
                                "并发目标",
                                w.max_parallel.to_string(),
                                Action::Parallel,
                                ctx,
                            )),
                    )),
            );
        }
        blocks.push(w::ruled_row(
            theme,
            label_actions(
                "Luna 监工".into(),
                vec![crate::pages::switch(
                    theme,
                    state
                        .workflow()
                        .map_or(state.probe_supervision, |w| w.luna_supervision),
                    ctx.callback(|v| Msg::Scheduling(Action::Supervision(v))),
                )],
            ),
        ));
        blocks.push(secondary(
            "开启时需有同一工作流的在线监工；关闭仍保留预算、模型池和恢复保护。",
        ));
        blocks.push(section(
            theme,
            "flow-manage",
            "工作流管理",
            "新建、移除以及批量调整依赖。",
            // 2×2 instead of a vertical stack: four chunky full-width buttons
            // read as a page of their own inside the narrow inspector.
            vec![uniform_grid(
                2,
                6.0,
                vec![
                    button("新工作流", || Action::New, ctx),
                    button("移除工作流", || Action::RemoveWorkflow, ctx),
                    button("顺序串联", || Action::Chain, ctx),
                    button("清空连线", || Action::Unlink, ctx),
                ],
            )],
            state.expanded.contains("flow-manage"),
            ctx,
        ));
    }
    if state.show_node_details
        && let Some(w) = state.workflow()
        && let Some(n) = state.node.and_then(|i| w.nodes.get(i))
    {
        blocks.push(
            TextBlock::new()
                .text(format!(
                    "绑定路线：{}",
                    state
                        .snapshot
                        .routes
                        .iter()
                        .find(|r| r.route_id == n.route_id)
                        .map_or_else(
                            || n.route_id.clone(),
                            |r| format!("{} / {} ({})", r.source, r.model, r.effort)
                        )
                ))
                .text_wrapping(TextWrapping::Wrap)
                .max_width(280.0)
                .into(),
        );
        blocks.push(line("节点名称", n.title.clone(), Action::NodeTitle, ctx));
        blocks.push(line(
            "分类标签（逗号分隔）",
            n.tags.join(", "),
            Action::Tags,
            ctx,
        ));
        blocks.push(row(vec![
            button("↑ 优先", || Action::MoveNode(-1), ctx),
            button("↓ 优先", || Action::MoveNode(1), ctx),
            button("移除节点", || Action::RemoveNode, ctx),
        ]));
        blocks.push(line(
            "节点职责（可自由调整）",
            n.role.clone(),
            Action::Role,
            ctx,
        ));
        if n.node_type == "model" {
            blocks.push(
                TextBlock::new()
                    .text(format!(
                        "备用 / 补位路线：{}",
                        n.fallback_routes
                            .iter()
                            .map(|id| state
                                .snapshot
                                .routes
                                .iter()
                                .find(|r| &r.route_id == id)
                                .map_or(id.clone(), |r| format!("{} / {}", r.source, r.model)))
                            .collect::<Vec<_>>()
                            .join(" → ")
                    ))
                    .text_wrapping(TextWrapping::Wrap)
                    .into(),
            );
            for route in state
                .routes()
                .into_iter()
                .filter(|r| {
                    state
                        .draft
                        .pool
                        .iter()
                        .any(|p| p.route_id == r.route_id && p.expires_at > model::now_ms())
                        && r.route_id != n.route_id
                        && !n.fallback_routes.contains(&r.route_id)
                })
                .take(8)
            {
                let id = route.route_id.clone();
                blocks.push(button(
                    &format!("+ 备用 {} / {}", route.source, route.model),
                    move || Action::AddFallback(id.clone()),
                    ctx,
                ));
            }
            blocks.push(button("清空备用路线", || Action::ClearFallback, ctx));
        }
        blocks.push(row(vec![
            ComboBox::new()
                .items_source(state.templates.iter().map(|t| t.title.as_str()))
                .selected_index(Some(state.template_index))
                .width(180.0)
                .on_selection_changed(ctx.callback(|v| Msg::Scheduling(Action::PickTemplate(v))))
                .into(),
            button(
                "套用",
                {
                    let i = state.template_index;
                    move || Action::ApplyTemplate(i)
                },
                ctx,
            ),
        ]));
        let mut advanced = Vec::new();
        for (label, key) in [
            ("任务类型", "kind"),
            ("节点提示词", "prompt"),
            ("输出模式 fixed/adaptive/native", "output_mode"),
            ("最大输出 Tokens", "max_output_tokens"),
            ("节点 Token 预算", "token_budget"),
            ("首次响应期待（秒）", "deadline_s"),
            ("首次活动等待（秒）", "ttft_s"),
            ("硬截止（秒）", "hard_deadline_s"),
            ("停滞窗口（秒）", "idle_s"),
            ("报告字符上限", "report_max_chars"),
        ] {
            let value = n.task.get(key).map_or_else(String::new, |v| {
                v.as_str().map_or_else(
                    || {
                        if v.is_null() {
                            String::new()
                        } else {
                            v.to_string()
                        }
                    },
                    str::to_owned,
                )
            });
            let view = line(label, value, move |v| Action::Param(key.into(), v), ctx);
            if key == "kind" || key == "prompt" {
                blocks.push(view);
            } else {
                advanced.push(view);
            }
        }
        advanced.push(line(
            "高级节点参数 JSON",
            state.task_editor.clone(),
            Action::TaskEditor,
            ctx,
        ));
        advanced.push(button("应用高级参数", || Action::ApplyTaskEditor, ctx));
        blocks.push(section(
            theme,
            "node-advanced",
            "高级调用参数",
            "输出容量、超时、重试和精确 JSON 设置。",
            advanced,
            state.expanded.contains("node-advanced"),
            ctx,
        ));
    }
    // A card, like the run pane: the fields used to float straight on the page
    // background next to the canvas edge, so the pane had no visible extent.
    // The scroll content keeps a 12 DIP gutter so the scrollbar never covers
    // the right edge of the text boxes.
    w::card(
        theme,
        ScrollViewer::new()
            .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
            .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
            .content(
                StackPanel::new()
                    .margin(Thickness::new(0.0, 0.0, 12.0, 0.0))
                    .spacing(12.0)
                    .keyed_children(crate::pages::keyed(blocks)),
            ),
    )
}

/// Status color classes for the node canvas; resolved to brushes in the draw
/// closure (brushes are device-bound, the classification is plain data).
#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Good,
    Warn,
    Bad,
    Info,
    Muted,
}

/// Classifies a router run-state string into a badge. Keyword matching on the
/// lowercase value keeps unknown router vocabulary from mislabeling a node —
/// anything non-final reads as waiting.
fn run_tone(state: &str) -> (Tone, String) {
    let s = state.to_ascii_lowercase();
    if s.contains("run") || s.contains("dispatch") {
        (Tone::Info, "运行中".into())
    } else if s.contains("succeed") || s.contains("complete") || s.contains("done") {
        (Tone::Good, "已完成".into())
    } else if s.contains("fail") || s.contains("error") {
        (Tone::Bad, "失败".into())
    } else if s.contains("cancel") || s.contains("abort") {
        (Tone::Muted, "已取消".into())
    } else {
        (Tone::Muted, "等待中".into())
    }
}

/// One node's canvas status: the connectivity light of its route and the
/// live run badge, both as tone + short label (None when not applicable).
type NodeStatus = (Option<(Tone, String)>, Option<(Tone, String)>);

/// Trims `text` to roughly `units` em-halves (ASCII ≈ 1, CJK ≈ 2) with an
/// ellipsis. Direct2D `DrawText` neither clips nor trims a NoWrap line, so a
/// long model id ran out of the 200 DIP library column into the canvas.
fn ellipsize(text: &str, units: f64) -> String {
    let width = |c: char| if c.is_ascii() { 1.0 } else { 2.0 };
    if text.chars().map(width).sum::<f64>() <= units {
        return text.to_owned();
    }
    let mut used = 1.0; // room for the ellipsis
    let mut out = String::new();
    for c in text.chars() {
        used += width(c);
        if used > units {
            break;
        }
        out.push(c);
    }
    out + "…"
}

fn canvas(state: &State, theme: &Theme, ctx: &ViewContext<Shell>) -> View {
    // Same page size the hit test and pagination use — rows per canvas
    // height. A stale page index (window shrank between messages) clamps to
    // the last page instead of drawing an empty strip.
    let page_size = state.palette_page_size();
    let pages = state.routes().len().saturating_sub(1) / page_size + 1;
    let palette_page = state.palette.min(pages - 1);
    let palette: Vec<_> = state
        .routes()
        .into_iter()
        .skip(palette_page * page_size)
        .take(page_size)
        .map(|r| (ellipsize(&r.model, 31.0), ellipsize(&r.source, 34.0)))
        .collect();
    let nodes: Vec<(String, String, String, f64, f64, String)> = state
        .workflow()
        .map(|w| {
            w.nodes
                .iter()
                .map(|n| {
                    (
                        n.id.clone(),
                        n.title.clone(),
                        n.tags.join(" · "),
                        n.x,
                        n.y,
                        n.node_type.clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    // Per-node status in the same order as `nodes`: the connectivity light of
    // the bound route, and the live run state once a run reports per-node
    // results. Plain data — the draw closure stays a pure renderer.
    let routes_by_id: std::collections::HashMap<&str, &Route> = state
        .snapshot
        .routes
        .iter()
        .map(|r| (r.route_id.as_str(), r))
        .collect();
    let run_states: std::collections::HashMap<String, String> = state
        .detail
        .as_ref()
        .and_then(|d| d["nodes"].as_object())
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v["job_id"].as_str().is_some())
                .filter_map(|(k, v)| v["state"].as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let statuses: Vec<NodeStatus> = state
        .workflow()
        .map(|w| {
            w.nodes
                .iter()
                .map(|n| {
                    let light = if n.node_type == "input" {
                        // 母节点：工作流入口，恒为绿色提示而非模型连通性。
                        // 状态带只有一行高——标签必须短到任何缩放都不折行。
                        Some((Tone::Good, "入口".into()))
                    } else if n.node_type != "model" {
                        None
                    } else {
                        // Router vocabulary: "verified" / "unverified" /
                        // "verification_required" / "" — only a verified
                        // route is a green light, anything else is a warning
                        // unless the router reports a hard failure.
                        match routes_by_id.get(n.route_id.as_str()) {
                            Some(r) if r.connectivity == "verified" => {
                                Some((Tone::Good, "已连通".into()))
                            }
                            Some(r) if r.connectivity.is_empty() => {
                                Some((Tone::Warn, "未验证".into()))
                            }
                            Some(r) if r.connectivity == "unverified" => {
                                Some((Tone::Warn, "未验证".into()))
                            }
                            Some(r) if r.connectivity == "verification_required" => {
                                Some((Tone::Warn, "需要验证".into()))
                            }
                            Some(_) => Some((Tone::Bad, "连通异常".into())),
                            None => Some((Tone::Warn, "未绑定模型".into())),
                        }
                    };
                    let badge = run_states.get(&n.id).map(|s| run_tone(s));
                    (light, badge)
                })
                .collect()
        })
        .unwrap_or_default();
    let positions: std::collections::HashMap<String, (f64, f64)> =
        nodes.iter().map(|n| (n.0.clone(), (n.3, n.4))).collect();
    let edges = state
        .workflow()
        .map(|w| w.edges.clone())
        .unwrap_or_default();
    let (pan, zoom, selected) = (state.pan, state.zoom, state.node);
    let connect_mode = state.connect_mode;
    let (bg, card, ink, accent, grid) = (
        theme.card_cf,
        theme.card_cf,
        theme.subtle_cf,
        theme.accent_cf,
        theme.divider_cf,
    );
    let family = theme.font_family.clone();
    let viewport = state.viewport.clone();
    let page = theme.page_cf;
    let drag = state.drag.current.clone();
    let pointer = state.pointer;
    let hover = state.hover;
    let flash = state.flash;
    let animate = flash.is_some();
    let model_title = if let Some(Drag::Model(i)) = &drag {
        state
            .routes()
            .get(*i)
            .map_or(String::new(), |r| r.model.clone())
    } else {
        String::new()
    };
    let world = (
        (pointer.0 - 210.0 - pan.0) / zoom,
        (pointer.1 - pan.1) / zoom,
    );
    let target = if pointer.0 >= 210.0 {
        state.workflow().and_then(|w| w.hit(world.0, world.1))
    } else {
        None
    };
    let valid_target = target.is_some_and(|to| {
        state.workflow().is_some_and(|w| match drag {
            Some(Drag::Model(_)) => w.nodes[to].node_type == "model",
            Some(Drag::Edge(from)) => w.can_connect(from, to).is_ok(),
            _ => false,
        })
    });
    let state_palette = palette_page * page_size;
    let draw = move |c: &windows_canvas::DrawContext<'_>| {
        use windows_canvas::{
            Ellipse, PathBuilder, Rect, RoundedRect, TextAlignment, TextFormat, Vector2,
            WordWrapping,
        };
        viewport.set((f64::from(c.width), f64::from(c.height)));
        c.clear(bg);
        let text = TextFormat::new(&family, 12.0)?;
        // Status band: one line high — wrapping stacked a second line past
        // the node edge, so band text never wraps (it just trims instead).
        let band = TextFormat::new(&family, 12.0)?.with_word_wrapping(WordWrapping::NoWrap);
        let tail = TextFormat::new(&family, 12.0)?
            .with_alignment(TextAlignment::Trailing)
            .with_word_wrapping(WordWrapping::NoWrap);
        let brush = c.create_solid_brush(ink)?;
        let line = c.create_solid_brush(grid)?;
        let hi = c.create_solid_brush(accent)?;
        let fill = c.create_solid_brush(card)?;
        let picked = c.create_solid_brush(windows_canvas::ColorF { a: 0.16, ..accent })?;
        let good = c.create_solid_brush(windows_canvas::ColorF::from_rgba8(62, 190, 132, 255))?;
        let bad = c.create_solid_brush(windows_canvas::ColorF::from_rgba8(235, 108, 108, 255))?;
        let warn = c.create_solid_brush(windows_canvas::ColorF::from_rgba8(232, 176, 84, 255))?;
        if nodes.is_empty() {
            c.draw_text(
                "拖入模型，开始编排工作流",
                &text,
                &Rect::new(240.0, 52.0, c.width - 20.0, 84.0),
                &hi,
            );
            c.draw_text(
                "或使用上方添加节点，选择研究、核验与汇总等职责模板",
                &text,
                &Rect::new(240.0, 90.0, c.width - 20.0, 140.0),
                &brush,
            );
        }

        for x in (210..c.width as usize).step_by(24) {
            c.draw_line(
                Vector2::new(x as f32, 0.0),
                Vector2::new(x as f32, c.height),
                &line,
                0.3,
            );
        }
        for y in (0..c.height as usize).step_by(24) {
            c.draw_line(
                Vector2::new(210.0, y as f32),
                Vector2::new(c.width, y as f32),
                &line,
                0.3,
            );
        }
        let point = |x: f64, y: f64| {
            Vector2::new((210.0 + pan.0 + x * zoom) as f32, (pan.1 + y * zoom) as f32)
        };
        for e in &edges {
            if let (Some(a), Some(b)) = (positions.get(&e.from), positions.get(&e.to)) {
                let tail = point(a.0 + 220.0, a.1 + 56.0);
                let tip = point(b.0, b.1 + 56.0);
                c.draw_line(tail, tip, &hi, 2.0);
                // Arrowhead at the target end: the bare line read as an
                // undirected association; the chevron makes from→to legible
                // at any zoom.
                let (dx, dy) = (tip.x - tail.x, tip.y - tail.y);
                let len = (dx * dx + dy * dy).sqrt();
                if len > 26.0 {
                    let (ux, uy) = (dx / len, dy / len);
                    let bx = tip.x - ux * 12.0;
                    let by = tip.y - uy * 12.0;
                    let arrow = PathBuilder::new(c.device())?.polygon([
                        Vector2::new(tip.x, tip.y),
                        Vector2::new(bx - uy * 5.0, by + ux * 5.0),
                        Vector2::new(bx + uy * 5.0, by - ux * 5.0),
                    ])?;
                    c.fill_path(&arrow, &hi);
                }
            }
        }
        for (i, n) in nodes.iter().enumerate() {
            let p = point(n.3, n.4);
            let w = (220.0 * zoom) as f32;
            let h = (112.0 * zoom) as f32;
            if p.x + w < 210.0 || p.x > c.width || p.y + h < 0.0 || p.y > c.height {
                continue;
            }
            let master = n.5 == "input";
            let rect = Rect::new(p.x, p.y, p.x + w, p.y + h);
            let rounded = RoundedRect::new(rect, 8.0, 8.0);
            c.fill_rounded_rect(&rounded, &fill);
            if selected == Some(i) {
                c.fill_rounded_rect(&rounded, &picked);
            }
            c.draw_rounded_rect(
                &rounded,
                if selected == Some(i) || hover == Some(i) {
                    &hi
                } else if master {
                    // 母节点 keeps a green frame so the entry point reads
                    // apart from model nodes at any zoom.
                    &good
                } else {
                    &line
                },
                if selected == Some(i) {
                    3.0
                } else if master {
                    2.0
                } else {
                    1.5
                },
            );
            if let Some((active, _, start)) = flash
                && active == i
            {
                let t = (start.elapsed().as_secs_f32() / 0.22).clamp(0.0, 1.0);
                let spread = 3.0 + 10.0 * t;
                let glow = c.create_solid_brush(windows_canvas::ColorF {
                    a: (1.0 - t) * 0.7,
                    ..accent
                })?;
                c.draw_rounded_rect(
                    &RoundedRect::new(
                        Rect::new(
                            p.x - spread,
                            p.y - spread,
                            p.x + w + spread,
                            p.y + h + spread,
                        ),
                        10.0,
                        10.0,
                    ),
                    &glow,
                    3.0,
                );
            }

            if zoom < 0.3 {
                continue;
            }
            c.draw_text(
                &n.1,
                &text,
                &Rect::new(p.x + 12.0, p.y + 9.0, p.x + w - 12.0, p.y + 43.0),
                &hi,
            );
            // Tags stop above the status band so three-line tags can't run
            // under it.
            c.draw_text(
                &n.2,
                &text,
                &Rect::new(p.x + 12.0, p.y + 44.0, p.x + w - 12.0, p.y + h - 30.0),
                &brush,
            );
            // Bottom band: connectivity light on the left, run state (or the
            // selection mark when idle) tail-aligned on the right.
            if zoom >= 0.6 {
                let top = p.y + h - 26.0;
                let bottom = p.y + h - 4.0;
                if let Some((tone, label)) = statuses.get(i).and_then(|s| s.0.as_ref()) {
                    let light = match tone {
                        Tone::Good => &good,
                        Tone::Bad => &bad,
                        Tone::Warn => &warn,
                        Tone::Info => &hi,
                        Tone::Muted => &brush,
                    };
                    c.draw_text(
                        &format!("● {label}"),
                        &band,
                        &Rect::new(p.x + 12.0, top, p.x + w * 0.55, bottom),
                        light,
                    );
                }
                if let Some((tone, label)) = statuses.get(i).and_then(|s| s.1.as_ref()) {
                    let badge = match tone {
                        Tone::Good => &good,
                        Tone::Bad => &bad,
                        Tone::Warn => &warn,
                        Tone::Info => &hi,
                        Tone::Muted => &brush,
                    };
                    c.draw_text(
                        label,
                        &tail,
                        &Rect::new(p.x + w * 0.55, top, p.x + w - 10.0, bottom),
                        badge,
                    );
                } else if selected == Some(i) {
                    c.draw_text(
                        "✓ 已选中",
                        &tail,
                        &Rect::new(p.x + w * 0.55, top, p.x + w - 10.0, bottom),
                        &hi,
                    );
                }
            }
            // Ports: masters are entry points — a filled output port only;
            // every other node shows a hollow output port on the right and a
            // hollow input port on the left, so edge direction reads even
            // without following the arrowheads.
            if master {
                c.fill_ellipse(
                    &Ellipse {
                        center: Vector2::new(p.x + w + 3.0, p.y + h / 2.0),
                        radius_x: 4.5,
                        radius_y: 4.5,
                    },
                    &good,
                );
            } else if zoom >= 0.6 {
                c.draw_ellipse(
                    &Ellipse {
                        center: Vector2::new(p.x + w + 2.0, p.y + h / 2.0),
                        radius_x: 4.5,
                        radius_y: 4.5,
                    },
                    &hi,
                    1.5,
                );
                c.draw_ellipse(
                    &Ellipse {
                        center: Vector2::new(p.x - 2.0, p.y + h / 2.0),
                        radius_x: 4.5,
                        radius_y: 4.5,
                    },
                    &line,
                    1.5,
                );
            }
        }
        if let Some(to) = target
            && matches!(drag, Some(Drag::Model(_) | Drag::Edge(_)))
        {
            let n = &nodes[to];
            let p = point(n.3, n.4);
            c.draw_rounded_rect(
                &RoundedRect::new(
                    Rect::new(
                        p.x - 4.0,
                        p.y - 4.0,
                        p.x + (220.0 * zoom) as f32 + 4.0,
                        p.y + (112.0 * zoom) as f32 + 4.0,
                    ),
                    10.0,
                    10.0,
                ),
                if valid_target { &good } else { &bad },
                3.0,
            );
        }
        if let Some(Drag::Edge(from)) = drag
            && let Some(n) = nodes.get(from)
        {
            c.draw_line(
                point(n.3 + 220.0, n.4 + 56.0),
                Vector2::new(pointer.0 as f32, pointer.1 as f32),
                if target.is_some() {
                    if valid_target { &good } else { &bad }
                } else {
                    &hi
                },
                2.5,
            );
        }
        if matches!(drag, Some(Drag::Model(_))) && pointer.0 >= 210.0 {
            let x = pointer.0 as f32;
            let y = pointer.1 as f32;
            let backing = c.create_solid_brush(windows_canvas::ColorF {
                r: page.r,
                g: page.g,
                b: page.b,
                a: 0.94,
            })?;
            let rect = RoundedRect::new(
                Rect::new(x, y, x + 220.0 * zoom as f32, y + 112.0 * zoom as f32),
                9.0,
                9.0,
            );
            c.fill_rounded_rect(&rect, &backing);
            c.draw_rounded_rect(
                &rect,
                if target.is_some() && !valid_target {
                    &bad
                } else {
                    &hi
                },
                2.5,
            );
            c.draw_text(
                &model_title,
                &text,
                &Rect::new(x + 12.0, y + 10.0, x + 210.0, y + 42.0),
                &hi,
            );
            c.draw_text(
                if target.is_some() {
                    if valid_target {
                        "松开替换此节点模型"
                    } else {
                        "本地节点不绑定模型"
                    }
                } else {
                    "松开创建模型节点"
                },
                &text,
                &Rect::new(x + 12.0, y + 46.0, x + 210.0, y + 82.0),
                &brush,
            );
        }
        let opaque = c.create_solid_brush(windows_canvas::ColorF {
            r: bg.r * bg.a + page.r * (1.0 - bg.a),
            g: bg.g * bg.a + page.g * (1.0 - bg.a),
            b: bg.b * bg.a + page.b * (1.0 - bg.a),
            a: 1.0,
        })?;
        c.fill_rect(&Rect::new(0.0, 0.0, 210.0, c.height), &opaque);
        c.draw_text(
            "模型库 · 拖入节点框",
            &text,
            &Rect::new(8.0, 8.0, 200.0, 36.0),
            &hi,
        );
        // Two fixed lines per 42 DIP row — model, then its source — instead of
        // "source / model" wrapping onto a second line only when long, which
        // left single-line rows top-heavy and the list's rhythm uneven.
        let small = TextFormat::new(&family, 11.0)?.with_word_wrapping(WordWrapping::NoWrap);
        let faint = c.create_solid_brush(windows_canvas::ColorF {
            a: ink.a * 0.7,
            ..ink
        })?;
        for (i, (model, source)) in palette.iter().enumerate() {
            let y = 42.0 + i as f32 * 42.0;
            if matches!(drag,Some(Drag::Model(active)) if active==state_palette+i) {
                c.fill_rounded_rect(
                    &RoundedRect::new(Rect::new(4.0, y - 3.0, 205.0, y + 38.0), 6.0, 6.0),
                    &picked,
                );
                c.draw_rounded_rect(
                    &RoundedRect::new(Rect::new(4.0, y - 3.0, 205.0, y + 38.0), 6.0, 6.0),
                    &hi,
                    1.5,
                );
            }
            c.draw_text(
                model,
                &band,
                &Rect::new(10.0, y + 2.0, 202.0, y + 20.0),
                &brush,
            );
            c.draw_text(
                source,
                &small,
                &Rect::new(10.0, y + 20.0, 202.0, y + 36.0),
                &faint,
            );
        }
        // Mode badge, top-right of the canvas: which gesture a drag on a node
        // performs right now, and how to switch. Accent-framed in connect
        // mode so the riskier mode is never mistaken for the default.
        let (mode, how) = if connect_mode {
            ("连线模式", "从节点拖到下游节点 · 画布锁定 · 双击空白处切换")
        } else {
            ("移动模式", "拖动节点或平移画布 · 双击空白处切换")
        };
        let right = c.width - 12.0;
        let badge = RoundedRect::new(Rect::new(right - 300.0, 10.0, right, 54.0), 8.0, 8.0);
        c.fill_rounded_rect(&badge, &opaque);
        c.draw_rounded_rect(
            &badge,
            if connect_mode { &hi } else { &line },
            if connect_mode { 1.5 } else { 1.0 },
        );
        c.draw_text(
            mode,
            &tail,
            &Rect::new(right - 290.0, 14.0, right - 10.0, 32.0),
            if connect_mode { &hi } else { &brush },
        );
        c.draw_text(
            how,
            &tail,
            &Rect::new(right - 290.0, 32.0, right - 10.0, 50.0),
            &brush,
        );
        Ok(())
    };
    let painting = if animate {
        windows_canvas::animated_canvas(draw)
    } else {
        windows_canvas::canvas_invalidated(&state.inv, draw)
    };
    Border::new()
        .background(Brush::Solid(Color::argb(0, 0, 0, 0)))
        .capture_pointer_on_press(true)
        .on_pointer_pressed(ctx.callback(|p| Msg::Scheduling(Action::Press(p))))
        .on_pointer_moved(ctx.callback(|p: PointerEventInfo| Msg::Scheduling(Action::Move(p))))
        .on_pointer_released(ctx.callback(|p| Msg::Scheduling(Action::Release(p))))
        .on_pointer_capture_lost(ctx.callback(|_| Msg::Scheduling(Action::CaptureLost)))
        .on_pointer_canceled(ctx.callback(|_| Msg::Scheduling(Action::StopDrag)))
        .content(painting)
}

#[cfg(test)]
mod gesture_tests {
    use super::{Drag, Gesture};

    #[test]
    fn winui_capture_loss_before_release_preserves_exactly_one_model_drop() {
        let mut gesture = Gesture::default();
        gesture.begin(Drag::Model(4));
        gesture.lost();
        assert!(matches!(gesture.release(), Some(Drag::Model(4))));
        assert!(gesture.release().is_none());
    }
    #[test]
    fn pointer_cancellation_or_movement_after_capture_loss_prevents_a_stale_drop() {
        let mut gesture = Gesture::default();
        gesture.begin(Drag::Model(4));
        gesture.lost();
        assert!(gesture.movement().is_none());
        assert!(gesture.release().is_none());
        gesture.begin(Drag::Model(5));
        gesture.cancel();
        assert!(gesture.release().is_none());
    }
    #[test]
    fn fresh_press_after_capture_loss_replaces_old_drag_and_edges_survive_normal_release() {
        let mut gesture = Gesture::default();
        gesture.begin(Drag::Model(4));
        gesture.lost();
        gesture.begin(Drag::Edge(1));
        assert!(matches!(gesture.movement(), Some(Drag::Edge(1))));
        gesture.lost();
        assert!(matches!(gesture.release(), Some(Drag::Edge(1))));
    }
    #[test]
    fn primary_run_explains_empty_unsaved_and_unsupervised_workflows() {
        let mut state = super::State {
            loaded: true,
            ..Default::default()
        };
        assert!(state.run_disabled_reason().unwrap().contains("添加节点"));
        let mut workflow = globaltokentracker_core::scheduler::Workflow::new("fixture".into());
        workflow
            .add_route(
                &globaltokentracker_core::scheduler::Route {
                    route_id: "fixture-route".into(),
                    ..Default::default()
                },
                10.0,
                10.0,
            )
            .unwrap();
        state.draft.workflows.push(workflow);
        state.dirty = true;
        assert!(state.run_disabled_reason().unwrap().contains("保存"));
        state.dirty = false;
        assert!(state.run_disabled_reason().unwrap().contains("Luna"));
        state.draft.workflows[0].luna_supervision = false;
        assert!(state.run_disabled_reason().is_none());
        state.reposition(0, 10.0, 10.0);
        assert!(
            !state.dirty,
            "selecting without moving must not create unsaved changes"
        );
        state.reposition(0, 12.0, 10.0);
        assert!(state.dirty);
        state.dirty = false;
        state.detail = Some(serde_json::json!({"state":"running"}));
        assert!(state.run_disabled_reason().unwrap().contains("运行"));
    }
    #[test]
    fn connect_mode_locks_panning() {
        let mut state = super::State::default();
        assert!(state.can_pan());
        state.toggle_connect_mode();
        assert!(!state.can_pan());
        state.toggle_connect_mode();
        assert!(state.can_pan());
    }

    #[test]
    fn double_click_needs_two_close_presses_in_time() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let first = Some((t0, (300.0, 200.0)));
        assert!(super::is_double_click(
            first,
            t0 + Duration::from_millis(250),
            (304.0, 197.0)
        ));
        // Too slow, too far, or no first press: not a double-click.
        assert!(!super::is_double_click(
            first,
            t0 + Duration::from_millis(650),
            (300.0, 200.0)
        ));
        assert!(!super::is_double_click(
            first,
            t0 + Duration::from_millis(100),
            (320.0, 200.0)
        ));
        assert!(!super::is_double_click(None, t0, (300.0, 200.0)));
    }

    #[test]
    fn ellipsize_keeps_short_text_and_trims_long_ids() {
        assert_eq!(super::ellipsize("deepseek-flash", 31.0), "deepseek-flash");
        let long = super::ellipsize("deepseek-ai/DeepSeek-V4.1-Flash-1017", 31.0);
        assert!(long.ends_with('…'));
        assert_eq!(long.chars().count(), 31);
        // CJK counts double, so the same budget holds half as many glyphs.
        assert_eq!(super::ellipsize("一二三四五六七八九十", 10.0), "一二三四…");
    }

    #[test]
    fn palette_page_size_tracks_canvas_height_within_bounds() {
        let state = super::State::default();
        // Before the first canvas frame the viewport reports 0 — the floor
        // keeps the old 10-row page (and a sane page count) instead of
        // underflowing on the negative quotient.
        assert_eq!(state.palette_page_size(), 10);
        // Fullscreen canvas: rows fill the strip instead of stranding its
        // lower half (the complaint this replaces: 10 rows + dead space).
        state.viewport.set((1900.0, 1200.0));
        assert_eq!(state.palette_page_size(), 27); // (1200 − 50) / 42
        // Absurdly tall windows clamp so one page can't explode; short ones
        // keep 10 so the shared page index stays inside the sources table's
        // own 10-row range.
        state.viewport.set((1900.0, 6000.0));
        assert_eq!(state.palette_page_size(), 60);
        state.viewport.set((1900.0, 300.0));
        assert_eq!(state.palette_page_size(), 10);
    }
    #[test]
    fn master_node_is_an_entry_point_and_refuses_inbound_edges() {
        let mut workflow = globaltokentracker_core::scheduler::Workflow::new("fixture".into());
        let master = workflow.add_master(10.0, 10.0).unwrap();
        let route = globaltokentracker_core::scheduler::Route {
            route_id: "fixture-route".into(),
            ..Default::default()
        };
        let model = workflow.add_route(&route, 300.0, 10.0).unwrap();
        assert_eq!(workflow.nodes[master].node_type, "input");
        assert_eq!(workflow.nodes[master].route_id, "");
        // 母节点是工作流入口：出边合法，入边拒绝。
        assert!(workflow.can_connect(master, model).is_ok());
        assert!(workflow.can_connect(model, master).is_err());
        workflow.connect(master, model).unwrap();
        // disconnect clears the node's edges but keeps every node.
        workflow.disconnect(model);
        assert_eq!(workflow.edges.len(), 0);
        assert_eq!(workflow.nodes.len(), 2);
        // remove still takes the node and its edges with it.
        workflow.connect(master, model).unwrap();
        workflow.remove(model);
        assert_eq!(workflow.edges.len(), 0);
        assert_eq!(workflow.nodes.len(), 1);
    }
}
