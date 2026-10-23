"""User-owned dispatch controls and bounded visual DAGs; no credentials or tools."""

import copy
import hashlib
import json
import math
import os
from pathlib import Path
import threading
import time
import uuid

from .controls import DispatchBlocked
from .models import Task, now_ms, reject_secrets

FILE_NAME = "scheduling.json"
DEFAULT = {"revision": 0, "enabled": True, "mode": "auto", "sources": {}, "pool": [], "workflows": [], "call_profile": {"preset":"inherit"}}
TERMINAL = {"succeeded", "failed", "blocked", "unknown", "cancelled", "interrupted"}
INFORMATION_PROMPT = "\n证据不足时报告 information_requests:[{question,reason}]。补充已有请求时，仅用上下文提供的准确 ID 返回 information_support:[{request_id,summary,evidence}]；每个问题分别绑定，不得将缺失证据视为已解决。"


def text(value, name, maximum=200, empty=False):
    if not isinstance(value, str) or len(value) > maximum or (not empty and not value.strip()):
        raise ValueError(name + " must be bounded text")
    reject_secrets(value)
    return value


def integer(value, name, low, high):
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        raise ValueError(name + " is outside its integer range")
    return value


def labels(values):
    if not isinstance(values, list) or len(values) > 12:
        raise ValueError("tags must be a list of at most 12 labels")
    return list(dict.fromkeys(text(v, "tag", 40) for v in values))


def graph(value, *, allow_draft=False):
    if not isinstance(value, dict) or set(value) - {"id", "name", "nodes", "edges", "token_budget", "max_parallel", "luna_supervision"}:
        raise ValueError("Invalid workflow fields")
    result = {"id": text(value.get("id"), "workflow id"), "name": text(value.get("name"), "workflow name", 100),
              "token_budget": integer(value.get("token_budget", 200000), "workflow budget", 1000, 100000000),
              "max_parallel": integer(value.get("max_parallel", 2), "workflow parallel", 1, 32),
              "luna_supervision": value.get("luna_supervision", True), "nodes": [], "edges": []}
    if not isinstance(result["luna_supervision"], bool):
        raise ValueError("luna_supervision must be a boolean")
    nodes, edges = value.get("nodes"), value.get("edges", [])
    if not isinstance(nodes, list) or not (0 if allow_draft else 1) <= len(nodes) <= 256 or not isinstance(edges, list) or len(edges) > 4096:
        raise ValueError("Workflow exceeds the 256-node / 4096-edge protocol envelope")
    ids = set()
    for node in nodes:
        if not isinstance(node, dict) or set(node) - {"id", "title", "route_id", "tags", "x", "y", "task", "node_type", "role", "fallback_routes"}:
            raise ValueError("Invalid workflow node fields")
        ident = text(node.get("id"), "node id")
        if ident in ids:
            raise ValueError("Duplicate node id")
        ids.add(ident)
        node_type = node.get("node_type", "model")
        if node_type not in {"model", "input", "merge", "gate", "output"}:
            raise ValueError("Unsupported workflow node type")
        fallbacks=node.get("fallback_routes",[])
        if not isinstance(fallbacks,list) or len(fallbacks)>8:raise ValueError("At most eight explicit fallback routes per node")
        fallbacks=list(dict.fromkeys(text(r,"fallback route") for r in fallbacks))
        task = node.get("task", {})
        allowed = {"kind", "prompt", "needs_web", "min_quality", "deadline_s", "hard_deadline_s", "ttft_s", "idle_s",
                   "stall_grace_s", "timeout_mode", "max_output_tokens", "output_mode", "token_budget", "report_max_chars", "queue_wait_s", "max_attempts", "allow_fallback", "fanout", "verify"}
        if not isinstance(task, dict) or set(task) - allowed:
            raise ValueError("Node task contains unsupported fields")
        from .profiles import ABS_LIMITS
        parsed = Task.parse({"kind": "extract", "prompt": "Complete the supplied material.", **task}, runtime_limits=ABS_LIMITS)
        if len(parsed.prompt) > ABS_LIMITS["max_node_prompt_chars"]:
            raise ValueError("Node prompt exceeds protocol envelope")
        pos = {}
        for key in ["x", "y"]:
            v = node.get(key, 60)
            if isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v) or not -10000 <= v <= 10000:
                raise ValueError("Node positions must be finite canvas coordinates")
            pos[key] = v
        result["nodes"].append({"id": ident, "title": text(node.get("title", ident), "node title", 100),
            "route_id": text(node.get("route_id", ""), "route id", empty=allow_draft or node_type != "model"),
            "node_type": node_type, "role": text(node.get("role", ""), "node role", ABS_LIMITS["max_node_role_chars"], empty=True),
            "fallback_routes":fallbacks,
            "tags": labels(node.get("tags", [])), **pos,
            "task": {"kind": parsed.kind, "prompt": parsed.prompt, **copy.deepcopy(task)}})
    seen = set()
    for edge in edges:
        if not isinstance(edge, dict) or set(edge) != {"from", "to"}:
            raise ValueError("Edges require from and to")
        pair = (edge["from"], edge["to"])
        if any(not isinstance(v, str) or v not in ids for v in pair) or pair[0] == pair[1] or pair in seen:
            raise ValueError("Invalid or duplicate workflow edge")
        seen.add(pair)
        result["edges"].append(dict(edge))
    outgoing, degrees = {n: [] for n in ids}, dict.fromkeys(ids, 0)
    for a, b in seen:
        outgoing[a].append(b)
        degrees[b] += 1
    ready = [n for n in ids if not degrees[n]]
    visited = 0
    while ready:
        visited += 1
        for n in outgoing[ready.pop()]:
            degrees[n] -= 1
            if not degrees[n]:
                ready.append(n)
    if visited != len(ids):
        raise ValueError("Workflow contains a cycle")
    return result


