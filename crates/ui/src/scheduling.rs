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
    drag: Option<Drag>,
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
            drag: None,
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
        if (self.saving && !matches!(&action, Action::Received(_)))
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
            Action::StopDrag => self.drag = None,
            Action::PickTemplate(v) => {if let Some(i)=v {self.template_index=i.min(self.templates.len().saturating_sub(1));}},
            Action::ToggleParams => self.show_params = !self.show_params,
            Action::ToggleRun => self.show_run = !self.show_run,
            Action::TogglePan => { self.pan_tool = !self.pan_tool; self.drag = None; },
            Action::Fit => { let (width,height)=self.viewport.get(); if let Some(w)=self.workflow() { let transform=w.fit_view((width-210.0).max(100.0),height);self.pan=transform.0;self.zoom=transform.1; } },
            Action::CloseInspector => {self.show_params=false;self.node=None;},
            Action::AddFallback(id) => {if let Some(i)=self.node && let Some(n)=self.workflow_mut().and_then(|w|w.nodes.get_mut(i)) && n.node_type=="model" && id!=n.route_id && !n.fallback_routes.contains(&id) && n.fallback_routes.len()<8 {n.fallback_routes.push(id);self.dirty=true;}}
            Action::ClearFallback => {if let Some(i)=self.node && let Some(n)=self.workflow_mut().and_then(|w|w.nodes.get_mut(i)){n.fallback_routes.clear();self.dirty=true;}}
            Action::Acknowledge(v) => self.acknowledge=v,
            Action::Collect(id) => self.request("router_collect",json!({"job_id":id,"max_chars":12000}),ctx),
            Action::Tab(tab) => self.tab=tab,
            Action::CallPreset(i) => { if let Some(p)=self.snapshot.call_presets.get(i){self.draft.call_profile=p["value"].clone();self.profile_editor=serde_json::to_string_pretty(&self.draft.call_profile).unwrap_or_default();self.dirty=true;} }
            Action::ProfileEditor(v) => self.profile_editor=v,
            Action::ApplyProfile => match serde_json::from_str::<Value>(&self.profile_editor){Ok(v) if v.is_object()=>{self.draft.call_profile=v;self.dirty=true;},_=>self.error=Some("调用配置需要合法的 JSON 对象".into())},
            Action::Plan => {if let Some(w)=self.workflow(){self.request("router_workflow_plan",json!({"definition":w,"workflow_id":self.workflow_id}),ctx);}},
            Action::Reload => {
                self.dirty = false;
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
                self.node = None;
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
                if p.is_right_button_pressed || p.is_middle_button_pressed || (self.pan_tool && p.x >= 210.0) {
                    self.drag = Some(Drag::Pan(p.x - self.pan.0, p.y - self.pan.1));
                } else if p.x < 200.0 && p.y >= 42.0 {
                    let i = self.palette * 10 + ((p.y - 42.0) / 42.0) as usize;
                    if i < self.routes().len() {
                        self.drag = Some(Drag::Model(i));
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
                            self.drag = Some(drag);
                        } else if p.x >= 210.0 { self.drag = Some(Drag::Pan(p.x-self.pan.0,p.y-self.pan.1)); }
                }
            }
            Action::Move(p) => {
                if self.drag.is_none(){return;}
                let (x, y) = self.point(p);
                match self.drag.clone() {
                    Some(Drag::Node(i, dx, dy)) => {
                        if let Some(n) = self.workflow_mut().and_then(|w| w.nodes.get_mut(i)) {
                            n.x = (x - dx).clamp(-10000.0, 10000.0);
                            n.y = (y - dy).clamp(-10000.0, 10000.0);
                            self.dirty = true;
                        }
                    }
                    Some(Drag::Pan(dx, dy)) => self.pan = (p.x - dx, p.y - dy),
                    _ => {}
                }
            }
            Action::Release(p) => {
                let (x, y) = self.point(p);
                match self.drag.take() {
                    Some(Drag::Node(i,dx,dy)) => {if let Some(n)=self.workflow_mut().and_then(|w|w.nodes.get_mut(i)){n.x=(x-dx).clamp(-10000.0,10000.0);n.y=(y-dy).clamp(-10000.0,10000.0);self.dirty=true;}},
                    Some(Drag::Pan(dx,dy)) => self.pan=(p.x-dx,p.y-dy),
                    Some(Drag::Model(index)) => {
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
            }
            Action::WorkflowId(v) => self.workflow_id = v,
            Action::Input(v) => self.input = v,
            Action::Run => {
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

fn button(label: &str, action: impl Fn() -> Action + 'static, ctx: &ViewContext<Shell>) -> View {
    Button::new()
        .on_click(ctx.callback(move |_| Msg::Scheduling(action())))
        .content(TextBlock::new().text(label))
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
    ))
}
fn row(children: Vec<View>) -> View {
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(8.0)
        .keyed_children(crate::pages::keyed(children))
}

pub fn page(state: &State, theme: &Theme, ctx: &mut ViewContext<Shell>) -> Vec<View> {
    if state.tab == "workflow" {
        return vec![workspace_view(state, theme, ctx)];
    }
    let mut blocks = vec![w::section_header(theme, Symbol::World, "模型调度")];
    blocks.push(row(vec![
        button("启动服务", || Action::Start, ctx),
        button("刷新状态", || Action::Load, ctx),
        button("重新扫描来源", || Action::Refresh, ctx),
        button(
            if state.busy {
                "处理中…"
            } else {
                "保存并应用"
            },
            || Action::Save,
            ctx,
        ),
        button("重载设置", || Action::Reload, ctx),
    ]));
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
    let head = Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .children((
            row(vec![
                button("保存", || Action::Save, ctx),
                button("预检", || Action::Plan, ctx),
                button("运行", || Action::Run, ctx),
                button("停止", || Action::Cancel, ctx),
                button("模型池", || Action::Tab("sources"), ctx),
                button("调用配置", || Action::Tab("profiles"), ctx),
            ]),
            TextBlock::new()
                .grid_column(1)
                .text(if state.busy {
                    "处理中"
                } else if state.dirty {
                    "未保存"
                } else {
                    "已同步"
                })
                .foreground(theme.subtle)
                .vertical_alignment(VerticalAlignment::Center),
        ));
    let selection = row(vec![
        ComboBox::new()
            .items_source(state.draft.workflows.iter().map(|w| w.name.as_str()))
            .selected_index((!state.draft.workflows.is_empty()).then_some(state.selected))
            .width(230.0)
            .on_selection_changed(ctx.callback(|v: Option<usize>| {
                Msg::Scheduling(v.map_or(Action::StopDrag, Action::Select))
            }))
            .into(),
        TextBlock::new()
            .text("Luna 监工")
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
        ToggleSwitch::new()
            .is_on(
                state
                    .workflow()
                    .map_or(state.probe_supervision, |w| w.luna_supervision),
            )
            .on_toggled(ctx.callback(|v| Msg::Scheduling(Action::Supervision(v))))
            .into(),
        button("工作流参数", || Action::ToggleParams, ctx),
        button(
            if state.show_run {
                "收起运行面板"
            } else {
                "输入与结果"
            },
            || Action::ToggleRun,
            ctx,
        ),
    ]);
    let tools = row(vec![
        TextBox::new()
            .placeholder_text("搜索模型")
            .text(&state.query)
            .width(130.0)
            .on_text_changed(ctx.callback(|v| Msg::Scheduling(Action::Query(v))))
            .into(),
        button("‹", || Action::Palette(-1), ctx),
        button("›", || Action::Palette(1), ctx),
        ComboBox::new()
            .items_source(state.templates.iter().map(|t| t.title.as_str()))
            .selected_index(Some(state.template_index))
            .width(130.0)
            .on_selection_changed(ctx.callback(|v| Msg::Scheduling(Action::PickTemplate(v))))
            .into(),
        button(
            "添加",
            {
                let i = state.template_index;
                move || Action::AddTemplate(i)
            },
            ctx,
        ),
        button(
            if state.pan_tool {
                "平移中"
            } else {
                "平移"
            },
            || Action::TogglePan,
            ctx,
        ),
        button("适应", || Action::Fit, ctx),
        button("−", || Action::Zoom(-0.1), ctx),
        button("+", || Action::Zoom(0.1), ctx),
        TextBlock::new()
            .text(format!("{:.0}%", state.zoom * 100.0))
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    ]);
    let side = state.node.is_some() || state.show_params;
    let body = Grid::new()
        .columns([
            GridLength::STAR,
            GridLength::Pixel(if side { 300.0 } else { 0.0 }),
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
    let error = TextBlock::new()
        .text(state.error.as_deref().unwrap_or(""))
        .foreground(theme.danger)
        .text_wrapping(TextWrapping::Wrap)
        .max_height(if state.error.is_some() { 44.0 } else { 0.0 });
    let hint = TextBlock::new()
        .text("空白处左键拖动平移 · 拖动节点移动 · 从右端口连接 · 适应可查看全图")
        .foreground(theme.subtle)
        .font_size(theme.label_size);
    Grid::new()
        .rows([
            GridLength::Auto,
            GridLength::Auto,
            GridLength::Auto,
            GridLength::Auto,
            GridLength::STAR,
            GridLength::Auto,
        ])
        .row_spacing(8.0)
        .children((
            head,
            Border::new().grid_row(1).content(selection),
            Border::new().grid_row(2).content(tools),
            Border::new()
                .grid_row(3)
                .content(StackPanel::new().spacing(4.0).children((error, hint))),
            Border::new().grid_row(4).content(body),
            Border::new().grid_row(5).content(if state.show_run {
                run_panel(state, ctx)
            } else {
                Border::new().into()
            }),
        ))
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
    let mut blocks = vec![button("关闭参数栏", || Action::CloseInspector, ctx)];
    if state.show_params {
        blocks.push(row(vec![
            button("新工作流", || Action::New, ctx),
            button("移除工作流", || Action::RemoveWorkflow, ctx),
        ]));
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
        blocks.push(row(vec![
            button("顺序串联", || Action::Chain, ctx),
            button("清空连线", || Action::Unlink, ctx),
        ]));
    }
    if let Some(w) = state.workflow()
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
            blocks.push(line(
                label,
                value,
                move |v| Action::Param(key.into(), v),
                ctx,
            ));
        }
        blocks.push(line(
            "高级节点参数 JSON",
            state.task_editor.clone(),
            Action::TaskEditor,
            ctx,
        ));
        blocks.push(button("应用高级参数", || Action::ApplyTaskEditor, ctx));
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
    Border::new()
        .background(Brush::Solid(Color::argb(0, 0, 0, 0)))
        .capture_pointer_on_press(true)
        .on_pointer_pressed(ctx.callback(|p| Msg::Scheduling(Action::Press(p))))
        .on_pointer_moved(ctx.callback(|p: PointerEventInfo| Msg::Scheduling(Action::Move(p))))
        .on_pointer_released(ctx.callback(|p| Msg::Scheduling(Action::Release(p))))
        .on_pointer_capture_lost(ctx.callback(|_| Msg::Scheduling(Action::StopDrag)))
        .content(windows_canvas::canvas_invalidated(&state.inv, move |c| {
            use windows_canvas::{Rect, RoundedRect, TextFormat, Vector2};
            viewport.set((f64::from(c.width), f64::from(c.height)));
            c.clear(bg);
            let text = TextFormat::new(&family, 12.0)?;
            let brush = c.create_solid_brush(ink)?;
            let line = c.create_solid_brush(grid)?;
            let hi = c.create_solid_brush(accent)?;
            let fill = c.create_solid_brush(card)?;
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
                c.draw_rounded_rect(&rounded, if selected == Some(i) { &hi } else { &line }, 2.0);
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
                c.draw_text(title, &text, &Rect::new(8.0, y, 202.0, y + 38.0), &brush);
            }
            Ok(())
        }))
}
