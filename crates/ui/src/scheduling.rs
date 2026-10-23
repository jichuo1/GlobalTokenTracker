//! Native dispatch page and demand-rendered Direct2D workflow editor.

use crate::{Msg, Shell, theme::Theme, widgets as w};
use globaltokentracker_core::scheduler::{self as model, Route, Settings, Snapshot, Workflow};
use serde_json::{Value, json};
use windows_reactor::*;

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
    ToggleMenu(&'static str),
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
    Plan,
    PickTemplate(Option<usize>),
    ToggleParams,
    ToggleRun,
    TogglePan,
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
    show_run: bool,
    pan_tool: bool,
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
            show_run: false,
            pan_tool: false,
            viewport: std::rc::Rc::new(std::cell::Cell::new((800.0, 500.0))),
            submit_key: String::new(),
            submit_signature: String::new(),
            acknowledge: false,
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
            Action::ToggleMenu(id) => {self.open_menu=if self.open_menu==Some(id){None}else{Some(id)};},
            Action::CloseMenu => self.open_menu=None,
            Action::EditNode => {self.show_node_details=true;self.show_params=false;},
            Action::ClearSelection => {self.node=None;self.show_node_details=false;self.drag.cancel();},
            Action::ClearFlash(id) => {if self.flash.as_ref().is_some_and(|f|f.1==id) {self.flash=None;}},
            Action::PickTemplate(v) => {if let Some(i)=v {self.template_index=i.min(self.templates.len().saturating_sub(1));}},
            Action::ToggleParams => {self.tab="workflow";self.show_params = !self.show_params;self.show_node_details=false;},
            Action::ToggleRun => {self.tab="workflow";self.show_run = !self.show_run;},
            Action::TogglePan => { self.pan_tool = !self.pan_tool; self.drag.cancel(); },
            Action::Fit => { let (width,height)=self.viewport.get(); if let Some(w)=self.workflow() { let transform=w.fit_view((width-210.0).max(100.0),height);self.pan=transform.0;self.zoom=transform.1; } },
            Action::CloseInspector => {self.show_params=false;self.show_node_details=false;},
            Action::AddFallback(id) => {if let Some(i)=self.node && let Some(n)=self.workflow_mut().and_then(|w|w.nodes.get_mut(i)) && n.node_type=="model" && id!=n.route_id && !n.fallback_routes.contains(&id) && n.fallback_routes.len()<8 {n.fallback_routes.push(id);self.dirty=true;}}
            Action::ClearFallback => {if let Some(i)=self.node && let Some(n)=self.workflow_mut().and_then(|w|w.nodes.get_mut(i)){n.fallback_routes.clear();self.dirty=true;}}
            Action::Acknowledge(v) => self.acknowledge=v,
            Action::Collect(id) => self.request("router_collect",json!({"job_id":id,"max_chars":12000}),ctx),
            Action::Tab(tab) => self.tab=tab,
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
                                self.snapshot = snapshot;
                                self.loaded = true;
                                self.saving = false;
                            }
                        }
                    }
                        Ok(value) => {
                            if value.get("results").is_some(){self.report_text=bounded_pretty(&value);self.report_detail=Some(value);return;}
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
                let pages = self.routes().len().saturating_sub(1) / 10;
                self.palette = self
                    .palette
                    .saturating_add_signed(delta as isize)
                    .min(pages);
            }
            Action::Zoom(delta) => { let (width,height)=self.viewport.get(); let anchor=((width-210.0)/2.0,height/2.0);let world=((anchor.0-self.pan.0)/self.zoom,(anchor.1-self.pan.1)/self.zoom);self.zoom=(self.zoom+delta).clamp(0.1,1.8);self.pan=(anchor.0-world.0*self.zoom,anchor.1-world.1*self.zoom); },
            Action::Press(p) => {
                self.pointer=(p.x,p.y);self.error=None;
                if p.is_right_button_pressed || p.is_middle_button_pressed || (self.pan_tool && p.x >= 210.0) {
                    self.drag.begin(Drag::Pan(p.x - self.pan.0, p.y - self.pan.1));
                } else if p.x < 200.0 && p.y >= 42.0 {
                    let i = self.palette * 10 + ((p.y - 42.0) / 42.0) as usize;
                    if i < self.routes().len() {
                        self.drag.begin(Drag::Model(i));
                    }
                } else {
                    let (x, y) = self.point(p);
                    if let Some(w) = self.workflow()
                        && let Some(i) = w.hit(x, y) {
                            let n = &w.nodes[i];
                            let drag = if x > n.x + 200.0 && (y - n.y - 56.0).abs() < 18.0 {
                                Drag::Edge(i)
                            } else {
                                Drag::Node(i, x - n.x, y - n.y)
                            };
                            self.node = Some(i);
                            self.drag.begin(drag);
                        } else if p.x >= 210.0 { self.drag.begin(Drag::Pan(p.x-self.pan.0,p.y-self.pan.1)); }
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
                                    } else {
                                        match w.add_route(&route, x, y) {
                                            Ok(i) => self.node = Some(i),
                                            Err(e) => self.error = Some(e.to_string()),
                                        }
                                    }
                                }
                            }
                            self.add_pool(route.route_id);
                        }
                    }
                    Some(Drag::Edge(from)) => {
                        let to = self.workflow().and_then(|w| w.hit(x, y));
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
                self.pulse(ctx);
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

fn bounded_pretty(value: &Value) -> String {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    if text.chars().count() > 24000 {
        text.chars().take(24000).collect::<String>() + "\n[界面摘录；完整报告请按作业 ID 分页回收]"
    } else {
        text
    }
}
fn label_actions(label: String, actions: Vec<View>) -> View {
    Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(12.0)
        .children((
            TextBlock::new()
                .text(label)
                .text_wrapping(TextWrapping::Wrap),
            Border::new().grid_column(1).content(row(actions)),
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
        "添加节点" => "选择职责模板；也可以直接从左侧模型库拖入。",
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
    Button::new()
        .min_height(36.0)
        .on_click(ctx.callback(move |_| Msg::Scheduling(action())))
        .content(label)
        .tooltip(help_for(label))
}
fn command(
    label: &str,
    description: &str,
    action: impl Fn() -> Action + 'static,
    enabled: bool,
    ctx: &ViewContext<Shell>,
) -> View {
    Button::new()
        .is_enabled(enabled)
        .min_height(58.0)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .horizontal_content_alignment(HorizontalAlignment::Stretch)
        .vertical_content_alignment(VerticalAlignment::Center)
        .automation_name(format!("{label}。{description}"))
        .on_click(ctx.callback(move |_| Msg::Scheduling(Action::Menu(Box::new(action())))))
        .content(
            Border::new().padding(Thickness::xy(4.0, 3.0)).content(
                StackPanel::new().spacing(3.0).children((
                    TextBlock::new()
                        .text(label)
                        .font_weight(FontWeight::SEMI_BOLD)
                        .text_wrapping(TextWrapping::Wrap),
                    secondary(description),
                )),
            ),
        )
}
fn menu(label: &str, description: &str, id: &'static str, ctx: &ViewContext<Shell>) -> View {
    Button::new()
        .min_height(36.0)
        .on_click(ctx.callback(move |_| Msg::Scheduling(Action::ToggleMenu(id))))
        .content(format!("{label} ▾"))
        .tooltip(description)
}
fn more_menu(_state: &State, ctx: &ViewContext<Shell>) -> View {
    menu("更多", "预检、设置、模型池和服务管理。", "more", ctx)
}
fn more_items(state: &State, ctx: &ViewContext<Shell>) -> Vec<View> {
    vec![
        command(
            "运行前预检",
            help_for("运行前预检"),
            || Action::Plan,
            !state.busy && state.workflow().is_some(),
            ctx,
        ),
        command(
            "工作流设置",
            help_for("工作流设置"),
            || Action::ToggleParams,
            true,
            ctx,
        ),
        command(
            "来源与模型池",
            help_for("来源与模型池"),
            || Action::Tab("sources"),
            true,
            ctx,
        ),
        command(
            "调用配置",
            help_for("调用配置"),
            || Action::Tab("profiles"),
            true,
            ctx,
        ),
        command(
            "输入与结果",
            help_for("输入与结果"),
            || Action::ToggleRun,
            true,
            ctx,
        ),
        secondary("服务管理"),
        command(
            "刷新状态",
            help_for("刷新状态"),
            || Action::Load,
            !state.busy,
            ctx,
        ),
        command(
            "重新扫描来源",
            help_for("重新扫描来源"),
            || Action::Refresh,
            !state.busy,
            ctx,
        ),
        command(
            "启动服务",
            help_for("启动服务"),
            || Action::Start,
            !state.busy,
            ctx,
        ),
        command(
            "重载设置",
            help_for("重载设置"),
            || Action::Reload,
            !state.busy,
            ctx,
        ),
    ]
}
fn view_items(state: &State, ctx: &ViewContext<Shell>) -> Vec<View> {
    vec![
        command("适应全图", help_for("适应全图"), || Action::Fit, true, ctx),
        command(
            if state.pan_tool {
                "退出平移工具"
            } else {
                "平移工具"
            },
            "开启后可从任意节点位置拖动整个画布；空白处始终可直接拖动。",
            || Action::TogglePan,
            true,
            ctx,
        ),
        row(vec![
            button("−", || Action::Zoom(-0.1), ctx),
            secondary(format!("{:.0}%", state.zoom * 100.0)),
            button("+", || Action::Zoom(0.1), ctx),
        ]),
        command(
            "输入与结果",
            help_for("输入与结果"),
            || Action::ToggleRun,
            true,
            ctx,
        ),
        secondary("操作指南"),
        secondary(
            "拖左侧模型：创建节点；拖入模型节点：替换绑定。拖节点主体：移动；从右端口拖到下游：连线。蓝色外框和底部名称表示已选中，点击编辑节点查看参数。",
        ),
        secondary("连线决定依赖；独立分支可并行。预检不生成内容，运行会按已保存配置占用预算。"),
    ]
}
fn menu_panel(state: &State, theme: &Theme, ctx: &ViewContext<Shell>) -> View {
    let items = match state.open_menu {
        Some("add") => state
            .templates
            .iter()
            .enumerate()
            .map(|(i, t)| command(&t.title, &t.role, move || Action::AddTemplate(i), true, ctx))
            .collect(),
        Some("view") => view_items(state, ctx),
        _ => more_items(state, ctx),
    };
    Border::new()
        .background(
            theme
                .page_bg
                .unwrap_or(Brush::Theme(ThemeBrush::SolidBackground)),
        )
        .border_brush(theme.divider)
        .border_thickness(Thickness::uniform(1.0))
        .corner_radius(CornerRadius::uniform(8.0))
        .padding(Thickness::uniform(14.0))
        .width(320.0)
        .content(
            StackPanel::new().spacing(10.0).children((
                Grid::new()
                    .columns([GridLength::STAR, GridLength::Auto])
                    .column_spacing(12.0)
                    .children((
                        TextBlock::new()
                            .text(match state.open_menu {
                                Some("add") => "选择职责模板",
                                Some("view") => "视图与操作说明",
                                _ => "更多操作",
                            })
                            .font_size(16.0)
                            .text_wrapping(TextWrapping::Wrap)
                            .vertical_alignment(VerticalAlignment::Center),
                        Border::new()
                            .grid_column(1)
                            .vertical_alignment(VerticalAlignment::Center)
                            .content(button("收起", || Action::CloseMenu, ctx)),
                    )),
                ScrollViewer::new()
                    .max_height(380.0)
                    .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
                    .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                    .content(
                        StackPanel::new()
                            .margin(Thickness::new(0.0, 0.0, 12.0, 0.0))
                            .spacing(7.0)
                            .keyed_children(crate::pages::keyed(items)),
                    ),
            )),
        )
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
    StackPanel::new().spacing(4.0).children((
        TextBlock::new().text(label),
        TextBox::new()
            .accepts_return(
                label.contains("提示词")
                    || label.contains("职责")
                    || label.contains("材料")
                    || label.contains("JSON"),
            )
            .min_height(if label.contains("JSON") { 140.0 } else { 32.0 })
            .text(value)
            .on_text_changed(ctx.callback(move |v| Msg::Scheduling(action(v)))),
        TextBlock::new()
            .text(field_help(label))
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap)
            .opacity(0.72)
            .max_height(if field_help(label).is_empty() {
                0.0
            } else {
                90.0
            }),
    ))
}
fn section(title: &str, description: &str, items: Vec<View>) -> View {
    Expander::new().slots([
        SlotView::new(
            ExpanderSlot::Header,
            StackPanel::new()
                .spacing(3.0)
                .children((TextBlock::new().text(title), secondary(description))),
        ),
        SlotView::new(
            ExpanderSlot::Content,
            StackPanel::new()
                .spacing(10.0)
                .keyed_children(crate::pages::keyed(items)),
        ),
    ])
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

pub fn page(state: &State, theme: &Theme, ctx: &mut ViewContext<Shell>) -> Vec<View> {
    if state.tab == "workflow" {
        return vec![workspace_view(state, theme, ctx)];
    }
    let mut blocks = vec![
        Grid::new()
            .columns([GridLength::STAR, GridLength::Auto])
            .children((
                StackPanel::new().spacing(4.0).children((
                    TextBlock::new()
                        .text(if state.tab == "profiles" {
                            "调用配置"
                        } else {
                            "来源与模型池"
                        })
                        .font_size(20.0),
                    secondary(if state.tab == "profiles" {
                        "选择预设并按需覆盖；保存后作用于后续分配。"
                    } else {
                        "先选可用来源和模型，再回到工作流编排。"
                    }),
                )),
                Border::new().grid_column(1).content(row(vec![
                    Button::new()
                        .style(ButtonStyle::Accent)
                        .is_enabled(state.dirty && !state.busy)
                        .on_click(ctx.callback(|_| Msg::Scheduling(Action::Save)))
                        .content("保存修改")
                        .tooltip(help_for("保存修改")),
                    more_menu(state, ctx),
                ])),
            )),
    ];
    if state.open_menu.is_some() {
        blocks.push(menu_panel(state, theme, ctx));
    }
    blocks.push(row(vec![
        TextBlock::new().text("允许模型调度").into(),
        ToggleSwitch::new()
            .is_on(state.draft.enabled)
            .on_toggled(ctx.callback(|v| Msg::Scheduling(Action::Enabled(v))))
            .into(),
        TextBlock::new().text("定向模式 · 仅使用模型池").into(),
        ToggleSwitch::new()
            .is_on(state.draft.mode == "directed")
            .on_toggled(ctx.callback(|v| Msg::Scheduling(Action::Directed(v))))
            .into(),
    ]));
    blocks.push(
        TextBlock::new()
            .text(if state.dirty {
                "有未保存的修改；保存后作用于新派发，已发出的请求允许完成。"
            } else {
                "自动模式按能力选路；定向模式按池内顺序选路，池到期后停止派发。"
            })
            .text_wrapping(TextWrapping::Wrap)
            .into(),
    );
    if let Some(error) = &state.error {
        blocks.push(
            TextBlock::new()
                .text(error)
                .foreground(theme.danger)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
    }
    if !state.notice.is_empty() {
        blocks.push(
            TextBlock::new()
                .text(&state.notice)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
    }
    blocks.push(row(vec![
        button("工作流画布", || Action::Tab("workflow"), ctx),
        button("来源与模型池", || Action::Tab("sources"), ctx),
        button("调用配置档", || Action::Tab("profiles"), ctx),
    ]));
    if state.tab == "profiles" {
        blocks.push(TextBlock::new().text("选择预设后可以修改 JSON。节点显式参数优先于预设默认值，当前配置上限与模型原生上限共同生效。保存仅修改本协调器设置。").text_wrapping(TextWrapping::Wrap).into());
        for chunk in state
            .snapshot
            .call_presets
            .iter()
            .enumerate()
            .collect::<Vec<_>>()
            .chunks(3)
        {
            blocks.push(row(chunk
                .iter()
                .map(|(i, p)| {
                    let index = *i;
                    button(
                        p["title"].as_str().unwrap_or("预设"),
                        move || Action::CallPreset(index),
                        ctx,
                    )
                })
                .collect()));
        }
        blocks.push(line(
            "调用配置 JSON",
            state.profile_editor.clone(),
            Action::ProfileEditor,
            ctx,
        ));
        blocks.push(button("应用配置到待保存设置", || Action::ApplyProfile, ctx));
        blocks.push(
            TextBlock::new()
                .text(format!(
                    "当前生效值\n{}",
                    serde_json::to_string_pretty(&state.snapshot.effective_profile)
                        .unwrap_or_default()
                ))
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
        return blocks;
    }
    if state.tab == "sources" {
        let mut sources = std::collections::BTreeSet::new();
        for s in &state.snapshot.sources {
            if let Some(id) = s["id"].as_str() {
                sources.insert(id.to_owned());
            }
        }
        for r in &state.snapshot.routes {
            sources.insert(r.source.clone());
        }
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
            blocks.push(row(vec![
                TextBlock::new()
                    .text(format!("{source} · {count} 模型 · {good} 近期生成验证"))
                    .width(420.0)
                    .into(),
                ToggleSwitch::new()
                    .is_on(state.draft.sources.get(&source).copied().unwrap_or(true))
                    .on_toggled(
                        ctx.callback(move |v| Msg::Scheduling(Action::Source(key.clone(), v))),
                    )
                    .into(),
            ]));
        }
        blocks.push(row(vec![
            line("搜索模型 / 来源", state.query.clone(), Action::Query, ctx),
            line(
                "加入池后有效期（分钟，1–10080）",
                state.ttl.clone(),
                Action::Ttl,
                ctx,
            ),
        ]));
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
            blocks.push(label_actions(
                status,
                vec![
                    button("入池 / 续期", move || Action::Pool(id.clone()), ctx),
                    button("检测", move || Action::Check(probe_id.clone()), ctx),
                ],
            ));
        }
        blocks.push(row(vec![
            button("上一组模型", || Action::Palette(-1), ctx),
            TextBlock::new()
                .text(format!(
                    "{} / {}",
                    state.palette + 1,
                    routes.len().saturating_sub(1) / 10 + 1
                ))
                .into(),
            button("下一组模型", || Action::Palette(1), ctx),
        ]));
        blocks.push(TextBlock::new().text("扫描只读取模型目录；生成检测会产生少量用量，监工可按工作流选择，遵守共享探针预算。列表可见不代表已验证可用。").text_wrapping(TextWrapping::Wrap).into());
        blocks.push(w::section_header(theme, Symbol::List, "限时模型池"));
        for (i, p) in state.draft.pool.iter().enumerate() {
            let name = state
                .snapshot
                .routes
                .iter()
                .find(|r| r.route_id == p.route_id)
                .map_or(p.route_id.as_str(), |r| r.model.as_str());
            let left = p.expires_at.saturating_sub(model::now_ms()) / 60000;
            let id = p.route_id.clone();
            blocks.push(label_actions(
                format!(
                    "{} · {name} · {}",
                    i + 1,
                    if left == 0 {
                        "已到期".into()
                    } else {
                        format!("剩余 {left} 分钟")
                    }
                ),
                vec![
                    button("↑", move || Action::MovePool(i, -1), ctx),
                    button("↓", move || Action::MovePool(i, 1), ctx),
                    button("移出", move || Action::RemovePool(id.clone()), ctx),
                ],
            ));
        }
        return blocks;
    }
    blocks
}

fn workspace_view(state: &State, theme: &Theme, ctx: &mut ViewContext<Shell>) -> View {
    let reason = state.run_disabled_reason();
    let mut actions = vec![
        Button::new()
            .style(ButtonStyle::Accent)
            .min_height(36.0)
            .is_enabled(reason.is_none())
            .on_click(ctx.callback(|_| Msg::Scheduling(Action::Run)))
            .content("▶ 运行工作流")
            .tooltip(reason.unwrap_or(help_for("运行工作流"))),
        Button::new()
            .min_height(36.0)
            .is_enabled(state.dirty && !state.busy)
            .on_click(ctx.callback(|_| Msg::Scheduling(Action::Save)))
            .content("保存修改")
            .tooltip(help_for("保存修改")),
    ];
    if state.active_run() {
        actions.insert(0, button("停止", || Action::Cancel, ctx));
    }
    actions.push(more_menu(state, ctx));
    let head = Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .rows([GridLength::Auto, GridLength::Auto])
        .column_spacing(16.0)
        .row_spacing(6.0)
        .children((
            ComboBox::new()
                .placeholder_text("选择或添加工作流")
                .items_source(state.draft.workflows.iter().map(|w| w.name.as_str()))
                .selected_index((!state.draft.workflows.is_empty()).then_some(state.selected))
                .min_height(36.0)
                .max_width(300.0)
                .horizontal_alignment(HorizontalAlignment::Left)
                .on_selection_changed(ctx.callback(|v: Option<usize>| {
                    Msg::Scheduling(v.map_or(Action::StopDrag, Action::Select))
                })),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(row(actions)),
            TextBlock::new()
                .grid_row(1)
                .grid_column_span(2)
                .margin(Thickness::new(8.0, 0.0, 0.0, 0.0))
                .text(format!(
                    "{} · {} 个节点",
                    if state.busy {
                        "处理中"
                    } else if state.dirty {
                        "有未保存修改"
                    } else {
                        "已同步"
                    },
                    state.workflow().map_or(0, |w| w.nodes.len())
                ))
                .font_size(12.0)
                .foreground(theme.subtle)
                .text_wrapping(TextWrapping::Wrap),
        ));
    let view = menu(
        "视图与帮助",
        "画布缩放、平移、结果面板和操作说明。",
        "view",
        ctx,
    );
    let tools = Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .children((
            row(vec![
                TextBox::new()
                    .placeholder_text("搜索模型库")
                    .text(&state.query)
                    .width(200.0)
                    .min_height(36.0)
                    .on_text_changed(ctx.callback(|v| Msg::Scheduling(Action::Query(v))))
                    .tooltip("按模型或来源名称筛选左侧列表。"),
                Button::new()
                    .width(36.0)
                    .height(36.0)
                    .is_enabled(state.palette > 0)
                    .on_click(ctx.callback(|_| Msg::Scheduling(Action::Palette(-1))))
                    .content("‹")
                    .tooltip("上一页模型"),
                Button::new()
                    .width(36.0)
                    .height(36.0)
                    .is_enabled((state.palette + 1) * 10 < state.routes().len())
                    .on_click(ctx.callback(|_| Msg::Scheduling(Action::Palette(1))))
                    .content("›")
                    .tooltip("下一页模型"),
                TextBlock::new()
                    .text(format!(
                        "{} / {} 页",
                        state.palette + 1,
                        state.routes().len().saturating_sub(1) / 10 + 1
                    ))
                    .font_size(12.0)
                    .foreground(theme.subtle)
                    .min_width(64.0)
                    .into(),
            ]),
            Border::new().grid_column(1).content(row(vec![
                menu("＋ 添加节点", help_for("添加节点"), "add", ctx),
                view,
            ])),
        ));
    let side = state.show_node_details || state.show_params;
    let body = Grid::new()
        .columns([
            GridLength::STAR,
            GridLength::Pixel(if side { 310.0 } else { 0.0 }),
        ])
        .children((
            Border::new().content(canvas(state, theme, ctx)),
            Border::new()
                .grid_column(1)
                .padding(Thickness::xy(if side { 6.0 } else { 0.0 }, 0.0))
                .content(if side {
                    inspector(state, theme, ctx)
                } else {
                    Border::new().into()
                }),
        ));
    let selected = state
        .node
        .and_then(|i| state.workflow().and_then(|w| w.nodes.get(i)));
    let info = if let Some(node) = selected {
        StackPanel::new().spacing(4.0).children((
            TextBlock::new()
                .text(format!("已选中：{}", node.title))
                .font_size(14.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .foreground(theme.accent)
                .text_wrapping(TextWrapping::Wrap),
            secondary("拖动节点主体移动 · 从右端口连接下游"),
        ))
    } else {
        secondary(reason.unwrap_or("从左侧拖入模型，或添加职责模板开始编排。"))
    };
    let actions = if selected.is_some() {
        vec![
            button("编辑节点", || Action::EditNode, ctx),
            button("取消选择", || Action::ClearSelection, ctx),
        ]
    } else {
        vec![]
    };
    let context = Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(16.0)
        .min_height(40.0)
        .children((
            Border::new()
                .vertical_alignment(VerticalAlignment::Center)
                .content(info),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(row(actions)),
        ));
    let status = StackPanel::new().spacing(6.0).children((
        TextBlock::new()
            .text(state.error.as_deref().unwrap_or(""))
            .foreground(theme.danger)
            .text_wrapping(TextWrapping::Wrap)
            .max_height(if state.error.is_some() { 44.0 } else { 0.0 }),
        context,
    ));
    let main = Grid::new()
        .rows([
            GridLength::Auto,
            GridLength::Auto,
            GridLength::STAR,
            GridLength::Auto,
            GridLength::Auto,
        ])
        .row_spacing(10.0)
        .children((
            head,
            Border::new().grid_row(1).content(tools),
            Border::new().grid_row(2).content(body),
            Border::new().grid_row(3).content(status),
            Border::new().grid_row(4).content(if state.show_run {
                run_panel(state, ctx)
            } else {
                Border::new().into()
            }),
        ));
    let mut layers = vec![KeyedView::new("workspace", main)];
    if state.open_menu.is_some() {
        layers.push(KeyedView::new(
            "dismiss",
            Border::new()
                .background(Brush::Solid(Color::argb(0, 0, 0, 0)))
                .on_pointer_pressed(ctx.callback(|_| Msg::Scheduling(Action::CloseMenu))),
        ));
        layers.push(KeyedView::new(
            "menu",
            Border::new()
                .horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Top)
                .margin(Thickness::new(0.0, 48.0, 0.0, 0.0))
                .content(menu_panel(state, theme, ctx)),
        ));
    }
    Grid::new().keyed_children(layers)
}

fn run_panel(state: &State, ctx: &ViewContext<Shell>) -> View {
    let mut blocks = vec![
        Grid::new()
            .columns([GridLength::Pixel(210.0), GridLength::STAR])
            .column_spacing(12.0)
            .children((
                line(
                    "工作流 ID",
                    state.workflow_id.clone(),
                    Action::WorkflowId,
                    ctx,
                ),
                Border::new().grid_column(1).content(line(
                    "本次输入材料",
                    state.input.clone(),
                    Action::Input,
                    ctx,
                )),
            )),
        row(vec![
            TextBlock::new()
                .text("新建未知结果的运行前接受重复付费风险")
                .into(),
            ToggleSwitch::new()
                .is_on(state.acknowledge)
                .on_toggled(ctx.callback(|v| Msg::Scheduling(Action::Acknowledge(v))))
                .into(),
        ]),
    ];
    for run in &state.snapshot.runs {
        if let Some(id) = run["id"].as_str() {
            let id = id.to_owned();
            blocks.push(button(
                &format!(
                    "{} · {}",
                    run["name"].as_str().unwrap_or("运行"),
                    run["state"].as_str().unwrap_or("未知")
                ),
                move || Action::Detail(id.clone()),
                ctx,
            ));
        }
    }
    if let Some(detail) = &state.detail {
        if let Some(nodes) = detail["nodes"].as_object() {
            for (id, node) in nodes {
                if let Some(job) = node["job_id"].as_str() {
                    let job = job.to_owned();
                    blocks.push(button(
                        &format!(
                            "节点 {id} · {} · 查看报告",
                            node["state"].as_str().unwrap_or("未知")
                        ),
                        move || Action::Collect(job.clone()),
                        ctx,
                    ));
                }
            }
        }
        blocks.push(
            TextBlock::new()
                .text(state.detail_text.clone())
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
    }
    if state.report_detail.is_some() {
        blocks.push(
            TextBlock::new()
                .text(state.report_text.clone())
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
    }
    ScrollViewer::new()
        .height(190.0)
        .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
        .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
        .content(
            StackPanel::new()
                .spacing(8.0)
                .keyed_children(crate::pages::keyed(blocks)),
        )
}

fn inspector(state: &State, _theme: &Theme, ctx: &ViewContext<Shell>) -> View {
    let mut blocks = vec![
        TextBlock::new()
            .text(if state.show_params {
                "工作流设置"
            } else {
                "节点设置"
            })
            .font_size(18.0)
            .into(),
        secondary(if state.show_params {
            "配置本工作流的执行范围和保护条件。"
        } else {
            "修改当前选中节点的职责与交付要求。"
        }),
        button("收起设置", || Action::CloseInspector, ctx),
    ];
    if state.show_params {
        blocks.push(section(
            "工作流管理",
            "新建、移除以及批量调整依赖。",
            vec![
                button("新工作流", || Action::New, ctx),
                button("移除工作流", || Action::RemoveWorkflow, ctx),
                button("顺序串联", || Action::Chain, ctx),
                button("清空连线", || Action::Unlink, ctx),
            ],
        ));
        blocks.push(row(vec![
            TextBlock::new().text("Luna 监工").into(),
            ToggleSwitch::new()
                .is_on(
                    state
                        .workflow()
                        .map_or(state.probe_supervision, |w| w.luna_supervision),
                )
                .on_toggled(ctx.callback(|v| Msg::Scheduling(Action::Supervision(v))))
                .into(),
        ]));
        blocks.push(secondary(
            "开启时需有同一工作流的在线监工；关闭仍保留预算、模型池和恢复保护。",
        ));
        if let Some(w) = state.workflow() {
            blocks.push(line("工作流名称", w.name.clone(), Action::Name, ctx));
            blocks.push(line(
                "总 Token 预算",
                w.token_budget.to_string(),
                Action::Budget,
                ctx,
            ));
            blocks.push(line(
                "并发目标",
                w.max_parallel.to_string(),
                Action::Parallel,
                ctx,
            ));
        }
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
            "高级调用参数",
            "输出容量、超时、重试和精确 JSON 设置。",
            advanced,
        ));
    }
    ScrollViewer::new()
        .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
        .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
        .content(
            StackPanel::new()
                .spacing(8.0)
                .keyed_children(crate::pages::keyed(blocks)),
        )
}

fn canvas(state: &State, theme: &Theme, ctx: &ViewContext<Shell>) -> View {
    let palette: Vec<_> = state
        .routes()
        .into_iter()
        .skip(state.palette * 10)
        .take(10)
        .map(|r| format!("{} / {}", r.source, r.model))
        .collect();
    let nodes: Vec<(String, String, String, f64, f64)> = state
        .workflow()
        .map(|w| {
            w.nodes
                .iter()
                .map(|n| (n.id.clone(), n.title.clone(), n.tags.join(" · "), n.x, n.y))
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
    let state_palette = state.palette * 10;
    let draw = move |c: &windows_canvas::DrawContext<'_>| {
        use windows_canvas::{Rect, RoundedRect, TextFormat, Vector2};
        viewport.set((f64::from(c.width), f64::from(c.height)));
        c.clear(bg);
        let text = TextFormat::new(&family, 12.0)?;
        let brush = c.create_solid_brush(ink)?;
        let line = c.create_solid_brush(grid)?;
        let hi = c.create_solid_brush(accent)?;
        let fill = c.create_solid_brush(card)?;
        let picked = c.create_solid_brush(windows_canvas::ColorF { a: 0.16, ..accent })?;
        let good = c.create_solid_brush(windows_canvas::ColorF::from_rgba8(62, 190, 132, 255))?;
        let bad = c.create_solid_brush(windows_canvas::ColorF::from_rgba8(235, 108, 108, 255))?;
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
                c.draw_line(
                    point(a.0 + 220.0, a.1 + 56.0),
                    point(b.0, b.1 + 56.0),
                    &hi,
                    2.0,
                );
            }
        }
        for (i, n) in nodes.iter().enumerate() {
            let p = point(n.3, n.4);
            let w = (220.0 * zoom) as f32;
            let h = (112.0 * zoom) as f32;
            if p.x + w < 210.0 || p.x > c.width || p.y + h < 0.0 || p.y > c.height {
                continue;
            }
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
                } else {
                    &line
                },
                if selected == Some(i) { 3.0 } else { 1.5 },
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
            c.draw_text(
                &n.2,
                &text,
                &Rect::new(p.x + 12.0, p.y + 44.0, p.x + w - 12.0, p.y + h - 8.0),
                &brush,
            );
            if selected == Some(i) && zoom >= 0.6 {
                c.draw_text(
                    "✓ 已选中",
                    &text,
                    &Rect::new(p.x + 12.0, p.y + h - 28.0, p.x + w - 24.0, p.y + h - 4.0),
                    &hi,
                );
            }
            c.draw_text(
                "●",
                &text,
                &Rect::new(
                    p.x + w - 15.0,
                    p.y + h / 2.0 - 10.0,
                    p.x + w + 3.0,
                    p.y + h / 2.0 + 12.0,
                ),
                &hi,
            );
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
        for (i, title) in palette.iter().enumerate() {
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
            c.draw_text(title, &text, &Rect::new(8.0, y, 202.0, y + 38.0), &brush);
        }
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
}