def validate(value):
    if not isinstance(value, dict) or set(value) - set(DEFAULT):
        raise ValueError("Invalid scheduling settings")
    result = copy.deepcopy(DEFAULT) | value
    from .profiles import validate as validate_profile
    result["call_profile"] = validate_profile(result["call_profile"])
    integer(result["revision"], "revision", 0, 2**53 - 1)
    if not isinstance(result["enabled"], bool) or result["mode"] not in {"auto", "directed"}:
        raise ValueError("Invalid scheduling switch or mode")
    sources = result["sources"]
    if not isinstance(sources, dict) or len(sources) > 64 or any(not isinstance(v, bool) for v in sources.values()):
        raise ValueError("Invalid source switches")
    for key in sources:
        text(key, "source")
    if not isinstance(result["pool"], list) or len(result["pool"]) > 128:
        raise ValueError("Model pool exceeds 128 entries")
    pool, seen = [], set()
    for row in result["pool"]:
        if not isinstance(row, dict) or set(row) - {"route_id", "expires_at", "tags"}:
            raise ValueError("Invalid pool entry")
        ident = text(row.get("route_id"), "pool route")
        if ident in seen:
            raise ValueError("Duplicate pool route")
        seen.add(ident)
        pool.append({"route_id": ident, "expires_at": integer(row.get("expires_at"), "pool expiry", 1, 2**53 - 1), "tags": labels(row.get("tags", []))})
    result["pool"] = pool
    if not isinstance(result["workflows"], list) or len(result["workflows"]) > 16:
        raise ValueError("At most 16 workflows may be stored")
    result["workflows"] = [graph(v, allow_draft=True) for v in result["workflows"]]
    if len({g["id"] for g in result["workflows"]}) != len(result["workflows"]):
        raise ValueError("Duplicate workflow id")
    return result


def settings(store, *, selector_only=False):
    def view(value):
        return copy.deepcopy({k: value[k] for k in ["enabled", "mode", "sources", "pool", "call_profile"]} if selector_only else value)
    try:
        path = store.path.parent / FILE_NAME
        if not path.exists():
            return view(DEFAULT)
        stat = path.stat()
        stamp = (stat.st_mtime_ns, stat.st_size, stat.st_ino)
        cached = getattr(store, "_scheduling_cache", None)
        if cached and cached[0] == stamp:
            return view(cached[1])
        with path.open("rb") as f:
            body = f.read(1048577)
        if len(body) > 1048576:
            raise ValueError("Settings exceed 1MiB")
        def pairs(items):
            d = {}
            for k, v in items:
                if k in d:
                    raise ValueError("Duplicate settings key")
                d[k] = v
            return d
        value = validate(json.loads(body, object_pairs_hook=pairs))
        store._scheduling_cache = (stamp, value)
        return view(value)
    except (OSError, ValueError, TypeError, KeyError):
        raise DispatchBlocked("scheduling_settings_invalid") from None


