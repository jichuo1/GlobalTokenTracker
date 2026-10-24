"""Caller-Codex planning, bounded operator execution and typed report routing.

Codex is an actual caller, never a label for an external model or an auto-spawned
agent. Reports may request corrections; only coordinate() creates new authority.
All model jobs still use RouterService's shared dispatch and usage accounting.
"""

from concurrent.futures import ThreadPoolExecutor
import copy
import difflib
import hashlib
import json
import os
import threading
import time
import uuid

from .models import Task, now_ms
from .scheduling import INFORMATION_PROMPT, MODEL_TYPES, TERMINAL, integer, reconcile_information, report_dependencies, text


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, ensure_ascii=False).encode()).hexdigest()


class OperatorWorkflow:
    def __init__(self, scheduling):
        self.owner = scheduling
        self.service = scheduling.service
        self.completion_lock = threading.Lock()
        self.completion_order = 0
        with self.service.store.lock, self.service.store.conn:
            self.service.store.conn.execute("CREATE TABLE IF NOT EXISTS workflow_reports("
                "seq INTEGER PRIMARY KEY AUTOINCREMENT,run_id TEXT NOT NULL,event_id TEXT NOT NULL,"
                "payload TEXT NOT NULL,UNIQUE(run_id,event_id))")
            for row in self.service.store.conn.execute("SELECT payload FROM visual_runs").fetchall():
                prior = json.loads(row[0])
                for node in prior["nodes"].values():
                    for version in [*node.get("history", []), node]:
                        self.completion_order = max(self.completion_order, (version.get("operation_result") or {}).get("completed_order", 0))

    def events(self, run_id, cursor=0, limit=32):
        self.owner.status(run_id)
        integer(cursor, "cursor", 0, 2**53 - 1)
        integer(limit, "limit", 1, 64)
        with self.service.store.lock:
            rows = self.service.store.conn.execute("SELECT seq,payload FROM workflow_reports WHERE run_id=? AND seq>? ORDER BY seq LIMIT ?",
                (run_id, cursor, limit + 1)).fetchall()
        return {"events": [{"seq": seq, **json.loads(body)} for seq, body in rows[:limit]],
                "next_cursor": rows[min(limit, len(rows)) - 1][0] if rows else cursor, "has_more": len(rows) > limit}

    def _event(self, run, source, kind, payload, event_id):
        # Progress can traverse reviewer relays without paying for a model call.
        # A final review itself remains a separately attributed native report.
        node_types = {n["id"]: n["node_type"] for n in run["definition"]["nodes"]}
        destinations, seen = [source], set()
        while destinations:
            current = destinations.pop(0)
            for edge in run["definition"]["edges"]:
                if edge.get("kind", "task") != "report" or edge["from"] != current:
                    continue
                target = edge["to"]
                if target in seen:
                    continue
                seen.add(target)
                value = {"event_id": event_id, "source_node": source, "target_node": target, "kind": kind,
                         "work_revision": run["nodes"][source].get("work_revision", 1), "at": now_ms(), "payload": payload}
                with self.service.store.lock, self.service.store.conn:
                    self.service.store.conn.execute("INSERT OR IGNORE INTO workflow_reports(run_id,event_id,payload) VALUES(?,?,?)",
                        (run["id"], event_id + ":revision:" + str(value["work_revision"]) + ":" + target, json.dumps(value, ensure_ascii=False)))
                if node_types[target] != "codex" and (kind == "progress" or node_types[target] in {"operator", "merge", "gate", "output"}):
                    destinations.append(target)

    def topology(self, run_id):
        run = self.owner.status(run_id, full=True)
        graph = run["definition"]
        task_edges = [e for e in graph["edges"] if e.get("kind", "task") == "task"]
        levels, pending = {}, {n["id"] for n in graph["nodes"]}
        while pending:
            ready = [n for n in pending if all(e["from"] in levels for e in task_edges if e["to"] == n)]
            if not ready:
                raise ValueError("Invalid task topology")
            for node in ready:
                levels[node] = max([levels[e["from"]] + 1 for e in task_edges if e["to"] == node], default=0)
                pending.remove(node)
        routes = {r.id: r for r in self.service.catalog.routes}
        models = []
        with self.service.capacity:
            inflight = dict(self.service.inflight)
        for node in graph["nodes"]:
            route = routes.get(node["route_id"])
            if route is not None:
                health = self.service.store.health("route:" + route.id)
                models.append({"node_id": node["id"], "route": route.public(),
                    "health": {k: health.get(k) for k in ("verification_required", "last_verified_at", "last_error", "cooldown_until", "latency_ms")},
                    "source_inflight": inflight.get(route.source, 0),
                    "provider_inflight": inflight.get("provider:" + route.failure_domain, 0)})
        return {"run_id": run_id, "state": run["state"], "control_revision": run.get("control_revision", 0),
                "caller_role": "codex", "identity_verified": False, "definition": graph, "input": run["input"],
                "levels": levels, "nodes": run["nodes"], "model_routes": models, "assignments": run.get("assignments", {}),
                "token_budget": graph["token_budget"], "tokens_used": run["tokens_used"],
                "tokens_estimated": run["tokens_estimated"], "pending_actions": self.pending(run),
                "execution_scope": {k: v for k, v in (run.get("execution") or {}).items() if k != "commands"},
                "command_ids": list((run.get("execution") or {}).get("commands", {}))}

    def pending(self, run):
        assignments = run.get("assignments", {})
        operators = [n for n in run["definition"]["nodes"] if n["node_type"] == "operator"]
        missing = [n["id"] for n in operators if n["id"] not in assignments and run["nodes"][n["id"]]["state"] not in TERMINAL]
        items = [{"type": "assign", "node_ids": missing, "requires": "real_caller_codex", "topology_action": "topology"}] if missing else []
        if not run.get("decisions"):
            items = [{"type": "assign", "node_ids": [n["id"] for n in run["definition"]["nodes"] if n["node_type"] in MODEL_TYPES],
                      "requires": "real_caller_codex", "topology_action": "topology"}]
        if run.get("review_proposals"):
            items.append({"type": "review", "proposals": run["review_proposals"], "correction_requires_coordinate": True})
        if run.get("execution_complete") and not run.get("accepted"):
            items.append({"type": "accept", "requires": "real_caller_codex", "evidence_digest": self._evidence_digest(run)})
        return items

    @staticmethod
    def _evidence_digest(run):
        return digest({k: {p: v.get(p) for p in ("state", "job_id", "work_revision", "operation_result", "report_digest")}
                       for k, v in run["nodes"].items()})

    @staticmethod
    def _unresolved_information(run):
        requests = {q["id"]: q for node in run["nodes"].values() for q in node.get("information_requests", [])}
        resolved = {ident for node in run["nodes"].values() for ident in node.get("information_resolved_ids", [])}
        return [q for ident, q in requests.items() if ident not in resolved]

    def _assignment(self, run, value):
        allowed = {"node_id", "instruction", "paths", "input_range", "acceptance", "command_ids", "file_ranges"}
        if not isinstance(value, dict) or set(value) - allowed:
            raise ValueError("Invalid Codex assignment")
        ident = text(value.get("node_id"), "assignment node")
        spec = next((n for n in run["definition"]["nodes"] if n["id"] == ident), None)
        if spec is None or spec["node_type"] not in MODEL_TYPES:
            raise ValueError("Assignment must target a model/operator/reviewer node in this topology")
        node = run["nodes"][ident]
        if node["state"] in {"running", "executing"}:
            raise ValueError("Cannot replace in-flight paid work")
        paths = value.get("paths", [])
        if not isinstance(paths, list) or len(paths) > 64 or len(set(paths)) != len(paths):
            raise ValueError("Assignment paths must be a bounded unique list")
        if spec["node_type"] == "operator" and not paths and not (run.get("execution") or {}).get("commands"):
            raise ValueError("Operator assignment requires writable files or authorized commands")
        grant = run.get("execution")
        if paths and (not grant or any(p not in grant["writable_paths"] for p in paths)):
            raise ValueError("Assignment cannot expand the caller workspace grant")
        commands = value.get("command_ids", [])
        if not isinstance(commands, list) or len(commands) > 16 or any(not isinstance(c, str) or c not in (grant or {}).get("commands", {}) for c in commands):
            raise ValueError("Assignment cannot expand the authorized command registry")
        if spec["node_type"] == "operator" and not paths and not commands:
            raise ValueError("Operator assignment needs files or explicit command_ids")
        span = value.get("input_range", [0, len(run["input"])])
        if not isinstance(span, list) or len(span) != 2:
            raise ValueError("input_range requires start and end offsets")
        low = integer(span[0], "input start", 0, len(run["input"]))
        high = integer(span[1], "input end", low, len(run["input"]))
        acceptance = value.get("acceptance", [])
        if not isinstance(acceptance, list) or len(acceptance) > 16:
            raise ValueError("Assignment acceptance must be a bounded list")
        ranges = value.get("file_ranges", {})
        if not isinstance(ranges, dict) or set(ranges) - set(paths):
            raise ValueError("file_ranges must select assigned files only")
        for bounds in ranges.values():
            if not isinstance(bounds, list) or len(bounds) != 2:
                raise ValueError("file_ranges requires one-based inclusive line ranges")
            integer(bounds[0], "file start line", 1, 2**31 - 1)
            integer(bounds[1], "file end line", bounds[0], 2**31 - 1)
        return {"id": uuid.uuid4().hex, "node_id": ident, "instruction": text(value.get("instruction"), "assignment instruction", 16000),
                "paths": paths, "file_ranges": ranges, "command_ids": list(dict.fromkeys(commands)), "input_range": [low, high], "acceptance": [text(a, "acceptance", 2000) for a in acceptance],
                "files": self._assignment_files(grant, paths, ranges) if paths else [], "source": "caller_reported_codex"}

    def _assignment_files(self, grant, paths, ranges):
        files, used = [], 0
        for path in paths:
            item = self.service.operator.snapshot(grant, [path])[0]
            content = item["content"]
            if path in ranges:
                start, end = ranges[path]
                lines = content.splitlines(keepends=True)
                if not item["exists"] or end > len(lines):
                    raise ValueError("file range exceeds current file lines")
                excerpt = "".join(lines[start - 1:end])
                item.update(content=excerpt, line_range=[start, end], total_lines=len(lines),
                    content_chars=len(content), excerpt_only=start != 1 or end != len(lines))
            used += len(item["content"])
            if used > 48000:
                raise ValueError("assignment files exceed bounded context; supply file_ranges or split the assignment")
            files.append(item)
        return files

    @staticmethod
    def _validate_assignments(run, assignments):
        specs = {n["id"]: n for n in run["definition"]["nodes"]}
        ancestors = {}
        for ident in specs:
            reached, frontier = set(), [ident]
            while frontier:
                current = frontier.pop()
                for edge in run["definition"]["edges"]:
                    if edge.get("kind", "task") == "task" and edge["to"] == current and edge["from"] not in reached:
                        reached.add(edge["from"])
                        frontier.append(edge["from"])
            ancestors[ident] = reached
        operators = {ident: a for ident, a in assignments.items() if specs[ident]["node_type"] == "operator"}
        for ident, assignment in operators.items():
            for ancestor in ancestors[ident]:
                parent = operators.get(ancestor)
                if parent and (not set(assignment["paths"]) <= set(parent["paths"])
                               or not set(assignment["command_ids"]) <= set(parent["command_ids"])):
                    raise ValueError("Child operator grant must remain within its parent assignment")
                if parent:
                    for path, bounds in parent.get("file_ranges", {}).items():
                        if path not in assignment["paths"]:
                            continue
                        child = assignment.get("file_ranges", {}).get(path)
                        if child is None or child[0] < bounds[0] or child[1] > bounds[1]:
                            raise ValueError("Child operator line range cannot expand its parent assignment")
        for index, ident in enumerate(operators):
            for other in list(operators)[index + 1:]:
                if ident in ancestors[other] or other in ancestors[ident]:
                    continue
                overlap = {os.path.normcase(p) for p in operators[ident]["paths"]} & {os.path.normcase(p) for p in operators[other]["paths"]}
                if overlap:
                    raise ValueError("Parallel operator assignments overlap writable paths; add task edges to serialize them")

    def _bounded_files(self, grant, paths, ranges=None):
        result, remaining = [], 6000
        # Read one bounded file at a time rather than retaining the whole grant
        # in memory. Hashes cover full bytes; content clipping is explicit.
        for path in paths:
            item = self.service.operator.snapshot(grant, [path])[0]
            content = item["content"]
            selected = content
            if path in (ranges or {}):
                start, end = ranges[path]
                lines = content.splitlines(keepends=True)
                selected = "".join(lines[start - 1:min(end, len(lines))])
                item.update(line_range=[start, min(end, len(lines))], total_lines=len(lines))
            excerpt = selected[:min(3000, remaining)]
            result.append({**item, "content": excerpt, "content_chars": len(content),
                           "excerpt_only": len(excerpt) != len(content)})
            remaining -= len(excerpt)
        return result

    def _execute(self, run_id, node_id, assignment, grant, operations, cancel, progress, work_revision):
        paths = list(assignment["paths"])
        if any(op.get("op") == "run_command" for op in operations if isinstance(op, dict)):
            # A fixed caller command can legally update other files in its
            # workspace grant, so verify their final content as well.
            paths = list(grant["writable_paths"])
        before = self._bounded_files(grant, paths, assignment.get("file_ranges"))
        result = self.service.operator.execute(run_id, node_id, assignment["id"], grant,
            assignment["paths"], operations, cancel, progress)
        try:
            after = self._bounded_files(grant, paths, assignment.get("file_ranges"))
        except (OSError, ValueError):
            return {**result, "state": "unknown", "reason": "operation_post_files_unavailable", "before_files": before}
        diffs, remaining = [], 6000
        for old, new in zip(before, after):
            if old["sha256"] == new["sha256"]:
                continue
            unified = "".join(difflib.unified_diff(old["content"].splitlines(keepends=True), new["content"].splitlines(keepends=True),
                fromfile="before/" + old["path"], tofile="after/" + new["path"]))
            excerpt = unified[:remaining]
            remaining -= len(excerpt)
            diffs.append({"path": new["path"], "before_sha256": old["sha256"], "after_sha256": new["sha256"],
                          "diff": excerpt, "excerpt_only": old["excerpt_only"] or new["excerpt_only"] or len(excerpt) != len(unified)})
        with self.completion_lock:
            self.completion_order += 1
            order = self.completion_order
        return {**result, "before_files": before, "after_files": after, "file_diffs": diffs,
                "completed_order": order, "completed_at": now_ms(), "work_revision": work_revision,
                "assignment_id": assignment["id"]}

    @staticmethod
    def _latest_files(run):
        latest = {}
        for node in run["nodes"].values():
            for version in [*node.get("history", []), node]:
                result = version.get("operation_result") or {}
                order = (result.get("completed_order", 0), result.get("completed_at", 0))
                for file in result.get("after_files", []):
                    previous = latest.get(file["path"])
                    if previous is None or order > previous[0]:
                        latest[file["path"]] = (order, file)
        return {path: item[1] for path, item in latest.items()}

    def _verify_files_for_acceptance(self, run):
        expected = self._latest_files(run)
        if not expected:
            return
        try:
            actual = self._bounded_files(run["execution"], list(expected))
        except (OSError, ValueError):
            raise ValueError("workflow_workspace_changed: operation evidence cannot be verified") from None
        if any(file["sha256"] != expected[file["path"]]["sha256"] or file["exists"] != expected[file["path"]]["exists"] for file in actual):
            raise ValueError("workflow_workspace_changed: rerun review against current files before accepting")

    def coordinate(self, run_id, expected_revision, decision, event_id):
        text(event_id, "Codex decision identity")
        if not isinstance(decision, dict) or decision.get("type") not in {"assign", "revise", "retry_operations", "accept", "reject"}:
            raise ValueError("Unsupported Codex decision")
        with self.owner.lock:
            run = self.owner.status(run_id, full=True)
            if run.get("mode") != "operator_v1":
                raise ValueError("Not a Codex-controlled workflow")
            prior = next((d for d in run.get("decisions", []) if d["event_id"] == event_id), None)
            if prior:
                if prior["digest"] != digest(decision):
                    raise ValueError("codex_decision_identity_conflict")
                return {**self.owner.status(run_id), "deduplicated": True}
            integer(expected_revision, "expected control revision", 0, 2**53 - 1)
            if expected_revision != run.get("control_revision", 0):
                raise ValueError("workflow_control_revision_conflict")
            recovering = decision["type"] in {"retry_operations", "accept"} and run["state"] == "interrupted" and not any(n["state"] in {"unknown", "running", "executing"} for n in run["nodes"].values())
            if run["state"] in TERMINAL and not recovering:
                raise ValueError("Terminal/unknown workflows require a new reviewed request, never automatic replay")
            if recovering:
                self.owner.cancellations[run_id] = threading.Event()
            if self.owner.cancellations[run_id].is_set():
                raise ValueError("workflow_cancelled")
            gate = Task("extract", "Caller Codex decision", workflow_id=run["workflow_id"])
            gate._workflow_supervision_opt_out = not run["definition"]["luna_supervision"]
            self.service.require_dispatch(run["workflow_id"], task=gate)
            kind = decision["type"]
            if kind == "assign":
                if set(decision) - {"type", "assignments", "reason"}:
                    raise ValueError("Invalid assignment decision fields")
                rows = decision.get("assignments")
                if not isinstance(rows, list) or not 0 <= len(rows) <= 256 or (not rows and any(n["node_type"] in MODEL_TYPES for n in run["definition"]["nodes"])):
                    raise ValueError("Codex must supply bounded assignments")
                assignments = [self._assignment(run, row) for row in rows]
                if len({a["node_id"] for a in assignments}) != len(assignments):
                    raise ValueError("Duplicate assignment node")
                for assignment in assignments:
                    if assignment["node_id"] in run.get("assignments", {}):
                        raise ValueError("Existing assignments require an approved revise decision")
                prospective = {**run.get("assignments", {}), **{a["node_id"]: a for a in assignments}}
                self._validate_assignments(run, prospective)
                run["assignments"] = prospective
                for node in run["definition"]["nodes"]:
                    if node["node_type"] == "codex":
                        run["nodes"][node["id"]].update(state="succeeded", source="caller_reported_codex")
                run["state"] = "running"
            elif kind in {"revise", "retry_operations"}:
                fields = {"type", "proposal_digest", "reason", "node_id"} if kind == "retry_operations" else {"type", "assignment", "proposal_digest", "reason"}
                if set(decision) - fields:
                    raise ValueError("Invalid correction decision fields")
                proposals = run.get("review_proposals", [])
                proposal = next((p for p in proposals if p["digest"] == decision.get("proposal_digest")), None)
                if proposal is None:
                    raise ValueError("Correction must confirm an exact current reviewer proposal digest")
                if any(n["state"] in {"running", "executing"} for n in run["nodes"].values()):
                    raise ValueError("Wait for in-flight work before approving a correction")
                if kind == "retry_operations":
                    ident = text(decision.get("node_id"), "operation retry node")
                    prior_assignment = run.get("assignments", {}).get(ident)
                    if prior_assignment is None:
                        raise ValueError("No existing assignment for this operation retry")
                    assignment = self._assignment(run, {k: v for k, v in prior_assignment.items() if k in {
                        "node_id", "instruction", "paths", "command_ids", "input_range", "acceptance", "file_ranges"}})
                else:
                    assignment = self._assignment(run, decision.get("assignment"))
                if assignment["node_id"] not in proposal["operator_nodes"]:
                    raise ValueError("Correction assignment is outside the reviewed operator scope")
                ident = assignment["node_id"]
                node = run["nodes"][ident]
                if node["state"] == "unknown" or (node.get("operation_result") or {}).get("state") == "unknown":
                    raise ValueError("Unknown side effects require reconciliation; do not replay as a correction")
                if node.get("work_revision", 1) >= run["definition"].get("max_rounds", 8):
                    raise ValueError("workflow_correction_round_limit")
                cached_job_id = None
                if kind == "retry_operations":
                    cached = self.service.store.job(node["job_id"])
                    operations = (cached.get("results") or [{}])[0].get("content", {}).get("operations")
                    current = {f["path"]: f for f in assignment["files"]}
                    if cached["state"] != "succeeded" or not isinstance(operations, list) or not operations:
                        raise ValueError("Operation retry requires an already validated complete model report")
                    if any(not isinstance(op, dict) or op.get("op") not in {"write_text", "replace_text"} or op.get("path") not in current
                           or op.get("base_sha256") != current[op["path"]]["sha256"] for op in operations):
                        raise ValueError("Operation retry forbids commands and requires every original file baseline unchanged")
                    if (node.get("operation_result") or {}).get("state") not in {"failed", "conflict", "cancelled"}:
                        raise ValueError("Only known file-operation failures may reuse a model report")
                    cached_job_id = node["job_id"]
                prospective = {**run.get("assignments", {}), ident: assignment}
                self._validate_assignments(run, prospective)
                node.setdefault("history", []).append({k: copy.deepcopy(v) for k, v in node.items() if k != "history"})
                node.update(state="running" if cached_job_id else "pending", job_id=cached_job_id, work_revision=node.get("work_revision", 1) + 1)
                if cached_job_id:
                    node["model_report_reused"] = cached_job_id
                for key in ("operation_result", "report_digest", "review", "reason", "information_requests", "information_resolved_ids"):
                    node.pop(key, None)
                run["assignments"] = prospective
                # Reviewers are activated again only by the new revision's real
                # receipts. Successful downstream operators never replay here.
                frontier, reached = [ident], set()
                while frontier:
                    current = frontier.pop()
                    for edge in run["definition"]["edges"]:
                        if edge.get("kind", "task") == "report" and edge["from"] == current and edge["to"] not in reached:
                            reached.add(edge["to"])
                            frontier.append(edge["to"])
                renewed = {ident}
                for spec in run["definition"]["nodes"]:
                    if spec["id"] in reached and spec["node_type"] in {"reviewer", "model"}:
                        renewed.add(spec["id"])
                        state = run["nodes"][spec["id"]]
                        state.setdefault("history", []).append({k: copy.deepcopy(v) for k, v in state.items() if k != "history"})
                        state.update(state="pending", job_id=None, work_revision=state.get("work_revision", 1) + 1)
                        state.pop("report_digest", None)
                        state.pop("review", None)
                        state.pop("information_requests", None)
                        state.pop("information_resolved_ids", None)
                run["review_proposals"] = [p for p in proposals if p["node_id"] not in renewed]
                run.update(state="running", execution_complete=False)
            elif kind == "accept":
                if set(decision) - {"type", "evidence_digest", "reason"} or not run.get("execution_complete") or decision.get("evidence_digest") != self._evidence_digest(run):
                    raise ValueError("Acceptance must bind the current complete topology evidence digest")
                if run.get("review_proposals") or any(n["state"] != "succeeded" for n in run["nodes"].values()):
                    raise ValueError("Cannot accept unresolved review or failed/unknown nodes")
                for spec in run["definition"]["nodes"]:
                    node = run["nodes"][spec["id"]]
                    requires_review = spec["node_type"] == "reviewer" or (spec["node_type"] == "model" and
                        bool(report_dependencies(run["definition"], spec["id"])))
                    if (requires_review or node.get("review") is not None) and (node.get("review") or {}).get("accepted") is not True:
                        raise ValueError("Cannot accept a rejected or incomplete review")
                if self._unresolved_information(run):
                    raise ValueError("Cannot accept unresolved_information_requests")
                # File reads may wait on another workspace command. Do not hold
                # the controller lock; recheck identity/revision after the read.
                evidence = self._evidence_digest(run)
                self.owner.lock.release()
                try:
                    self._verify_files_for_acceptance(run)
                finally:
                    self.owner.lock.acquire()
                latest = self.owner.status(run_id, full=True)
                if latest.get("control_revision", 0) != expected_revision or self._evidence_digest(latest) != evidence:
                    raise ValueError("workflow_control_revision_conflict")
                if self.owner.cancellations[run_id].is_set():
                    raise ValueError("workflow_cancelled")
                run = latest
                run.update(state="succeeded", accepted=True, accepted_by="caller_reported_codex")
            else:
                if set(decision) - {"type", "reason"}:
                    raise ValueError("Invalid reject fields")
                run.update(state="cancelling", rejected_by="caller_reported_codex")
                self.owner.cancellations[run_id].set()
            run.setdefault("decisions", []).append({"event_id": event_id, "digest": digest(decision), "type": kind,
                "at": now_ms(), "source": "caller_reported_codex", "identity_verified": False})
            run["control_revision"] = run.get("control_revision", 0) + 1
            run["pending_actions"] = self.pending(run)
            self.owner._save(run)
            if recovering and kind == "retry_operations":
                thread = threading.Thread(target=self.owner._run, args=(run_id,), daemon=True, name="gtt-operator-reconcile")
                self.owner.threads[run_id] = thread
                thread.start()
            return self.owner.status(run_id)

    def _usage(self, run):
        ids = set(run.get("job_ids", []))
        used = estimated = reserved = 0
        for ident in ids:
            job = self.service.store.job(ident)
            used += job["tokens_used"]
            estimated += job["tokens_estimated"]  # a subset of used, not additive
        for node in run["nodes"].values():
            if node["state"] == "running" and node.get("job_id"):
                job = self.service.store.job(node["job_id"])
                reserved += max(0, node.get("reserved_budget", 0) - job["tokens_used"])
        run.update(tokens_used=used, tokens_estimated=estimated)
        return used, reserved

    @staticmethod
    def _reported_operators(run, target):
        specs = {n["id"]: n for n in run["definition"]["nodes"]}
        result, seen, pending = set(), set(), [target]
        while pending:
            current = pending.pop()
            for edge in run["definition"]["edges"]:
                if edge.get("kind", "task") != "report" or edge["to"] != current or edge["from"] in seen:
                    continue
                seen.add(edge["from"])
                pending.append(edge["from"])
                if specs[edge["from"]]["node_type"] == "operator":
                    result.add(edge["from"])
        return sorted(result)

    def _finish_model(self, run, spec, node, job, executor, futures):
        node.update(tokens_used=job["tokens_used"], tokens_estimated=job["tokens_estimated"], report_count=len(job["results"]))
        node["reports"] = [{"worker": r["worker"], "ok": r.get("ok", False), "summary": str((r.get("content") or {}).get("summary", ""))[:600], "excerpt_only": True} for r in job["results"][:8]]
        if job["state"] != "succeeded" or not job["results"] or not all(r.get("ok") for r in job["results"]):
            unknown = job["state"] == "interrupted" or any(a.get("unknown_paid_outcome") or (a.get("error") or {}).get("code") == "timeout" for a in job["attempts"])
            node.update(state="unknown" if unknown else "failed", error=job.get("error") or next((r.get("error") for r in job["results"] if r.get("error")), None))
            self._event(run, spec["id"], "result", {"state": node["state"], "job_id": job["job_id"]}, job["job_id"] + ":failed")
            return
        content = job["results"][0]["content"]
        node["report_digest"] = digest({"node_id": spec["id"], "job_id": job["job_id"],
            "work_revision": node.get("work_revision", 1), "content": content})
        node["evidence_refs"] = [job["job_id"]]
        inherited = node.get("information_requests", [])
        try:
            node["information_requests"] = reconcile_information(inherited, job)
        except ValueError as error:
            node.update(state="blocked", reason=str(error))
            return
        resolved = {q["id"] for q in inherited} - {q["id"] for q in node["information_requests"]}
        node["information_resolved_ids"] = sorted(set(node.get("information_resolved_ids", [])) | resolved)
        if spec["node_type"] == "operator":
            operations = content.get("operations")
            if not isinstance(operations, list):
                node.update(state="blocked", reason="operator_report_requires_operations")
                return
            assignment = run["assignments"][spec["id"]]
            sliced = {f["path"]: f for f in assignment["files"] if f.get("excerpt_only")}
            for operation in operations:
                if not isinstance(operation, dict) or operation.get("path") not in sliced:
                    continue
                excerpt = sliced[operation["path"]]["content"]
                if operation.get("op") != "replace_text" or not operation.get("old_text") or excerpt.count(operation["old_text"]) != 1:
                    reason = "sliced_file_requires_unique_replace_within_assigned_lines"
                    node.update(state="blocked", reason=reason)
                    run.setdefault("review_proposals", []).append({"node_id": spec["id"], "digest": node["report_digest"],
                        "operator_nodes": [spec["id"]], "job_id": job["job_id"], "summary": reason})
                    return
            node["state"] = "executing"
            # persist intent before the executor can cause any local side effect
            self.owner._save(run)
            grant = copy.deepcopy(run["execution"])
            grant["commands"] = {k: v for k, v in grant.get("commands", {}).items() if k in assignment["command_ids"]}
            snapshot = copy.deepcopy(run)
            def progress(value):
                self._event(snapshot, spec["id"], "progress", value, assignment["id"] + ":operation:" + digest(value))
            futures[spec["id"]] = executor.submit(self._execute, run["id"], spec["id"], copy.deepcopy(assignment),
                grant, operations, self.owner.cancellations[run["id"]], progress, node.get("work_revision", 1))
        else:
            node.update(state="succeeded", review=content.get("review"))
            report_sources = report_dependencies(run["definition"], spec["id"])
            if spec["node_type"] == "reviewer" or report_sources:
                review = content.get("review")
                if not isinstance(review, dict) or not isinstance(review.get("accepted"), bool):
                    node.update(state="blocked", reason="review_report_requires_explicit_accepted_boolean")
                elif not review["accepted"]:
                    operators = self._reported_operators(run, spec["id"])
                    run.setdefault("review_proposals", []).append({"node_id": spec["id"], "digest": node["report_digest"], "operator_nodes": operators, "job_id": job["job_id"], "summary": str(content.get("summary", ""))[:1200]})
            self._event(run, spec["id"], "review" if report_sources else "result", {"state": node["state"], "job_id": job["job_id"], "report_digest": node["report_digest"], "review": node.get("review")}, job["job_id"] + ":report")

    def run(self, run_id):
        futures = {}
        executor = ThreadPoolExecutor(max_workers=8, thread_name_prefix="gtt-operator")
        try:
            while not self.owner.stopping.is_set():
                with self.owner.lock:
                    run = self.owner.status(run_id, full=True)
                    if run["state"] in TERMINAL:
                        return
                    before = digest(run)
                    specs = {n["id"]: n for n in run["definition"]["nodes"]}
                    states = run["nodes"]
                    cancelled = self.owner.cancellations[run_id].is_set()
                    for ident, node in states.items():
                        if node["state"] == "running":
                            job = self.service.store.job(node["job_id"])
                            node["job_state"] = job["state"]
                            progress = next(iter(job.get("progress", {}).values()), {}).get("activity")
                            if progress != node.get("activity"):
                                node["activity"] = progress
                                if progress:
                                    self._event(run, ident, "progress", {"activity": progress, "job_id": job["job_id"]}, job["job_id"] + ":progress:" + digest(progress))
                            if cancelled:
                                self.service.cancel(node["job_id"])
                            if job["state"] in {"succeeded", "partial", "failed", "cancelled", "interrupted", "needs_codex"}:
                                if cancelled:
                                    node["state"] = "cancelled"
                                else:
                                    self._finish_model(run, specs[ident], node, job, executor, futures)
                        if node["state"] == "executing" and ident in futures and futures[ident].done():
                            try:
                                result = futures.pop(ident).result()
                            except ValueError as error:
                                result = {"state": "failed", "receipts": [], "reason": str(error)[:300]}
                            except Exception as error:
                                result = {"state": "unknown", "receipts": [], "reason": type(error).__name__}
                            node.update(state="succeeded" if result["state"] == "applied" else "unknown" if result["state"] == "unknown" else "cancelled" if result["state"] == "cancelled" else "blocked", operation_result=result)
                            if node["state"] == "blocked":
                                run.setdefault("review_proposals", []).append({"node_id": ident, "digest": digest({"node_id": ident, "job_id": node["job_id"], "work_revision": node.get("work_revision", 1), "result": result}), "operator_nodes": [ident],
                                    "job_id": node["job_id"], "summary": "Operation conflict/failure requires a new exact Codex assignment"})
                            self._event(run, ident, "operation_result", {"state": node["state"], "result": result, "job_id": node["job_id"]}, node["job_id"] + ":operations")
                    used, reserved = self._usage(run)
                    if cancelled:
                        for node in states.values():
                            if node["state"] not in TERMINAL | {"running", "executing"}:
                                node["state"] = "cancelled"
                        if not any(n["state"] in {"running", "executing"} for n in states.values()):
                            run["state"] = "cancelled"
                    elif run.get("decisions"):
                        for ident, spec in specs.items():
                            node = states[ident]
                            if node["state"] not in {"pending", "awaiting_assignment"} or spec["node_type"] == "codex":
                                continue
                            deps = [states[e["from"]] for e in run["definition"]["edges"] if e.get("kind", "task") == "task" and e["to"] == ident]
                            sources = [states[source] for source in report_dependencies(run["definition"], ident)]
                            if any(d["state"] in TERMINAL - {"succeeded"} for d in deps + sources):
                                node.update(state="blocked", reason="upstream_not_succeeded")
                                continue
                            if any(d["state"] != "succeeded" for d in deps + sources):
                                continue
                            inherited = {q["id"]: q for source in deps + sources for q in source.get("information_requests", [])}
                            resolved = {q for source in deps + sources for q in source.get("information_resolved_ids", [])}
                            node["information_requests"] = [q for key, q in inherited.items() if key not in resolved]
                            node["information_resolved_ids"] = sorted(resolved)
                            if spec["node_type"] not in MODEL_TYPES:
                                refs = list(dict.fromkeys(ref for source in deps + sources for ref in source.get("evidence_refs", [])))
                                if spec["node_type"] in {"gate", "output"} and node["information_requests"]:
                                    node.update(state="blocked", reason="unresolved_information_requests")
                                elif spec["node_type"] == "gate" and not refs:
                                    node.update(state="blocked", reason="gate_requires_model_evidence")
                                else:
                                    node.update(state="succeeded", local_node=spec["node_type"], evidence_refs=refs)
                                continue
                            assignment = run.get("assignments", {}).get(ident)
                            if spec["node_type"] == "operator" and not assignment:
                                node["state"] = "awaiting_assignment"
                                continue
                            if sum(n["state"] in {"running", "executing"} for n in states.values()) >= run["definition"]["max_parallel"]:
                                continue
                            if assignment and assignment["paths"]:
                                # A lower stage sees the real files produced by
                                # its successful ancestors, never stale snapshots.
                                assignment["files"] = self._assignment_files(run["execution"], assignment["paths"], assignment.get("file_ranges", {}))
                            span = assignment["input_range"] if assignment else [0, len(run["input"])]
                            related = []
                            for edge in run["definition"]["edges"]:
                                if edge["to"] == ident:
                                    src = states[edge["from"]]
                                    contents = [r.get("content") for r in self.service.store.job(src["job_id"])["results"] if r.get("ok")] if src.get("job_id") else []
                                    if specs[edge["from"]]["node_type"] == "operator":
                                        # A proposal is not proof of applied content. Actual
                                        # file snapshots below come from the broker future.
                                        contents = [{k: v for k, v in content.items() if k != "operations"} for content in contents]
                                    actual = src.get("operation_result") or {}
                                    related.append({"source_node": edge["from"], "kind": edge.get("kind", "task"), "reports": contents,
                                        "operation_result": {k: v for k, v in actual.items() if k not in {"before_files", "after_files", "file_diffs"}} if actual else None,
                                        "before_files": actual.get("before_files", []), "after_files": actual.get("after_files", []),
                                        "file_diffs": actual.get("file_diffs", []),
                                        "assignment": {k: v for k, v in run.get("assignments", {}).get(edge["from"], {}).items() if k != "files"}})
                            direct = {item["source_node"] for item in related}
                            relayed = [{"source_node": source, "work_revision": states[source].get("work_revision", 1),
                                "operation_result": states[source].get("operation_result")}
                                for source in self._reported_operators(run, ident) if source not in direct]
                            context = json.dumps({"input_slice": run["input"][span[0]:span[1]], "input_range": span,
                                "assignment": assignment, "upstream": related,
                                "report_evidence": relayed,
                                "unresolved_information_requests": node["information_requests"]}, ensure_ascii=False)
                            prompt = spec["role"] + "\n" + spec["task"]["prompt"] + INFORMATION_PROMPT
                            if assignment:
                                prompt += "\nCodex assignment: " + assignment["instruction"]
                            if spec["node_type"] == "reviewer" or sources:
                                prompt += '\n审阅真实操作收据与 after_files/file_diffs，串联回报的原始执行证据在 report_evidence；不将提案当作完成证明，excerpt_only 表示片段不是全文。报告 review:{"accepted":boolean,"issues":array}；存在错误或证据不足时 accepted=false，修正必须交真实 Codex 确认，不自行执行。'
                            task = {**spec["task"], "prompt": prompt, "context": context, "preferred_route": spec["route_id"],
                                "allowed_routes": [spec["route_id"], *spec["fallback_routes"]], "workflow_id": run["workflow_id"],
                                "work_key": "operator:" + run_id + ":" + ident + ":" + str(node.get("work_revision", 1))}
                            if spec["node_type"] in {"operator", "reviewer"}:
                                task.update(fanout=1, verify=False)
                            try:
                                budget = self.service.parse_task(task).token_budget
                            except ValueError as error:
                                node.update(state="blocked", reason=str(error)[:300])
                                continue
                            if used + reserved + budget > run["definition"]["token_budget"]:
                                if not any(n["state"] == "running" for n in states.values()):
                                    node.update(state="blocked", reason="workflow_token_budget_exhausted")
                                continue
                            # Never hold the UI/control lock through discovery.
                            self.owner._save(run)
                            self.owner.lock.release()
                            try:
                                try:
                                    submitted = self.service.submit(task, _supervision_required=run["definition"]["luna_supervision"],
                                        _cancellation_event=self.owner.cancellations[run_id], _operator_assignment=assignment if spec["node_type"] == "operator" else None)
                                except ValueError as error:
                                    submitted = {"state": "blocked", "reason": str(error)[:300]}
                            finally:
                                self.owner.lock.acquire()
                            # Merge only this admission result into the newest
                            # controller snapshot, preserving decisions/cancel.
                            latest = self.owner.status(run_id, full=True)
                            run = latest
                            states = run["nodes"]
                            node = states[ident]
                            if "job_id" not in submitted:
                                node.update(state="cancelled" if self.owner.cancellations[run_id].is_set() else "blocked", reason=submitted.get("reason", "work_already_covered"))
                            else:
                                node.update(state="running", job_id=submitted["job_id"], reserved_budget=budget)
                                if submitted["job_id"] not in run.setdefault("job_ids", []):
                                    run["job_ids"].append(submitted["job_id"])
                                reserved += budget
                    if not cancelled and run.get("decisions") and all(n["state"] in TERMINAL for n in states.values()):
                        run.update(state="awaiting_codex", execution_complete=True)
                    elif not cancelled and run["state"] != "succeeded" and not any(n["state"] in {"running", "executing"} for n in states.values()):
                        run["state"] = "awaiting_codex"
                    run["pending_actions"] = self.pending(run)
                    if digest(run) != before:
                        self.owner._save(run)
                    if run["state"] in TERMINAL:
                        return
                self.owner.stopping.wait(.2)
        except Exception as error:
            with self.owner.lock:
                run = self.owner.status(run_id, full=True)
                run.update(state="interrupted", runtime_error=type(error).__name__)
                for node in run["nodes"].values():
                    if node["state"] in {"running", "executing"}:
                        node["state"] = "unknown"
                self.owner._save(run)
        finally:
            self.owner.cancellations[run_id].set()
            executor.shutdown(wait=False, cancel_futures=True)
