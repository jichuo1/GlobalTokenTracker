"""One durable intake for tasks, research groups and saved visual workflows.

The journal records identity before admission. An interrupted admission is never
replayed: its preallocated target remains queryable, or the receipt stays unknown.
Execution, reservations and accounting still belong to the existing worker path.
"""

import copy
import hashlib
import json
import threading
import uuid

from .models import Task, now_ms, reject_secrets
from .scheduling import graph, integer, settings, text

READ_ACTIONS = {"status", "collect", "report", "list", "cancel"}
LEGACY_SUBMISSIONS = {"router_plan", "router_submit", "router_group_plan", "router_group_submit",
                      "router_workflow_plan", "router_workflow_submit"}


def encoded(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":"))


class Dispatch:
    def __init__(self, service):
        self.service = service
        self.lock = threading.RLock()
        self.admissions = {}
        with service.store.lock, service.store.conn:
            service.store.conn.execute("CREATE TABLE IF NOT EXISTS dispatches("
                "id TEXT PRIMARY KEY,request_id TEXT UNIQUE NOT NULL,fingerprint TEXT NOT NULL,"
                "created_at INTEGER NOT NULL,kind TEXT NOT NULL,workflow_id TEXT NOT NULL,payload TEXT NOT NULL)")
            service.store.conn.execute("CREATE INDEX IF NOT EXISTS dispatch_workflow ON dispatches(workflow_id,created_at)")
            service.store.conn.execute("CREATE INDEX IF NOT EXISTS dispatch_fingerprint ON dispatches(fingerprint)")
            for ident, body in service.store.conn.execute("SELECT id,payload FROM dispatches").fetchall():
                row = json.loads(body)
                if row["state"] == "admitting":
                    row["state"] = "unknown"
                    self._save(row)

    def _save(self, row):
        with self.service.store.lock, self.service.store.conn:
            self.service.store.conn.execute("INSERT INTO dispatches VALUES(?,?,?,?,?,?,?) "
                "ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",
                (row["dispatch_id"], row["request_id"], row["fingerprint"], row["created_at"],
                 row["kind"], row["workflow_id"], encoded(row)))

    def _find(self, dispatch_id=None, request_id=None):
        if bool(dispatch_id) == bool(request_id):
            raise ValueError("Supply exactly one dispatch_id or request_id")
        value = text(dispatch_id or request_id, "dispatch identity")
        field = "id" if dispatch_id else "request_id"
        with self.service.store.lock:
            row = self.service.store.conn.execute("SELECT payload FROM dispatches WHERE " + field + "=?", (value,)).fetchone()
        if row is None:
            raise ValueError("Unknown dispatch")
        return json.loads(row[0])

    def _prepare(self, request):
        if not isinstance(request, dict):
            raise ValueError("dispatch request must be an object")
        request = copy.deepcopy(request)
        kind = request.get("kind")
        allowed = {"task": {"kind", "task"}, "group": {"kind", "request"},
                   "workflow": {"kind", "definition", "saved_workflow_id", "expected_revision", "input_text", "workflow_id"}}
        if kind not in allowed or set(request) - allowed[kind]:
            raise ValueError("Unsupported dispatch request fields or kind")
        if kind == "task":
            task = self.service.parse_task(request.get("task"))
            return request, task.workflow_id, task
        if kind == "group":
            from .group_contract import GroupRequest
            group = GroupRequest.parse(request.get("request"))
            return request, group.workflow_id, None
        if ("definition" in request) == ("saved_workflow_id" in request):
            raise ValueError("Workflow requires exactly one definition or saved_workflow_id")
        if "saved_workflow_id" in request:
            ident = text(request.pop("saved_workflow_id"), "saved workflow id")
            value = settings(self.service.store)
            if "expected_revision" in request and integer(request.pop("expected_revision"), "expected revision", 0, 2**53 - 1) != value["revision"]:
                raise ValueError("scheduling_revision_conflict: reload before dispatch")
            definition = next((g for g in value["workflows"] if g["id"] == ident), None)
            if definition is None:
                raise ValueError("Unknown saved workflow")
            request["definition"] = definition
        elif "expected_revision" in request:
            raise ValueError("expected_revision requires saved_workflow_id")
        request["definition"] = graph(request["definition"])
        self.service.scheduling._check_limits(request["definition"])
        workflow_id = text(request.get("workflow_id", "default"), "workflow id")
        text(request.get("input_text", ""), "workflow input",
             self.service.call_profile()["limits"]["max_workflow_input_chars"], empty=True)
        gate = Task("extract", "Dispatch workflow", workflow_id=workflow_id)
        gate._workflow_supervision_opt_out = not request["definition"]["luna_supervision"]
        return request, workflow_id, gate

    def plan(self, request):
        request, _, _ = self._prepare(request)
        kind = request["kind"]
        if kind == "task":
            result = self.service.plan(request["task"])
        elif kind == "group":
            result = self.service.groups.plan(request["request"])
        else:
            result = self.service.scheduling.plan(request["definition"], request.get("workflow_id", "default"))
        return {"kind": kind, "plan": result, "entrypoint": "router_dispatch"}

    def submit(self, request, request_id, acknowledge_unknown_risk=False):
        text(request_id, "dispatch request id")
        if not isinstance(acknowledge_unknown_risk, bool):
            raise ValueError("acknowledge_unknown_risk must be boolean")
        if not isinstance(request, dict):
            raise ValueError("dispatch request must be an object")
        reject_secrets(encoded(request))
        fingerprint = hashlib.sha256(encoded(request).encode()).hexdigest()
        with self.lock:
            try:
                prior = self._find(request_id=request_id)
            except ValueError as error:
                if str(error) != "Unknown dispatch":
                    raise
            else:
                if prior["fingerprint"] != fingerprint:
                    raise ValueError("dispatch_request_id_conflict")
                return {**self._status(prior), "deduplicated": True}
        request, workflow_id, gate = self._prepare(request)
        self.service.require_dispatch(workflow_id, task=gate)
        with self.lock:
            # Validation/discovery must not hold the admission lock. Another
            # client may have admitted this identity while it was being checked.
            try:
                prior = self._find(request_id=request_id)
            except ValueError:
                prior = None
            if prior is not None:
                if prior["fingerprint"] != fingerprint:
                    raise ValueError("dispatch_request_id_conflict")
                return {**self._status(prior), "deduplicated": True}
            if not acknowledge_unknown_risk:
                with self.service.store.lock:
                    previous = self.service.store.conn.execute("SELECT payload FROM dispatches WHERE fingerprint=? ORDER BY created_at DESC", (fingerprint,)).fetchall()
                for candidate in previous:
                    prior = json.loads(candidate[0])
                    detail = self._detail(prior)
                    uncertain = (prior["state"] == "unknown" and detail is None) or (detail is not None and detail["state"] in {"interrupted", "unknown"})
                    if detail is not None and prior["kind"] != "workflow":
                        attempts = self.service.store.job(prior["target_id"])["attempts"]
                        uncertain |= any(a.get("unknown_paid_outcome") or (a.get("error") or {}).get("code") == "timeout" for a in attempts)
                    if uncertain:
                        return {**self._status(prior), "blocked": "unknown_paid_outcome_requires_review"}
            target_id = uuid.uuid4().hex
            cancellation = threading.Event()
            row = {"dispatch_id": uuid.uuid4().hex, "request_id": request_id, "fingerprint": fingerprint,
                   "created_at": now_ms(), "kind": request["kind"], "workflow_id": workflow_id,
                   "target_id": target_id, "state": "admitting"}
            self._save(row)
            self.admissions[row["dispatch_id"]] = cancellation
        try:
            if row["kind"] == "task":
                result = self.service.submit(request["task"], _job_id=target_id, _cancellation_event=cancellation)
            elif row["kind"] == "group":
                result = self.service.groups.submit(request["request"], _job_id=target_id, _cancellation_event=cancellation)
            else:
                result = self.service.scheduling.submit(request["definition"], request.get("input_text", ""),
                    workflow_id, request_id="dispatch:" + row["dispatch_id"],
                    acknowledge_unknown_risk=acknowledge_unknown_risk, _run_id=target_id, _cancellation_event=cancellation)
        except Exception as error:
            # A known rejection before target persistence is safe to report.
            # Unexpected failure, or any existing target, remains queryable.
            with self.lock:
                row = self._find(dispatch_id=row["dispatch_id"])
                if cancellation.is_set() and self._detail(row) is None:
                    row.update(state="cancelled", target_id=None, reason="cancelled_before_admission")
                    self._save(row)
                    return self._status(row)
                row["state"] = "rejected" if isinstance(error, ValueError) and self._detail(row) is None else "unknown"
                row["error"] = str(error)[:300] if isinstance(error, ValueError) else "admission_outcome_unknown"
                self._save(row)
            raise
        finally:
            with self.lock:
                self.admissions.pop(row["dispatch_id"], None)
        with self.lock:
            row = self._find(dispatch_id=row["dispatch_id"])
            row["target_id"] = result.get({"task": "job_id", "group": "group_id", "workflow": "run_id"}[row["kind"]])
            row["state"] = result["state"]
            row["receipt"] = {k: v for k, v in result.items() if k not in {"task", "preferences"}}
            self._save(row)
        if row.get("cancel_requested") and row["target_id"]:
            self._cancel(row)
        return {**result, **self._status(row)}

    def _detail(self, row):
        if not row.get("target_id"):
            return None
        try:
            if row["kind"] == "workflow":
                return self.service.scheduling.status(row["target_id"])
            if row["kind"] == "group":
                return self.service.groups.status(row["target_id"])
            return self.service.status(row["target_id"])
        except ValueError:
            return None

    def _status(self, row):
        detail = self._detail(row)
        result = {k: v for k, v in row.items() if k not in {"fingerprint", "receipt"}}
        result.update(row.get("receipt", {}))
        if detail is not None:
            result.update(state=detail["state"], detail=detail)
        if row.get("target_id"):
            result[{"task": "job_id", "group": "group_id", "workflow": "run_id"}[row["kind"]]] = row["target_id"]
        return result

    def _cancel(self, row):
        if self._detail(row) is None:
            return self._status(row)
        target = self.service.scheduling if row["kind"] == "workflow" else self.service.groups if row["kind"] == "group" else self.service
        target.cancel(row["target_id"])
        return self._status(row)

    def control(self, action, **params):
        allowed = {"plan": {"request"}, "submit": {"request", "request_id", "acknowledge_unknown_risk"},
                   "status": {"dispatch_id", "request_id"}, "cancel": {"dispatch_id", "request_id"},
                   "collect": {"dispatch_id", "request_id", "cursor", "max_chars"},
                   "report": {"dispatch_id", "request_id", "node_id", "worker", "offset", "max_chars"},
                   "list": {"workflow_id", "cursor", "limit"}}
        if action not in allowed or set(params) - allowed[action]:
            raise ValueError("Unsupported dispatch action or fields")
        if action in {"plan", "submit"}:
            self.service.require_external_interface()
            if "request" not in params or (action == "submit" and "request_id" not in params):
                raise ValueError("dispatch request and stable submit request_id are required")
            return getattr(self, action)(**params)
        if action == "list":
            cursor = integer(params.get("cursor", 0), "cursor", 0, 2**53 - 1)
            limit = integer(params.get("limit", 16), "limit", 1, 64)
            workflow = params.get("workflow_id")
            clause, values = ("WHERE workflow_id=?", [text(workflow, "workflow id")]) if workflow is not None else ("", [])
            with self.service.store.lock:
                rows = self.service.store.conn.execute("SELECT payload FROM dispatches " + clause +
                    " ORDER BY created_at DESC,id DESC LIMIT ? OFFSET ?", (*values, limit + 1, cursor)).fetchall()
            items = [self._status(json.loads(r[0])) for r in rows[:limit]]
            for item in items:
                item.pop("detail", None)
            return {"dispatches": items, "next_cursor": cursor + len(items), "has_more": len(rows) > limit}
        identity = {k: params.pop(k) for k in ("dispatch_id", "request_id") if k in params}
        row = self._find(**identity)
        if action == "status":
            return self._status(row)
        if action == "cancel":
            with self.lock:
                row = self._find(dispatch_id=row["dispatch_id"])
                row["cancel_requested"] = True
                self._save(row)
                if row["dispatch_id"] in self.admissions:
                    self.admissions[row["dispatch_id"]].set()
            return self._cancel(row)
        maximum = integer(params.get("max_chars", 12000), "max_chars", 2000, 30000)
        if self._detail(row) is None:
            return {**self._status(row), "results": [], "has_more": False, "next_cursor": 0}
        if action == "collect":
            cursor = integer(params.get("cursor", 0), "cursor", 0, 2**53 - 1)
            target = self.service.scheduling if row["kind"] == "workflow" else self.service.groups if row["kind"] == "group" else self.service
            return {**target.collect(row["target_id"], cursor=cursor, max_chars=maximum),
                    "dispatch_id": row["dispatch_id"], "kind": row["kind"], "report_action": "router_dispatch:report"}
        worker = integer(params.get("worker"), "worker", 0, 2**31 - 1)
        offset = integer(params.get("offset", 0), "offset", 0, 2**53 - 1)
        target_id = row["target_id"]
        if row["kind"] == "workflow":
            run = self.service.scheduling.status(target_id)
            node_id = text(params.get("node_id"), "node id")
            node = run["nodes"].get(node_id)
            if node is None or not node.get("job_id"):
                raise ValueError("No report for this workflow node")
            target_id = node["job_id"]
        elif "node_id" in params:
            raise ValueError("node_id requires a workflow dispatch")
        job = self.service.store.job(target_id)
        report = next((r for r in job["results"] if r["worker"] == worker), None)
        if report is None:
            raise ValueError("Unknown report worker")
        body = encoded(report)
        if offset > len(body):
            raise ValueError("Report offset exceeds length")
        end = min(len(body), offset + maximum)
        return {"dispatch_id": row["dispatch_id"], "job_id": target_id, "worker": worker,
                "report_json": body[offset:end], "offset": offset, "next_offset": end,
                "has_more": end < len(body), "total_chars": len(body), "sha256": hashlib.sha256(body.encode()).hexdigest()}

    def legacy(self, method, params):
        kind = "workflow" if method.startswith("router_workflow_") else "group" if method.startswith("router_group_") else "task"
        if kind == "workflow":
            request = {"kind": kind, **{k: v for k, v in params.items() if k not in {"request_id", "acknowledge_unknown_risk"}}}
        else:
            key = "task" if kind == "task" else "request"
            if key not in params or set(params) - {key, "request_id", "acknowledge_unknown_risk"}:
                raise ValueError("Legacy dispatch requires only " + key)
            request = {"kind": kind, key: params[key]}
        if method.endswith("plan"):
            return self.plan(request)["plan"]
        request_id = params.get("request_id") or ("legacy:" + kind + ":" + hashlib.sha256(encoded(request).encode()).hexdigest())
        result = self.submit(request, request_id, params.get("acknowledge_unknown_risk", False))
        result.pop("detail", None)
        return {**result, "deprecated_entrypoint": method, "replacement": "router_dispatch"}