def route_reason(value, route):
    if not value["enabled"]:
        return "scheduling_disabled"
    if value["sources"].get(route.source) is False:
        return "source_dispatch_disabled"
    if value["mode"] == "directed":
        live = [r for r in value["pool"] if r["expires_at"] > now_ms()]
        if not live:
            return "directed_pool_expired" if value["pool"] else "directed_pool_empty"
        if route.id not in {r["route_id"] for r in live}:
            return "route_not_selected"
    return None


def reconcile_information(inherited, job):
    """Bind support to exact ancestor questions; this checks structure, not truth."""
    pending = {q["id"]: q for q in inherited}
    for report in job["results"]:
        content = report.get("content") or {}
        support = content.get("information_support", [])
        requests = content.get("information_requests", [])
        if not isinstance(support, list) or len(support) > 8 or not isinstance(requests, list) or len(requests) > 8:
            raise ValueError("invalid_information_protocol")
        seen = set()
        for item in support:
            if not isinstance(item, dict) or not isinstance(item.get("request_id"), str) or item["request_id"] not in {q["id"] for q in inherited} or item["request_id"] in seen:
                raise ValueError("information_support_requires_exact_upstream_request")
            seen.add(item["request_id"])
            for field in ["summary", "evidence"]:
                text(item.get(field), "information support " + field, 4000)
            pending.pop(item["request_id"], None)
        for i, item in enumerate(requests):
            if not isinstance(item, dict):
                raise ValueError("invalid_information_request")
            question = text(item.get("question"), "information question", 2000)
            reason = text(item.get("reason"), "information reason", 2000)
            ident = f"{job['job_id']}:{report['worker']}:{i}"
            pending[ident] = {"id": ident, "question": question, "reason": reason, "job_id": job["job_id"]}
    if len(pending) > 64:
        raise ValueError("too_many_unresolved_information_requests")
    return list(pending.values())


