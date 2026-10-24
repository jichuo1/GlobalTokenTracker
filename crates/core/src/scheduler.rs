//! Native scheduling client and graph operations. Credentials never leave loopback.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolEntry {
    pub route_id: String,
    pub expires_at: u64,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub title: String,
    pub route_id: String,
    #[serde(default = "model_node")]
    pub node_type: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub fallback_routes: Vec<String>,
    pub tags: Vec<String>,
    pub x: f64,
    pub y: f64,
    pub task: Value,
}

impl Node {
    pub fn is_model_backed(&self) -> bool {
        matches!(self.node_type.as_str(), "model" | "operator" | "reviewer")
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    #[default]
    Task,
    Report,
}

impl EdgeKind {
    pub fn is_task(&self) -> bool {
        *self == Self::Task
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "EdgeKind::is_task")]
    pub kind: EdgeKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub name: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub token_budget: u64,
    pub max_parallel: usize,
    #[serde(default = "supervised")]
    pub luna_supervision: bool,
    #[serde(
        default = "default_max_rounds",
        skip_serializing_if = "is_default_max_rounds"
    )]
    pub max_rounds: usize,
}

fn default_max_rounds() -> usize {
    8
}
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires a borrowed field"
)]
fn is_default_max_rounds(value: &usize) -> bool {
    *value == default_max_rounds()
}

fn model_node() -> String {
    "model".into()
}
fn supervised() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
pub struct Template {
    pub id: String,
    pub title: String,
    pub node_type: String,
    pub role: String,
    pub tags: Vec<String>,
    pub task: Value,
}

pub fn templates() -> Result<Vec<Template>> {
    serde_json::from_str(include_str!(
        "../../../integrations/model-router/workflow-templates.json"
    ))
    .context("职责模板资源无效")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub revision: u64,
    pub enabled: bool,
    pub mode: String,
    pub sources: BTreeMap<String, bool>,
    pub pool: Vec<PoolEntry>,
    pub workflows: Vec<Workflow>,
    #[serde(default = "inherit_profile")]
    pub call_profile: Value,
}

