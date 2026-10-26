"""Caller-owned work queue and read claims; never executes or impersonates Codex."""

import copy
import hashlib
import json
import os

from .models import now_ms
from .scheduling import TERMINAL, integer, text


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True).encode()).hexdigest()


def source_identity(sources):
    return [{k: item.get(k) for k in ("path", "sha256", "exists", "line_range")} for item in sources]


def has_dispatch_decisions(run):
    # Old persisted runs and protocol fixtures used opaque nonempty markers.
    return any(not isinstance(decision, dict) or decision.get("type") != "controller_work"
               for decision in run.get("decisions", []))


class ControllerWork:
    def __init__(self, operators):
        self.operators = operators

    @staticmethod
    def items(run):
        return run.get("controller_work", {}).get("items", {})

    @staticmethod
    def report_identity(ident, node):
        operation = node.get("operation_result") or {}
        return fingerprint({"node_id": ident, **{key: node.get(key) for key in
            ("state", "job_id", "work_revision", "report_digest", "review")},
            "operation_state": operation.get("state"), "completed_order": operation.get("completed_order"),
            "after_files": source_identity(operation.get("after_files", []))})

    def inbox(self, run):
        seen = run.get("controller_work", {}).get("seen", {})
        result = []
        for ident, node in run["nodes"].items():
            if not node.get("job_id") or node["state"] not in TERMINAL:
                continue
            identity = self.report_identity(ident, node)
            if seen.get(ident) == identity:
                continue
            result.append({"node_id": ident, "state": node["state"], "job_id": node["job_id"],
                "work_revision": node.get("work_revision", 1), "fingerprint": identity,
                "error": node.get("error"), "review": node.get("review")})
        return result

    def active(self, run):
        if run["state"] in TERMINAL:
            return None
        return next((item for item in self.items(run).values()
                     if item["status"] == "running" and item.get("lease_until", 0) > now_ms()), None)

    @staticmethod
    def overlap(left, right):
        return bool({os.path.normcase(p) for p in left} & {os.path.normcase(p) for p in right})

    def blockers(self, run, item):
        blockers = []
        for ident in item["depends_on"]:
            state = run["nodes"][ident]["state"]
            satisfied = state in TERMINAL if item["wait_for"] == "terminal" else state == "succeeded"
            if not satisfied:
                blockers.append("dependency:" + ident + ":" + state)
        if item["access"] == "current" and item["paths"]:
            operators = {n["id"] for n in run["definition"]["nodes"] if n["node_type"] == "operator"}
            for ident, node in run["nodes"].items():
                assignment = run.get("assignments", {}).get(ident)
                if ident in operators and assignment and node["state"] in {"running", "executing"} and (
                        assignment["command_ids"] or self.overlap(item["paths"], assignment["paths"])):
                    blockers.append("active_operator:" + ident)
        return blockers

    def blocks_operator(self, run, assignment):
        active = self.active(run)
        if active and active["access"] == "current" and active["paths"] and (
                assignment.get("command_ids") or self.overlap(active["paths"], assignment["paths"])):
            return active["id"]
        return None

    def known_stale(self, run, item):
        if item["access"] != "current" or item["status"] not in {"running", "completed"}:
            return False
        if any(self.report_identity(ident, run["nodes"][ident]) != identity
               for ident, identity in item.get("dependency_sources", {}).items()):
            return True
        sources = {s["path"]: s.get("sha256") for s in item.get("sources", [])}
        for node in run["nodes"].values():
            operation = node.get("operation_result") or {}
            if operation.get("completed_at", 0) < item.get("started_at", 0):
                continue
            if any(f["path"] in sources and f.get("sha256") != sources[f["path"]]
                   for f in operation.get("after_files", [])):
                return True
        return False

    def unfinished(self, run):
        return [item["id"] for item in self.items(run).values()
                if item["status"] not in {"completed", "cancelled"} or self.known_stale(run, item)]

    def view(self, run):
        active = self.active(run)
        items, ready = [], []
        for original in self.items(run).values():
            item = {key: copy.deepcopy(value) for key, value in original.items() if key not in {"sources", "history"}}
            blockers = self.blockers(run, original)
            stale = self.known_stale(run, original)
            if (original["status"] not in {"completed", "cancelled"} or stale) and not blockers and original is not active:
                ready.append(original["id"])
            if original["status"] == "running" and original.get("lease_until", 0) <= now_ms():
                item["status"] = "lease_expired"
            if stale:
                item["status"] = "stale"
            item["blockers"] = blockers
            item["sources"] = source_identity(original.get("sources", []))
            items.append(item)
        reports = self.inbox(run)
        workers = [ident for ident, n in run["nodes"].items() if n["state"] in {"running", "executing"}]
        phase = ("stale_inputs" if self.known_stale(run, active) else active["kind"]) if active else "ready" if ready else "review_needed" if reports else "waiting" if workers else "idle"
        if run["state"] in TERMINAL:
            phase = "finished"
        return {"phase": phase, "source": "caller_reported_codex", "identity_verified": False,
            "active_item": copy.deepcopy(active), "items": items, "ready_items": ready,
            "reports_ready": reports, "active_external_nodes": workers,
            "can_continue": run["state"] not in TERMINAL and bool(active or ready or reports),
            "should_wait": bool(workers and not (active or ready or reports)),
            "next_action": "refresh_controller_work" if phase == "stale_inputs" else "inspect_reports" if reports else "continue_work" if active else "start_ready_work" if ready else "wait_for_dependency" if workers else "review_completion"}

    def display_nodes(self, run, view):
        nodes = copy.deepcopy(run["nodes"])
        mothers = {n["id"] for n in run["definition"]["nodes"] if n["node_type"] == "codex"}
        for ident in mothers:
            node = nodes[ident]
            node.update(dependency_state=node["state"], caller_phase=view["phase"], caller_identity_verified=False)
            if view["active_item"]:
                node.update(state="running", caller_lease_until=view["active_item"]["lease_until"])
            elif view["ready_items"] and run["state"] not in TERMINAL:
                node["state"] = "awaiting_plan"
        return nodes

    def update(self, run, decision):
        op = decision.get("op")
        fields = {"enqueue": {"items"}, "start": {"item_id", "lease_s"}, "heartbeat": {"item_id", "lease_s"},
                  "finish": {"item_id", "summary", "verdict", "reports", "issues"}, "cancel": {"item_id", "reason"}, "ack_reports": {"reports"}}
        if op not in fields or set(decision) - {"type", "op", *fields.get(op, ())}:
            raise ValueError("Invalid controller work decision")
        controller = run.setdefault("controller_work", {"items": {}, "seen": {}})
        items = controller["items"]
        if op == "enqueue":
            incoming = decision.get("items")
            if not isinstance(incoming, list) or not 1 <= len(incoming) <= 16 or len(items) + len(incoming) > 64:
                raise ValueError("Controller work queue exceeds its bounded capacity")
            prepared = {}
            for spec in incoming:
                if not isinstance(spec, dict) or set(spec) - {"id", "kind", "instruction", "paths", "depends_on", "wait_for", "access"}:
                    raise ValueError("Invalid controller work item")
                ident = text(spec.get("id"), "controller item id")
                if ident in items or ident in prepared:
                    raise ValueError("Controller item identity already exists")
                kind = spec.get("kind")
                if kind not in {"research", "analysis", "review", "verification", "planning"}:
                    raise ValueError("Invalid controller work kind")
                paths, deps = spec.get("paths", []), spec.get("depends_on", [])
                allowed = (run.get("execution") or {}).get("writable_paths", [])
                if (not isinstance(paths, list) or len(paths) > 32 or any(not isinstance(p, str) or p not in allowed for p in paths)
                        or len(set(paths)) != len(paths)):
                    raise ValueError("Controller reads cannot enlarge the execution scope")
                if (not isinstance(deps, list) or len(deps) > 32 or any(not isinstance(d, str) or d not in run["nodes"] for d in deps)
                        or len(set(deps)) != len(deps)):
                    raise ValueError("Invalid controller node dependencies")
                access, wait_for = spec.get("access", "snapshot"), spec.get("wait_for", "success")
                if access not in {"snapshot", "current"} or wait_for not in {"success", "terminal"}:
                    raise ValueError("Invalid controller read/dependency mode")
                prepared[ident] = {"id": ident, "kind": kind, "instruction": text(spec.get("instruction"), "controller instruction", 4000),
                    "paths": paths, "depends_on": deps, "access": access, "wait_for": wait_for,
                    "status": "queued", "created_at": now_ms(), "starts": 0}
            items.update(prepared)
            return
        if op == "ack_reports":
            reports = decision.get("reports")
            if not isinstance(reports, list) or not 1 <= len(reports) <= 64:
                raise ValueError("Invalid controller report acknowledgements")
            current = {r["node_id"]: r["fingerprint"] for r in self.inbox(run)}
            for report in reports:
                if (not isinstance(report, dict) or set(report) != {"node_id", "fingerprint"}
                        or not isinstance(report.get("node_id"), str) or report["node_id"] not in current
                        or current[report["node_id"]] != report.get("fingerprint")):
                    raise ValueError("Controller acknowledgement must bind the current exact report")
            controller["seen"].update({r["node_id"]: r["fingerprint"] for r in reports})
            return
        ident = text(decision.get("item_id"), "controller item id")
        if ident not in items or items[ident]["status"] == "cancelled" or (
                items[ident]["status"] == "completed" and not self.known_stale(run, items[ident])):
            raise ValueError("Controller work item is missing or terminal")
        item, active = items[ident], self.active(run)
        if op == "start":
            if active is not None:
                raise ValueError("Finish or release the current controller item first")
            blockers = self.blockers(run, item)
            if blockers:
                raise ValueError("controller_item_blocked: " + ",".join(blockers))
            if item["starts"] >= 16:
                raise ValueError("Controller work restart bound reached")
            lease = integer(decision.get("lease_s", 120), "controller lease", 15, 600)
            sources = self.operators._assignment_files(run.get("execution"), item["paths"], {}, blocking=False) if item["paths"] else []
            if item["status"] != "queued":
                item.setdefault("history", []).append({k: copy.deepcopy(item.get(k)) for k in
                    ("started_at", "completed_at", "status", "summary")})
            item.pop("summary", None)
            item.pop("completed_at", None)
            item.update(status="running", started_at=now_ms(), lease_until=now_ms() + lease * 1000,
                        starts=item["starts"] + 1, sources=sources,
                        dependency_sources={node_id: self.report_identity(node_id, run["nodes"][node_id]) for node_id in item["depends_on"]})
        elif op == "cancel":
            item.update(status="cancelled", summary=text(decision.get("reason"), "controller cancellation", 2000), completed_at=now_ms())
        else:
            if active is None or active["id"] != ident:
                raise ValueError("Controller claim expired or is not running")
            if op == "heartbeat":
                lease = integer(decision.get("lease_s", 120), "controller lease", 15, 600)
                item["lease_until"] = now_ms() + lease * 1000
            elif op == "finish":
                summary = text(decision.get("summary"), "controller result summary", 4000)
                verdict = decision.get("verdict")
                if verdict not in {None, "accepted", "needs_revision"}:
                    raise ValueError("Invalid controller verification verdict")
                if verdict is not None and item["kind"] not in {"review", "verification"}:
                    raise ValueError("Only controller verification/review can return a verdict")
                proposals = self.operators.caller_review_proposals(run, decision.get("reports"), decision.get("issues")) if verdict == "needs_revision" else []
                stale = self.known_stale(run, item)
                if item["access"] == "current" and item["paths"]:
                    current = self.operators._assignment_files(run["execution"], item["paths"], {}, blocking=False)
                    stale = stale or source_identity(current) != source_identity(item["sources"])
                item.update(status="stale" if stale else "completed", summary=summary, completed_at=now_ms(), verdict=verdict)
                if not stale:
                    existing = run.setdefault("review_proposals", []) if proposals else []
                    existing.extend(p for p in proposals if not any(old["digest"] == p["digest"] for old in existing))