class Scheduling:
    def __init__(self, service):
        self.service = service
        self.lock = threading.RLock()
        self.threads = {}
        self.cancellations = {}
        self.stopping = threading.Event()
        with service.store.lock, service.store.conn:
            service.store.conn.execute("CREATE TABLE IF NOT EXISTS visual_runs(id TEXT PRIMARY KEY,updated_at INTEGER NOT NULL,payload TEXT NOT NULL)")
            service.store.conn.execute("CREATE UNIQUE INDEX IF NOT EXISTS visual_request_identity ON visual_runs(json_extract(payload,'$.request_id')) WHERE json_extract(payload,'$.request_id') IS NOT NULL")
            service.store.conn.execute("CREATE INDEX IF NOT EXISTS visual_execution_identity ON visual_runs(json_extract(payload,'$.fingerprint'))")
            for ident, payload in service.store.conn.execute("SELECT id,payload FROM visual_runs").fetchall():
                run = json.loads(payload)
                if run["state"] not in TERMINAL:
                    run["state"] = "interrupted"
                    for node in run["nodes"].values():
                        if node["state"] in {"queued", "running"}:
                            node["state"] = "unknown"
                    self._save(run)

    def _save(self, run):
        with self.service.store.lock, self.service.store.conn:
            self.service.store.conn.execute("INSERT INTO visual_runs VALUES(?,?,?) ON CONFLICT(id) DO UPDATE SET updated_at=excluded.updated_at,payload=excluded.payload",
                (run["id"], now_ms(), json.dumps(run, ensure_ascii=False)))

    def status(self, run_id, *, full=False):
        text(run_id, "run id")
        with self.service.store.lock:
            row = self.service.store.conn.execute("SELECT payload FROM visual_runs WHERE id=?", (run_id,)).fetchone()
        if not row:
            raise ValueError("Unknown workflow run")
        result = json.loads(row[0])
        if not full:
            result.pop("input", None)
            result.pop("definition", None)
            for node in result["nodes"].values():
                requests = node.get("information_requests", [])
                node["information_request_count"] = len(requests)
                node["information_requests_excerpt_only"] = bool(requests)
                node["information_requests"] = [{**q, "question": q["question"][:300], "reason": q["reason"][:300]} for q in requests[:8]]
        return result

    def control(self, action="get", value=None, expected_revision=None, route_id=None, workflow_id="default", luna_supervision=True):
        if not isinstance(luna_supervision, bool):
            raise ValueError("luna_supervision must be a boolean")
        if action == "save":
            candidate = validate(value)
            with self.lock:
                prior = settings(self.service.store)
                if integer(expected_revision, "expected revision", 0, 2**53 - 1) != prior["revision"]:
                    raise ValueError("scheduling_revision_conflict: reload before saving")
                candidate["revision"] = prior["revision"] + 1
                body = json.dumps(candidate, ensure_ascii=False).encode("utf-8")
                if len(body) > 1048576:
                    raise ValueError("Settings exceed 1MiB")
                path = self.service.state_dir / FILE_NAME
                temporary = path.with_name(FILE_NAME + "." + uuid.uuid4().hex + ".tmp")
                try:
                    with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as f:
                        f.write(body)
                    os.replace(temporary, path)
                finally:
                    if temporary.exists():
                        temporary.unlink()
            with self.service.capacity:
                self.service.call_profile()
                self.service.capacity.notify_all()
        elif action not in {"get", "refresh", "check"}:
            raise ValueError("Unsupported scheduling action")
        check = None
        if action in {"refresh", "check"}:
            self.service.require_external_interface()
        if action == "refresh":
            self.service.catalog.refresh(force=True)
        if action == "check":
            check = self.service.recovery.recheck(text(route_id, "route id"), workflow_id, supervision_required=luna_supervision)
        value = settings(self.service.store)
        # A plain get does not launch native discovery or a generation probe.
        routes = []
        for route in self.service.catalog.routes:
            h = self.service.store.health("route:" + route.id)
            p = self.service.store.health("provider:" + route.failure_domain)
            s = self.service.store.health("source:" + route.source)
            verified = h.get("last_verified_at", 0)
            routes.append({"route_id": route.id, "source": route.source, "model": route.model, "effort": route.effort,
                "output_limit": route.output_limit, "pricing": route.pricing, "capabilities": route.capabilities,
                "auth_hint": route.auth_hint, "dispatch_enabled": route.enabled and value["sources"].get(route.source, True),
                "selection_reason": route_reason(value, route), "last_verified_at": verified, "last_error": h["last_error"],
                "connectivity": "verification_required" if any(x.get("verification_required") for x in [h, p, s]) else
                    "cooling_down" if max(h["cooldown_until"], p["cooldown_until"]) > now_ms() else
                    "verified" if verified and 0 <= now_ms() - verified <= 86400000 else "stale" if verified else "unverified"})
        with self.service.store.lock:
            runs = [json.loads(r[0]) for r in self.service.store.conn.execute("SELECT payload FROM visual_runs ORDER BY updated_at DESC LIMIT 16")]
        from .profiles import presets
        return {"settings": value, "routes": routes, "sources": [{k:r[k] for k in ["id","installed","registered","reason"] if k in r} for r in self.service.config.get("source_registry", [])[:64]],
            "discovery_errors": getattr(self.service.catalog, "discovery_errors", []), "refreshed_at": self.service.catalog.refreshed_at,
            "supervisors": [{"workflow_id": a["workflow_id"], "id": a["id"]} for a in self.service._active_supervisors()],
            "runs": [{k: r[k] for k in ["id", "name", "workflow_id", "state", "tokens_used"]} for r in runs], "check": check,
            "templates": json.loads((Path(__file__).parent.parent / "workflow-templates.json").read_text(encoding="utf-8")),
            "call_presets": presets(),
            "effective_profile": self.service.call_profile()}

    def plan(self, definition, workflow_id="default"):
        definition = graph(definition)
        self._check_limits(definition)
        return {"workflow": definition, "nodes": [{"id": n["id"], "plan": self.service.plan({**n["task"],
            "preferred_route": n["route_id"], "allowed_routes":[n["route_id"],*n["fallback_routes"]],"workflow_id": workflow_id})
            if n["node_type"] == "model" else {"local_node": n["node_type"], "generates_content": False}} for n in definition["nodes"]]}

    def submit(self, definition, input_text="", workflow_id="default", request_id="", acknowledge_unknown_risk=False):
        definition = graph(definition)
        from .profiles import ABS_LIMITS
        text(input_text,"workflow input",ABS_LIMITS["max_workflow_input_chars"],empty=True)
        text(workflow_id,"workflow id",200)
        text(request_id,"workflow request id",200,empty=True)
        if not isinstance(acknowledge_unknown_risk,bool):
            raise ValueError("acknowledge_unknown_risk must be boolean")
        fingerprint_nodes=[{k:v for k,v in n.items() if k not in {"x","y","title","tags"}} for n in definition["nodes"]]
        fingerprint=hashlib.sha256(json.dumps({"nodes":fingerprint_nodes,"edges":definition["edges"],"input":input_text,"workflow_id":workflow_id},sort_keys=True,ensure_ascii=False).encode()).hexdigest()
        request_id = request_id or fingerprint
        prior = self._previous(request_id,fingerprint,acknowledge_unknown_risk)
        if prior:return prior
        self._check_limits(definition)
        gate = Task("extract", "Workflow supervision setting", workflow_id=workflow_id)
        gate._workflow_supervision_opt_out = not definition["luna_supervision"]
        self.service.require_dispatch(workflow_id, task=gate)
        text(input_text, "workflow input", self.service.call_profile()["limits"]["max_workflow_input_chars"], empty=True)
        self.service.ensure_catalog()
        with self.lock:
            prior = self._previous(request_id,fingerprint,acknowledge_unknown_risk)
            if prior:return prior
            self.threads = {k:t for k,t in self.threads.items() if t.is_alive()}
            self.cancellations = {k:v for k,v in self.cancellations.items() if k in self.threads}
            if sum(t.is_alive() for t in self.threads.values()) >= 2:
                raise ValueError("visual_workflow_capacity_full")
            ident = uuid.uuid4().hex
            self.cancellations[ident] = threading.Event()
            run = {"id": ident, "name": definition["name"], "workflow_id": workflow_id, "state": "running", "tokens_used": 0,
                "definition_id":definition["id"],
                "tokens_estimated": 0, "definition": definition, "input": input_text,
                "supervision": "required" if definition["luna_supervision"] else "workflow_opt_out",
                "request_id":request_id,"fingerprint":fingerprint,"acknowledge_unknown_risk":acknowledge_unknown_risk,
                "nodes": {n["id"]: {"state": "pending", "job_id": None, "tokens_used": 0} for n in definition["nodes"]}}
            self._save(run)
            thread = threading.Thread(target=self._run, args=(ident,), daemon=True, name="gtt-visual-dag")
            self.threads[ident] = thread
            thread.start()
        return {"run_id": ident, "state": "running"}

    def _previous(self, request_id, fingerprint, acknowledge):
        with self.lock, self.service.store.lock:
            candidates=[json.loads(r[0]) for r in self.service.store.conn.execute("SELECT payload FROM visual_runs WHERE json_extract(payload,'$.request_id')=? OR json_extract(payload,'$.fingerprint')=?",(request_id,fingerprint))]
            for prior in candidates:
                if prior.get("request_id")==request_id:
                    if prior.get("fingerprint")!=fingerprint:raise ValueError("workflow_request_id_conflict")
                    return {"run_id":prior["id"],"state":prior["state"],"deduplicated":True}
            for prior in candidates:
                if not acknowledge and (prior["state"]=="interrupted" or any(n["state"]=="unknown" for n in prior["nodes"].values())):
                    return {"run_id":prior["id"],"state":prior["state"],"blocked":"unknown_paid_outcome_requires_review"}
        return None

    def cancel(self, run_id):
        if run_id in self.cancellations:
            self.cancellations[run_id].set()
        with self.lock:
            run = self.status(run_id, full=True)
            if run["state"] not in TERMINAL:
                run["state"] = "cancelling"
                self._save(run)
        return {"run_id": run_id, "state": run["state"]}

    def _run(self, run_id):
        try:
            while not self.stopping.is_set():
                with self.lock:
                    run = self.status(run_id, full=True)
                    definition, states = run["definition"], run["nodes"]
                    before = (run["state"], tuple((n["state"], n["job_id"], n["tokens_used"],n.get("job_state"),str(n.get("activity"))) for n in states.values()))
                    used, estimated, reserved = 0, 0, 0
                    for node in states.values():
                        if node["job_id"]:
                            if node["state"] in TERMINAL:
                                used += node["tokens_used"]
                                estimated += node.get("tokens_estimated", 0)
                                continue
                            job = self.service.store.job(node["job_id"])
                            node["job_state"] = job["state"]
                            progress = next(iter(job.get("progress",{}).values()),{})
                            node["activity"] = progress.get("activity")
                            node["tokens_used"] = job["tokens_used"]
                            node["tokens_estimated"] = job["tokens_estimated"]
                            used += job["tokens_used"]
                            estimated += job["tokens_estimated"]
                            if job["state"] in {"succeeded", "partial", "failed", "cancelled", "interrupted", "needs_codex"}:
                                node["state"] = "succeeded" if job["state"] == "succeeded" and all(r.get("ok") for r in job["results"]) else (
                                    "unknown" if job["state"] == "interrupted" or any((a.get("error") or {}).get("code") == "timeout" or a.get("unknown_paid_outcome") for a in job["attempts"]) else "failed")
                                # The child job owns the full report. Keep a
                                # bounded index here instead of rewriting large
                                # reports on every DAG progress/cancel update.
                                node["report_count"] = len(job["results"])
                                node["reports"] = [{"worker": r["worker"], "ok": r.get("ok", False),
                                    "summary": str((r.get("content") or {}).get("summary", ""))[:600],
                                    "excerpt_only": True} for r in job["results"][:8]]
                                node["evidence_refs"] = [node["job_id"]] if node["state"] == "succeeded" else []
                                node["error"] = job.get("error") or next((r.get("error") for r in job["results"] if r.get("error")), None)
                                if node["state"] == "succeeded":
                                    try:
                                        node["information_requests"] = reconcile_information(node.get("information_requests", []), job)
                                    except ValueError as error:
                                        node.update(state="blocked", reason=str(error), evidence_refs=[])
                            else:
                                node["state"] = "running"
                                reserved += max(0, node["reserved_budget"] - job["tokens_used"])
                    run.update(tokens_used=used, tokens_estimated=estimated)
                    cancelling = run["state"] == "cancelling"
                    active = sum(n["state"] == "running" for n in states.values())
                    for spec in definition["nodes"]:
                        node = states[spec["id"]]
                        if cancelling:
                            if node["state"] == "running":
                                self.service.cancel(node["job_id"])
                            elif node["state"] == "pending":
                                node["state"] = "cancelled"
                            continue
                        if node["state"] != "pending":
                            continue
                        deps = [states[e["from"]] for e in definition["edges"] if e["to"] == spec["id"]]
                        if any(d["state"] in TERMINAL - {"succeeded"} for d in deps):
                            node.update(state="blocked", reason="upstream_not_succeeded")
                            continue
                        if any(d["state"] != "succeeded" for d in deps) or active >= definition["max_parallel"]:
                            continue
                        refs = list(dict.fromkeys(ref for d in deps for ref in d.get("evidence_refs", [])))
                        requests = {q["id"]: q for d in deps for q in d.get("information_requests", [])}
                        node["information_requests"] = list(requests.values())
                        if spec["node_type"] != "model":
                            if spec["node_type"] in {"gate", "output"} and requests:
                                node.update(state="blocked", reason="unresolved_information_requests")
                            elif spec["node_type"] == "gate" and not refs:
                                node.update(state="blocked", reason="gate_requires_model_evidence")
                            else:
                                node.update(state="succeeded", evidence_refs=refs, local_node=spec["node_type"], role=spec["role"])
                            continue
                        upstream = [{"job_id": ident, "reports": [r.get("content") for r in self.service.store.job(ident)["results"] if r.get("ok")]}
                                    for ident in refs]
                        context = json.dumps({"input": run["input"], "upstream_evidence": upstream,
                            "unresolved_information_requests": list(requests.values())}, ensure_ascii=False)
                        context_limit = self.service.call_profile()["limits"]["max_context_chars"] - len(spec["task"]["prompt"]) - len(spec["role"]) - len(INFORMATION_PROMPT) - 8
                        if len(context) > context_limit:
                            node.update(state="blocked", reason="upstream_evidence_too_large")
                            continue  # Never silently clip evidence or fabricate completeness.
                        task_data = {**spec["task"], "prompt": "职责：" + spec["role"] + "\n" + spec["task"]["prompt"] + INFORMATION_PROMPT,
                            "context": context, "preferred_route": spec["route_id"],
                            "allowed_routes": [spec["route_id"], *spec["fallback_routes"]], "workflow_id": run["workflow_id"],
                            "work_key": "visual:" + run_id + ":" + spec["id"]}
                        try:
                            budget = self.service.parse_task(task_data).token_budget
                        except ValueError as error:
                            node.update(state="blocked", reason=str(error)[:300])
                            continue
                        if used + reserved + budget > definition["token_budget"]:
                            if active:
                                continue
                            node.update(state="blocked", reason="workflow_token_budget_exhausted")
                            continue
                        try:
                            routes = [r for ident in [spec["route_id"],*spec["fallback_routes"]] for r in self.service.catalog.routes if r.id==ident]
                            if not routes:
                                raise ValueError("workflow_route_not_found")
                            accepted = False
                            for route in routes:
                                try:self.service.require_scheduling(route);accepted=True;break
                                except DispatchBlocked:pass
                            if not accepted:self.service.require_scheduling(routes[0])
                            # Discovery/CLI startup can wait; UI cancellation
                            # and settings writes must not wait behind that IO.
                            self.lock.release()
                            try:
                                submitted = self.service.submit(task_data, _supervision_required=definition["luna_supervision"],
                                    _cancellation_event=self.cancellations.get(run_id))
                            finally:
                                self.lock.acquire()
                            if self.cancellations[run_id].is_set():
                                run["state"], cancelling = "cancelling", True
                            if "job_id" not in submitted:
                                raise ValueError("visual_node_not_submitted")
                            node.update(state="running", job_id=submitted["job_id"], reserved_budget=budget)
                            reserved += budget
                            active += 1
                        except ValueError as error:
                            node.update(state="cancelled" if self.cancellations[run_id].is_set() else "blocked", reason=str(error)[:300])
                            if self.cancellations[run_id].is_set():
                                run["state"], cancelling = "cancelling", True
                    if all(n["state"] in TERMINAL for n in states.values()):
                        run["state"] = "cancelled" if cancelling else "succeeded" if all(n["state"] == "succeeded" for n in states.values()) else "failed"
                    after = (run["state"], tuple((n["state"], n["job_id"], n["tokens_used"],n.get("job_state"),str(n.get("activity"))) for n in states.values()))
                    if before != after:
                        self._save(run)
                    if run["state"] in TERMINAL:
                        return
                self.stopping.wait(.5)
        except Exception:
            with self.lock:
                run = self.status(run_id, full=True)
                run["state"] = "interrupted"
                self._save(run)

    def shutdown(self):
        self.stopping.set()
        for thread in self.threads.values():
            thread.join(timeout=2)

    def _check_limits(self, definition):
        limits = self.service.call_profile()["limits"]
        if len(definition["nodes"]) > limits["max_workflow_nodes"] or definition["token_budget"] > limits["max_workflow_tokens"] or definition["max_parallel"] > limits["max_workflow_parallel"]:
            raise ValueError("Workflow exceeds the active call profile limits")
        if any(len(n["role"])>limits["max_node_role_chars"] or len(n["task"]["prompt"])>limits["max_node_prompt_chars"] for n in definition["nodes"]):
            raise ValueError("Node responsibilities or prompt exceed the active call profile limits")