fn inherit_profile() -> Value {
    json!({"preset":"inherit"})
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            revision: 0,
            enabled: true,
            mode: "auto".into(),
            sources: BTreeMap::new(),
            pool: Vec::new(),
            workflows: Vec::new(),
            call_profile: inherit_profile(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Route {
    pub route_id: String,
    pub source: String,
    pub model: String,
    pub effort: String,
    pub output_limit: Option<u64>,
    pub pricing: Value,
    pub capabilities: Vec<String>,
    pub auth_hint: bool,
    pub dispatch_enabled: bool,
    pub selection_reason: Option<String>,
    pub connectivity: String,
    pub last_verified_at: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub settings: Settings,
    pub routes: Vec<Route>,
    pub sources: Vec<Value>,
    pub refreshed_at: u64,
    pub discovery_errors: Vec<Value>,
    pub supervisors: Vec<Value>,
    pub runs: Vec<Value>,
    pub check: Option<Value>,
    pub call_presets: Vec<Value>,
    pub effective_profile: Value,
}

impl Snapshot {
    /// Restore observation only; never resubmit a paid or interrupted run.
    pub fn active_run(&self, workflow_id: &str) -> Option<&str> {
        self.runs.iter().find_map(|run| {
            (run["workflow_id"].as_str() == Some(workflow_id)
                && matches!(
                    run["state"].as_str(),
                    Some("running" | "cancelling" | "awaiting_codex")
                ))
            .then(|| run["id"].as_str())
            .flatten()
        })
    }
}

pub fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Empty optional editors inherit defaults. Mounting a control is not an edit.
pub fn edit_parameter(task: &mut Value, key: &str, value: &str) -> bool {
    let Some(fields) = task.as_object_mut() else {
        return false;
    };
    if value.trim().is_empty() && !matches!(key, "prompt" | "kind") {
        return fields.remove(key).is_some();
    }
    let new = if matches!(key, "prompt" | "kind" | "output_mode" | "timeout_mode") {
        json!(value)
    } else if let Ok(number) = value.parse::<u64>() {
        json!(number)
    } else {
        return false;
    };
    if fields.get(key) == Some(&new) {
        return false;
    }
    fields.insert(key.to_owned(), new);
    true
}

impl Workflow {
    pub fn new(id: String) -> Self {
        Self {
            id,
            name: "定向工作流".into(),
            nodes: Vec::new(),
            edges: Vec::new(),
            token_budget: 200_000,
            max_parallel: 2,
            luna_supervision: true,
            max_rounds: default_max_rounds(),
        }
    }

    /// Stable ready-node ordering; no recursion or graph library is needed.
    pub fn order(&self) -> Result<Vec<usize>> {
        self.order_with_edges(&[])
    }

    fn order_with_edges(&self, additional: &[(usize, usize, EdgeKind)]) -> Result<Vec<usize>> {
        if self.nodes.is_empty()
            || self.nodes.len() > 256
            || self.edges.len() + additional.len() > 4096
            || !(1..=64).contains(&self.max_rounds)
        {
            bail!("工作流超出图结构协议范围");
        }
        let ids: BTreeMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        if ids.len() != self.nodes.len() {
            bail!("节点 ID 重复");
        }
        let mut degrees = vec![0usize; self.nodes.len()];
        let mut outgoing = vec![Vec::new(); self.nodes.len()];
        let mut seen = BTreeSet::new();
        let mut indexed = Vec::with_capacity(self.edges.len() + additional.len());
        for e in &self.edges {
            let (Some(&a), Some(&b)) = (ids.get(e.from.as_str()), ids.get(e.to.as_str())) else {
                bail!("连线引用了不存在的节点");
            };
            indexed.push((a, b, e.kind));
        }
        indexed.extend_from_slice(additional);
        for (a, b, kind) in indexed {
            if a >= self.nodes.len() || b >= self.nodes.len() {
                bail!("连线引用了不存在的节点");
            }
            if a == b || !seen.insert((a, b, kind)) {
                bail!("连线重复或节点连接了自身");
            }
            self.check_target(b, kind)?;
            if kind == EdgeKind::Task {
                degrees[b] += 1;
                outgoing[a].push(b);
            }
        }
        let mut ready: BTreeSet<usize> = degrees
            .iter()
            .enumerate()
            .filter_map(|(i, d)| (*d == 0).then_some(i))
            .collect();
        let mut result = Vec::new();
        while let Some(i) = ready.pop_first() {
            result.push(i);
            for &j in &outgoing[i] {
                degrees[j] -= 1;
                if degrees[j] == 0 {
                    ready.insert(j);
                }
            }
        }
        if result.len() != self.nodes.len() {
            bail!("检测到循环依赖");
        }
        // Reports are a separate directed plane. A task/report reciprocal pair
        // is legal, while a report-only cycle would endlessly recirculate work.
        let mut reports = vec![Vec::new(); self.nodes.len()];
        let mut report_degrees = vec![0usize; self.nodes.len()];
        for (a, b, kind) in seen {
            if kind == EdgeKind::Report {
                reports[a].push(b);
                report_degrees[b] += 1;
            }
        }
        let mut ready: Vec<usize> = report_degrees
            .iter()
            .enumerate()
            .filter_map(|(i, d)| (*d == 0).then_some(i))
            .collect();
        let mut visited = 0;
        while let Some(i) = ready.pop() {
            visited += 1;
            for &next in &reports[i] {
                report_degrees[next] -= 1;
                if report_degrees[next] == 0 {
                    ready.push(next);
                }
            }
        }
        if visited != self.nodes.len() {
            bail!("检测到回报连线循环");
        }
        self.validate_start_dependencies(&outgoing, &reports, &result)?;
        if self.nodes.iter().any(|n| {
            !n.x.is_finite()
                || !n.y.is_finite()
                || n.x < -10_000.0
                || n.y < -10_000.0
                || n.x > 10_000.0
                || n.y > 10_000.0
        }) {
            bail!("节点坐标无效");
        }
        Ok(result)
    }

    fn validate_start_dependencies(
        &self,
        tasks: &[Vec<usize>],
        reports: &[Vec<usize>],
        task_order: &[usize],
    ) -> Result<()> {
        let count = self.nodes.len();
        if reports.iter().flatten().all(|&target| {
            matches!(
                self.nodes[target].node_type.as_str(),
                "codex" | "operator" | "input"
            )
        }) {
            return Ok(());
        }
        let mut descendants = vec![vec![false; count]; count];
        for &node in task_order.iter().rev() {
            for &next in &tasks[node] {
                descendants[node][next] = true;
                let next_descendants = descendants[next].clone();
                for (current, reachable) in descendants[node].iter_mut().zip(next_descendants) {
                    *current |= reachable;
                }
            }
        }
        let mut effective = tasks.to_vec();
        for (source, targets) in reports.iter().enumerate() {
            for &target in targets {
                let kind = self.nodes[target].node_type.as_str();
                // Codex/operators receive feedback without waiting for it on
                // first dispatch. Reviewers must wait for every report; other
                // nodes wait only for reports outside their task descendants.
                if !matches!(kind, "codex" | "operator" | "input")
                    && (kind == "reviewer" || !descendants[target][source])
                    && !effective[source].contains(&target)
                {
                    effective[source].push(target);
                }
            }
        }
        let mut degrees = vec![0usize; count];
        for targets in &effective {
            for &target in targets {
                degrees[target] += 1;
            }
        }
        let mut ready: Vec<usize> = degrees
            .iter()
            .enumerate()
            .filter_map(|(i, degree)| (*degree == 0).then_some(i))
            .collect();
        let mut visited = 0;
        while let Some(node) = ready.pop() {
            visited += 1;
            for &next in &effective[node] {
                degrees[next] -= 1;
                if degrees[next] == 0 {
                    ready.push(next);
                }
            }
        }
        if visited != count {
            bail!("任务与回报启动前置形成循环，节点无法开始执行");
        }
        Ok(())
    }

    pub fn add_route(&mut self, route: &Route, x: f64, y: f64) -> Result<usize> {
        if self.nodes.len() >= 256 {
            bail!("最多 256 个节点，实际执行还受配置档约束");
        }
        let mut sequence = self.nodes.len() + 1;
        while self
            .nodes
            .iter()
            .any(|n| n.id == format!("node-{sequence}"))
        {
            sequence += 1;
        }
        self.nodes.push(Node { id: format!("node-{sequence}"), title: route.model.clone(), route_id: route.route_id.clone(), node_type: "model".into(), role: String::new(), fallback_routes: Vec::new(), tags: Vec::new(),
            x: x.clamp(-10_000.0, 10_000.0), y: y.clamp(-10_000.0, 10_000.0),
            task: json!({"kind":"extract","prompt":"分析输入材料，返回完整且有证据的 JSON 报告。","max_output_tokens":8192,"token_budget":48000,"deadline_s":300,"hard_deadline_s":900}) });
        Ok(self.nodes.len() - 1)
    }

    /// Codex mother node waits for the real caller to plan, confirm corrections
    /// and accept work. The router never pretends to start Codex itself.
    pub fn add_master(&mut self, x: f64, y: f64) -> Result<usize> {
        if self.nodes.len() >= 256 {
            bail!("最多 256 个节点，实际执行还受配置档约束");
        }
        let mut sequence = self.nodes.len() + 1;
        while self
            .nodes
            .iter()
            .any(|n| n.id == format!("node-{sequence}"))
        {
            sequence += 1;
        }
        self.nodes.push(Node {
            id: format!("node-{sequence}"),
            title: "母节点 · Codex 接口".into(),
            route_id: String::new(),
            node_type: "codex".into(),
            role: "真实 Codex 调用方：检查完整拓扑、切片分配、确认修正并验收下游工作。".into(),
            fallback_routes: Vec::new(),
            tags: vec!["母节点".into()],
            x: x.clamp(-10_000.0, 10_000.0),
            y: y.clamp(-10_000.0, 10_000.0),
            task: json!({"kind":"extract","prompt":"提供输入材料。"}),
        });
        Ok(self.nodes.len() - 1)
    }

    /// Read-only connection preview; never clones prompts or edits the graph.
    pub fn can_connect(&self, from: usize, to: usize) -> Result<()> {
        self.can_connect_kind(from, to, EdgeKind::Task)
    }

    fn check_target(&self, to: usize, kind: EdgeKind) -> Result<()> {
        let node = self.nodes.get(to).context("目标节点不存在")?;
        if node.node_type == "input" || (kind == EdgeKind::Task && node.node_type == "codex") {
            bail!("输入和母节点不接受任务线；回报线可进入 Codex 母节点");
        }
        Ok(())
    }

    pub fn can_connect_kind(&self, from: usize, to: usize, kind: EdgeKind) -> Result<()> {
        self.order_with_edges(&[(from, to, kind)])?;
        Ok(())
    }

    pub fn connect(&mut self, from: usize, to: usize) -> Result<()> {
        self.connect_kind(from, to, EdgeKind::Task)
    }

    pub fn connect_kind(&mut self, from: usize, to: usize, kind: EdgeKind) -> Result<()> {
        self.can_connect_kind(from, to, kind)?;
        let a = self.nodes.get(from).context("起始节点不存在")?.id.clone();
        let b = self.nodes.get(to).context("目标节点不存在")?.id.clone();
        self.edges.push(Edge {
            from: a,
            to: b,
            kind,
        });
        Ok(())
    }

    pub fn can_connect_bidirectional(&self, from: usize, to: usize) -> Result<()> {
        self.order_with_edges(&[(from, to, EdgeKind::Task), (to, from, EdgeKind::Report)])?;
        Ok(())
    }

    pub fn connect_bidirectional(&mut self, from: usize, to: usize) -> Result<()> {
        self.can_connect_bidirectional(from, to)?;
        let (a, b) = (self.nodes[from].id.clone(), self.nodes[to].id.clone());
        self.edges.push(Edge {
            from: a.clone(),
            to: b.clone(),
            kind: EdgeKind::Task,
        });
        self.edges.push(Edge {
            from: b,
            to: a,
            kind: EdgeKind::Report,
        });
        Ok(())
    }

    pub fn remove(&mut self, index: usize) {
        if index >= self.nodes.len() {
            return;
        }
        let node = self.nodes.remove(index);
        self.edges.retain(|e| e.from != node.id && e.to != node.id);
    }

    /// Removes every edge touching the node; the node itself stays.
    pub fn disconnect(&mut self, index: usize) {
        if let Some(node) = self.nodes.get(index) {
            let id = node.id.clone();
            self.edges.retain(|e| e.from != id && e.to != id);
        }
    }

    /// The same rectangles are used by Direct2D painting and pointer hit tests.
    pub fn hit(&self, x: f64, y: f64) -> Option<usize> {
        self.nodes
            .iter()
            .rposition(|n| (n.x..=n.x + 220.0).contains(&x) && (n.y..=n.y + 112.0).contains(&y))
    }

    /// Fit rectangles in a viewport, retaining a margin around all nodes.
    pub fn fit_view(&self, width: f64, height: f64) -> ((f64, f64), f64) {
        if self.nodes.is_empty() || !width.is_finite() || !height.is_finite() {
            return ((24.0, 24.0), 1.0);
        }
        let min_x = self.nodes.iter().map(|n| n.x).fold(f64::INFINITY, f64::min);
        let min_y = self.nodes.iter().map(|n| n.y).fold(f64::INFINITY, f64::min);
        let max_x = self
            .nodes
            .iter()
            .map(|n| n.x + 220.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let max_y = self
            .nodes
            .iter()
            .map(|n| n.y + 112.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let zoom = (((width - 48.0).max(1.0) / (max_x - min_x))
            .min((height - 48.0).max(1.0) / (max_y - min_y)))
        .clamp(0.02, 1.8);
        (
            (
                (width - (max_x - min_x) * zoom) / 2.0 - min_x * zoom,
                (height - (max_y - min_y) * zoom) / 2.0 - min_y * zoom,
            ),
            zoom,
        )
    }

    pub fn add_template(&mut self, template: &Template, route: Option<&Route>) -> Result<usize> {
        let fallback = Route::default();
        let index = self.add_route(
            route.unwrap_or(&fallback),
            40.0 + (self.nodes.len() % 3) as f64 * 250.0,
            40.0 + (self.nodes.len() / 3) as f64 * 160.0,
        )?;
        self.apply_template(index, template)?;
        Ok(index)
    }

    pub fn apply_template(&mut self, index: usize, template: &Template) -> Result<()> {
        let node = self.nodes.get_mut(index).context("未选择节点")?;
        node.title.clone_from(&template.title);
        node.node_type.clone_from(&template.node_type);
        node.role.clone_from(&template.role);
        node.tags.clone_from(&template.tags);
        if !node.is_model_backed() {
            node.route_id.clear();
            node.fallback_routes.clear();
        }
        if let (Some(target), Some(values)) = (node.task.as_object_mut(), template.task.as_object())
        {
            target.extend(values.clone());
        }
        Ok(())
    }

    pub fn chain(&mut self) {
        let codex: Vec<&Node> = self
            .nodes
            .iter()
            .filter(|node| node.node_type == "codex")
            .collect();
        let inputs: Vec<&Node> = self
            .nodes
            .iter()
            .filter(|node| node.node_type == "input")
            .collect();
        let entry = if codex.len() == 1 {
            codex.first().copied()
        } else if codex.is_empty() && inputs.len() == 1 {
            inputs.first().copied()
        } else {
            None
        };
        let ordered: Vec<&Node> = entry
            .into_iter()
            .chain(
                self.nodes
                    .iter()
                    .filter(|node| !matches!(node.node_type.as_str(), "codex" | "input")),
            )
            .collect();
        self.edges.retain(|edge| edge.kind == EdgeKind::Report);
        self.edges.extend(ordered.windows(2).map(|p| Edge {
            from: p[0].id.clone(),
            to: p[1].id.clone(),
            kind: EdgeKind::Task,
        }));
    }
}

pub fn endpoint_path() -> PathBuf {
    crate::external_interface::settings_path()
        .with_file_name("router")
        .join("daemon-endpoint.json")
}

/// Start an existing installation, without downloading runtimes or credentials.
pub fn ensure_running() -> Result<()> {
    if call("router_scheduling", json!({})).is_ok() {
        return Ok(());
    }
    let directory = endpoint_path()
        .parent()
        .context("协调器目录不存在")?
        .to_path_buf();
    let config = directory.join("config.json");
    if !config.is_file() {
        bail!("尚未配置模型协调器，请先完成协调器安装");
    }
    if let Ok(bytes) = std::fs::read(endpoint_path())
        && let Ok(endpoint) = serde_json::from_slice::<Value>(&bytes)
        && let Some(port) = endpoint["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
    {
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        if std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_ok() {
            bail!("现有协调器仍在运行但管理接口不可用，请先更新协调器；未启动重复服务");
        }
    }
    let exe = std::env::current_exe()?;
    let parent = exe.parent().context("程序目录无效")?;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .context("开发目录无效")?;
    let candidates = [
        parent.join("model-router/router.py"),
        parent.join("integrations/model-router/router.py"),
        root.join("integrations/model-router/router.py"),
    ];
    let entry = candidates
        .into_iter()
        .find(|p| p.is_file())
        .context("找不到已安装的 model-router/router.py")?;
    let mut command = std::process::Command::new(if cfg!(windows) { "python" } else { "python3" });
    // LocalClient uses the shared startup lock; simultaneous UI windows must
    // not create two services against the same state database.
    command
        .arg(entry)
        .arg("call")
        .arg("--method")
        .arg("router_scheduling")
        .arg("--config")
        .arg(config)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let _child = command
        .spawn()
        .context("协调器启动失败，请检查已安装的 Python")?;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(200));
        if call("router_scheduling", json!({})).is_ok() {
            return Ok(());
        }
    }
    bail!("协调器未在期限内就绪，请检查本机服务状态")
}

/// Sends only to an authenticated loopback endpoint owned by the local router.
/// The endpoint token is never placed in process arguments, errors, or logs.
pub fn call(method: &str, params: Value) -> Result<Value> {
    let (method, params, legacy_plan) = adapt_legacy_workflow_call(method, params)?;
    let operator_workflow = requires_operator_workflow(method, &params);
    if method == "router_dispatch" || operator_workflow {
        verify_dispatch_runtime(operator_workflow)?;
    }
    let (port, token, agent) = local_client(Duration::from_secs(150))?;
    let path = if method == "router_scheduling"
        || matches!(method, "router_workflow_status" | "router_workflow_cancel")
        || (method == "router_dispatch"
            && matches!(
                params["action"].as_str(),
                Some("status" | "collect" | "cancel" | "list" | "report" | "topology" | "events")
            )) {
        "manage"
    } else {
        "rpc"
    };
    let mut response = agent
        .post(&format!("http://127.0.0.1:{port}/{path}"))
        .header("Authorization", &format!("Bearer {token}"))
        .send_json(json!({"method":method,"params":params}))
        .map_err(|_| {
            anyhow::anyhow!("协调器请求失败，请核对服务状态；未知结果请先查询，避免重复提交")
        })?;
    let result: Value = response
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_json()
        .context("协调器响应无效")?;
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        bail!("{error}");
    }
    let result = result.get("result").cloned().context("协调器未返回结果")?;
    if legacy_plan {
        return result
            .get("plan")
            .cloned()
            .context("协调器未返回 workflow plan");
    }
    Ok(result)
}

fn adapt_legacy_workflow_call(method: &str, params: Value) -> Result<(&str, Value, bool)> {
    if !matches!(method, "router_workflow_plan" | "router_workflow_submit") {
        return Ok((method, params, false));
    }
    let action = if method == "router_workflow_plan" {
        "plan"
    } else {
        "submit"
    };
    let definition = params
        .get("definition")
        .cloned()
        .context("旧 workflow 调用缺少 definition")?;
    let mut request = json!({
        "kind": "workflow",
        "definition": definition,
        "workflow_id": params.get("workflow_id").cloned().unwrap_or_else(|| json!("default"))
    });
    if action == "submit" {
        let request_id = params
            .get("request_id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .context("旧 workflow submit 必须提供非空 request_id")?;
        request["input_text"] = params
            .get("input_text")
            .cloned()
            .unwrap_or_else(|| json!(""));
        if let Some(execution) = params.get("execution") {
            request["execution"] = execution.clone();
        }
        let mut dispatch_params = json!({
            "request": request,
            "request_id": request_id
        });
        if let Some(acknowledge) = params.get("acknowledge_unknown_risk") {
            dispatch_params["acknowledge_unknown_risk"] = acknowledge.clone();
        }
        dispatch_params["action"] = json!(action);
        Ok(("router_dispatch", dispatch_params, false))
    } else {
        Ok((
            "router_dispatch",
            json!({"action": action, "request": request}),
            true,
        ))
    }
}

fn local_client(timeout: Duration) -> Result<(u16, String, ureq::Agent)> {
    let file = std::fs::File::open(endpoint_path()).context("协调器未运行，请先启动模型协调器")?;
    let mut body = Vec::new();
    file.take(4097).read_to_end(&mut body)?;
    if body.len() > 4096 {
        bail!("协调器端点文件无效");
    }
    let endpoint: Value = serde_json::from_slice(&body).context("协调器端点文件无效")?;
    let port = endpoint["port"]
        .as_u64()
        .filter(|p| (1024..=65535).contains(p))
        .and_then(|p| u16::try_from(p).ok())
        .context("协调器端口无效")?;
    let token = endpoint["token"]
        .as_str()
        .filter(|s| {
            (16..=512).contains(&s.len())
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .context("协调器认证元数据无效")?
        .to_owned();
    let agent = ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(timeout))
        .build()
        .new_agent();
    Ok((port, token, agent))
}

/// Call the versioned dispatch facade after proving that the resident service
/// advertises the protocol this client needs. Management actions stay on
/// `/manage`, so they remain available for reconciliation when dispatch is off.
pub fn dispatch(action: &str, params: Value) -> Result<Value> {
    if !matches!(
        action,
        "plan"
            | "submit"
            | "status"
            | "collect"
            | "cancel"
            | "list"
            | "report"
            | "coordinate"
            | "topology"
            | "events"
    ) {
        bail!("未知调度操作: {action}");
    }
    let mut params = params;
    let fields = params
        .as_object_mut()
        .context("调度参数顶层必须是 JSON 对象")?;
    fields.insert("action".into(), json!(action));
    call("router_dispatch", params)
}

fn requires_operator_workflow(method: &str, params: &Value) -> bool {
    fn graph_uses_operator(value: &Value) -> bool {
        value.get("max_rounds").is_some()
            || value["nodes"].as_array().is_some_and(|nodes| {
                nodes.iter().any(|n| {
                    matches!(
                        n["node_type"].as_str(),
                        Some("codex" | "operator" | "reviewer")
                    )
                })
            })
            || value["edges"]
                .as_array()
                .is_some_and(|edges| edges.iter().any(|e| e["kind"].as_str() == Some("report")))
    }
    if method == "router_dispatch" {
        params["action"].as_str() == Some("coordinate")
            || graph_uses_operator(&params["request"]["definition"])
    } else {
        method == "router_scheduling"
            && params["value"]["workflows"]
                .as_array()
                .is_some_and(|values| values.iter().any(graph_uses_operator))
    }
}

fn verify_dispatch_runtime(operator_workflow: bool) -> Result<()> {
    let (port, token, agent) = local_client(Duration::from_secs(10))?;
    let health: Value = agent
        .get(&format!("http://127.0.0.1:{port}/health"))
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .context("协调器健康握手失败")?
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_json()
        .context("协调器健康响应无效")?;
    if health["runtime_contract_version"].as_u64() != Some(2)
        || !health["features"].as_array().is_some_and(|features| {
            features
                .iter()
                .any(|feature| feature.as_str() == Some("dispatch_v1"))
        })
    {
        bail!("协调器缺少 dispatch_v1 或 runtime_contract_version=2，请更新服务后重试");
    }
    if operator_workflow
        && !health["features"].as_array().is_some_and(|features| {
            features
                .iter()
                .any(|feature| feature.as_str() == Some("operator_workflow_v1"))
        })
    {
        bail!("协调器缺少 operator_workflow_v1；操作员和回报拓扑未派发，请更新服务后重试");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restore_only_observes_an_active_run_in_the_current_workflow() {
        let snapshot = Snapshot {
            runs: vec![
                json!({"id":"other","workflow_id":"other","state":"running"}),
                json!({"id":"unknown","workflow_id":"round","state":"interrupted"}),
                json!({"id":"owned","workflow_id":"round","state":"cancelling"}),
                json!({"id":"decision","workflow_id":"review","state":"awaiting_codex"}),
            ],
            ..Snapshot::default()
        };
        assert_eq!(snapshot.active_run("round"), Some("owned"));
        assert_eq!(snapshot.active_run("missing"), None);
        assert_eq!(snapshot.active_run("review"), Some("decision"));
    }
    fn sample() -> Workflow {
        let mut g = Workflow::new("test".into());
        let route = Route {
            route_id: "route".into(),
            model: "model".into(),
            ..Route::default()
        };
        for i in 0..3 {
            g.add_route(&route, f64::from(i) * 250.0, 20.0).unwrap();
        }
        g
    }
    #[test]
    fn legacy_workflow_calls_use_dispatch_v1_payloads_and_require_submit_id() {
        let definition = json!({"id":"wf","nodes":[],"edges":[]});
        let (method, plan, unwrap_plan) = adapt_legacy_workflow_call(
            "router_workflow_plan",
            json!({"definition":definition.clone(),"workflow_id":"team"}),
        )
        .unwrap();
        assert_eq!(method, "router_dispatch");
        assert!(unwrap_plan);
        assert_eq!(plan["action"], "plan");
        assert_eq!(plan["request"]["kind"], "workflow");
        assert_eq!(plan["request"]["workflow_id"], "team");

        let (method, submit, unwrap_plan) = adapt_legacy_workflow_call(
            "router_workflow_submit",
            json!({
                "definition":definition.clone(),
                "workflow_id":"team",
                "input_text":"input",
                "request_id":"stable-id",
                "acknowledge_unknown_risk":true
            }),
        )
        .unwrap();
        assert_eq!(method, "router_dispatch");
        assert!(!unwrap_plan);
        assert_eq!(submit["action"], "submit");
        assert_eq!(submit["request_id"], "stable-id");
        assert_eq!(submit["acknowledge_unknown_risk"], true);
        assert_eq!(submit["request"]["input_text"], "input");
        assert!(
            adapt_legacy_workflow_call("router_workflow_submit", json!({"definition":definition}))
                .is_err()
        );
    }

    #[test]
    fn graph_order_rejects_cycles_and_keeps_previous_edges() {
        let mut g = sample();
        g.connect(2, 0).unwrap();
        g.connect(0, 1).unwrap();
        assert_eq!(g.order().unwrap(), vec![2, 0, 1]);
        assert!(g.connect(1, 2).is_err());
        assert_eq!(g.edges.len(), 2);
    }
    #[test]
    fn report_plane_allows_reciprocal_links_but_rejects_its_own_cycles() {
        let mut graph = sample();
        graph.connect(0, 1).unwrap();
        graph.connect_kind(1, 0, EdgeKind::Report).unwrap();
        assert_eq!(graph.order().unwrap(), vec![0, 1, 2]);
        let before = graph.edges.clone();
        assert!(graph.connect_kind(0, 1, EdgeKind::Report).is_err());
        assert_eq!(graph.edges, before);
        assert!(graph.connect(1, 0).is_err());
    }

    #[test]
    fn a_reviewer_must_not_wait_for_a_report_from_its_unstarted_task_child() {
        let mut graph = sample();
        graph.nodes[0].node_type = "reviewer".into();
        graph.nodes[1].node_type = "operator".into();
        let master = graph.add_master(10.0, 10.0).unwrap();
        graph.connect(master, 0).unwrap();
        graph.connect(0, 1).unwrap();
        let before = graph.edges.clone();
        assert!(graph.can_connect_kind(1, 0, EdgeKind::Report).is_err());
        assert!(graph.connect_kind(1, 0, EdgeKind::Report).is_err());
        assert_eq!(graph.edges, before);
        graph.edges.push(Edge {
            from: graph.nodes[1].id.clone(),
            to: graph.nodes[0].id.clone(),
            kind: EdgeKind::Report,
        });
        assert!(graph.order().is_err());
    }

    #[test]
    fn an_operator_task_chain_does_not_wait_for_feedback_before_first_dispatch() {
        let mut graph = sample();
        graph.nodes[0].node_type = "operator".into();
        graph.nodes[1].node_type = "operator".into();
        graph.connect_bidirectional(0, 1).unwrap();
        assert_eq!(graph.order().unwrap(), vec![0, 1, 2]);
        // Generic models also do not wait for a task descendant's feedback.
        graph.nodes[0].node_type = "model".into();
        assert_eq!(graph.order().unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn codex_accepts_report_chains_and_bidirectional_changes_are_atomic() {
        let mut graph = sample();
        let master = graph.add_master(10.0, 10.0).unwrap();
        graph.connect_bidirectional(master, 0).unwrap();
        graph.connect_kind(0, 1, EdgeKind::Report).unwrap();
        graph.connect_kind(1, master, EdgeKind::Report).unwrap();
        assert!(graph.order().is_ok());
        assert!(graph.can_connect(0, master).is_err());
        let mut incompatible = sample();
        incompatible.connect_kind(0, 1, EdgeKind::Report).unwrap();
        let before = incompatible.edges.clone();
        assert!(incompatible.connect_bidirectional(0, 1).is_err());
        assert_eq!(incompatible.edges, before);
    }

    #[test]
    fn legacy_graphs_omit_default_fields_and_chain_keeps_report_edges() {
        let mut graph = sample();
        graph.connect_kind(2, 0, EdgeKind::Report).unwrap();
        graph.chain();
        assert_eq!(
            graph
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Report)
                .count(),
            1
        );
        let value = serde_json::to_value(&graph).unwrap();
        assert!(value.get("max_rounds").is_none());
        assert!(value["edges"][1].get("kind").is_none());
        assert_eq!(value["edges"][0]["kind"], "report");
        let restored: Workflow = serde_json::from_value(value).unwrap();
        assert_eq!(restored.max_rounds, 8);
        assert_eq!(restored.edges[1].kind, EdgeKind::Task);
        graph.max_rounds = 0;
        assert!(graph.order().is_err());
    }

    #[test]
    fn operator_templates_keep_exact_model_bindings() {
        let mut graph = sample();
        for kind in ["operator", "reviewer"] {
            let template = Template {
                id: kind.into(),
                title: kind.into(),
                node_type: kind.into(),
                role: String::new(),
                tags: Vec::new(),
                task: json!({"kind":"extract"}),
            };
            graph.apply_template(0, &template).unwrap();
            assert!(graph.nodes[0].is_model_backed());
            assert_eq!(graph.nodes[0].route_id, "route");
        }
    }

    #[test]
    fn chain_promotes_a_late_unique_mother_without_choosing_among_multiple_mothers() {
        let mut graph = sample();
        let master = graph.add_master(500.0, 10.0).unwrap();
        graph.connect_kind(2, master, EdgeKind::Report).unwrap();
        graph.chain();
        assert_eq!(graph.order().unwrap(), vec![master, 0, 1, 2]);
        assert!(graph.edges.iter().any(|e| e.from == graph.nodes[master].id
            && e.to == graph.nodes[0].id
            && e.kind == EdgeKind::Task));
        assert!(
            graph
                .edges
                .iter()
                .any(|e| e.to == graph.nodes[master].id && e.kind == EdgeKind::Report)
        );
        graph.add_master(750.0, 10.0).unwrap();
        graph.chain();
        assert!(
            graph
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Task)
                .all(|e| e.from != graph.nodes[master].id)
        );
        let mut legacy = sample();
        let input = legacy.add_master(500.0, 10.0).unwrap();
        legacy.nodes[input].node_type = "input".into();
        legacy.chain();
        assert_eq!(legacy.order().unwrap(), vec![input, 0, 1, 2]);
    }

    #[test]
    fn advanced_graphs_and_coordination_require_the_operator_feature() {
        for definition in [
            json!({"nodes":[{"node_type":"codex"}]}),
            json!({"nodes":[{"node_type":"operator"}]}),
            json!({"nodes":[{"node_type":"reviewer"}]}),
            json!({"edges":[{"from":"a","to":"b","kind":"report"}]}),
            json!({"max_rounds":4}),
        ] {
            assert!(requires_operator_workflow(
                "router_dispatch",
                &json!({"action":"submit","request":{"definition":definition}})
            ));
        }
        assert!(requires_operator_workflow(
            "router_dispatch",
            &json!({"action":"coordinate"})
        ));
        assert!(!requires_operator_workflow(
            "router_dispatch",
            &json!({"action":"status"})
        ));
        assert!(!requires_operator_workflow(
            "router_dispatch",
            &json!({"action":"submit","request":{"definition":{"nodes":[{"node_type":"model"}]}}})
        ));
    }
    #[test]
    fn removal_prunes_edges_and_hit_testing_matches_geometry() {
        let mut g = sample();
        g.chain();
        assert_eq!(g.hit(255.0, 25.0), Some(1));
        g.remove(1);
        assert_eq!(g.edges, [] as [Edge; 0]);
        assert_eq!(g.order().unwrap().len(), 2);
        assert_eq!(g.hit(-1.0, 10.0), None);
    }
    #[test]
    fn graph_json_round_trip_preserves_custom_parameters_and_tags() {
        let mut g = sample();
        g.nodes[0].tags.push("资料核查".into());
        g.nodes[0].task["idle_s"] = json!(240);
        let copy: Workflow = serde_json::from_value(serde_json::to_value(&g).unwrap()).unwrap();
        assert_eq!(copy.nodes[0].task["idle_s"], 240);
        assert_eq!(copy.nodes[0].tags, vec!["资料核查"]);
    }

    #[test]
    fn optional_monitor_templates_and_legacy_graph_defaults_stay_compatible() {
        let mut g = sample();
        let templates = templates().unwrap();
        assert!(templates.iter().any(|t| t.id == "support"));
        for template in &templates {
            g.apply_template(0, template).unwrap();
            g.order().unwrap();
        }
        let mut value = serde_json::to_value(&g).unwrap();
        value.as_object_mut().unwrap().remove("luna_supervision");
        assert!(
            serde_json::from_value::<Workflow>(value)
                .unwrap()
                .luna_supervision
        );
        g.luna_supervision = false;
        assert!(
            !serde_json::from_value::<Workflow>(serde_json::to_value(g).unwrap())
                .unwrap()
                .luna_supervision
        );
    }
    #[test]
    fn opening_optional_editors_preserves_defaults_and_clearing_an_override_inherits() {
        let mut task = json!({"kind":"extract","prompt":"evidence","max_output_tokens":8192});
        let prior = task.clone();
        for key in ["output_mode", "idle_s", "ttft_s", "report_max_chars"] {
            assert!(!edit_parameter(&mut task, key, ""));
        }
        assert!(!edit_parameter(&mut task, "max_output_tokens", "8192"));
        assert_eq!(task, prior);
        assert!(edit_parameter(&mut task, "output_mode", "adaptive"));
        assert!(edit_parameter(&mut task, "output_mode", ""));
        assert_eq!(task, prior);
    }
    #[test]
    fn fit_keeps_nodes_visible_and_hit_testing_correct_after_pan_and_zoom() {
        let mut graph = sample();
        for node in &mut graph.nodes {
            node.x -= 500.0;
            node.y -= 300.0;
        }
        graph.order().unwrap();
        for (width, height) in [(320.0, 240.0), (1200.0, 800.0)] {
            let ((x, y), zoom) = graph.fit_view(width, height);
            for node in &graph.nodes {
                assert!(x + node.x * zoom >= 23.9 && y + node.y * zoom >= 23.9);
                assert!(
                    x + (node.x + 220.0) * zoom <= width - 23.9
                        && y + (node.y + 112.0) * zoom <= height - 23.9
                );
                let screen = (x + (node.x + 10.0) * zoom, y + (node.y + 10.0) * zoom);
                assert_eq!(
                    graph.hit((screen.0 - x) / zoom, (screen.1 - y) / zoom),
                    graph.hit(node.x + 10.0, node.y + 10.0)
                );
            }
        }
    }
    #[test]
    fn connection_preview_rejects_cycles_and_duplicates_without_mutating_the_graph() {
        let mut g = sample();
        g.connect(0, 1).unwrap();
        g.connect(1, 2).unwrap();
        let before = g.edges.clone();
        assert!(g.can_connect(2, 0).is_err());
        assert!(g.can_connect(0, 1).is_err());
        assert!(g.can_connect(0, 0).is_err());
        assert!(g.can_connect(0, 99).is_err());
        assert_eq!(g.edges, before);
        assert!(g.can_connect(0, 2).is_ok());
        assert_eq!(g.edges, before);
        g.connect(0, 2).unwrap();
        assert_eq!(g.edges.len(), 3);
    }
}
