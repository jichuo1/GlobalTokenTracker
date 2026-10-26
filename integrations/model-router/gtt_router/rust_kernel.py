"""Client for the Rust state owner and scheduling runtime.

Private job/run data is transferred once and then updated by revisioned deltas.
Native adapter credentials and model calls remain in Python. Transport errors
never restart the owner, switch backends, or replay writes.
"""

import json
import copy
import hashlib
import sqlite3
from collections import OrderedDict
from contextlib import nullcontext
from pathlib import Path
import queue
import subprocess
import threading

from .models import now_ms, WorkerFailure
from .controller_work import has_dispatch_decisions
from .processes import background_options

FRAME_LIMIT = 16 * 1024 * 1024
MESSAGE_LIMIT = 256 * 1024 * 1024


class RustKernel:
    def __init__(self, executable, timeout_s=10):
        path = Path(executable)
        if not path.is_absolute() or not path.is_file():
            raise ValueError("scheduler_kernel.executable must name an existing absolute file")
        self.lock = threading.Lock()
        self.responses = queue.Queue(maxsize=1)
        self.timeout_s = timeout_s
        self.broken = False
        self.persistent = False
        self.store_lock = None
        self.views = OrderedDict()
        self.catalog_key = None
        self.transport_stats = {"calls": 0, "request_bytes": 0, "response_bytes": 0}
        self.process = subprocess.Popen([str(path)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, bufsize=0, **background_options())
        self.reader = threading.Thread(target=self._read, daemon=True, name="gtt-rust-kernel-output")
        self.reader.start()
        try:
            health = self.call("health", {})
            if health.get("protocol") != 1 or not {"rank", "reserve", "workflow", "reports", "job_state"} <= set(health.get("features", [])):
                raise ValueError("scheduler_kernel_protocol_mismatch")
            self.features = set(health.get("features", []))
        except Exception:
            self.close()
            raise

    @classmethod
    def from_config(cls, config):
        value = config.get("scheduler_kernel", {"backend": "python"})
        if not isinstance(value, dict) or set(value) - {"backend", "executable", "timeout_s"}:
            raise ValueError("Invalid scheduler_kernel settings")
        if value.get("backend", "python") == "python":
            return None
        if value.get("backend") != "rust":
            raise ValueError("Unsupported scheduler_kernel backend")
        timeout = value.get("timeout_s", 10)
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not 0.1 <= timeout <= 60:
            raise ValueError("Invalid scheduler_kernel timeout_s")
        return cls(value.get("executable", ""), timeout)

    def _read(self):
        # Unbuffered pipes may return partial reads; accumulate to one bounded
        # newline frame without Python's byte-at-a-time FileIO.readline().
        pending = bytearray()
        parts, part_bytes, expected_size = [], 0, None
        try:
            while True:
                block = self.process.stdout.read(65536)
                if not block:
                    break
                pending.extend(block)
                if len(pending) > FRAME_LIMIT:
                    break
                while b"\n" in pending:
                    line, _, tail = pending.partition(b"\n")
                    pending = bytearray(tail)
                    self.transport_stats["response_bytes"] += len(line) + 1
                    value = json.loads(line)
                    if isinstance(value, dict) and "wire_chunk" in value:
                        size, data = value.get("size"), value.get("data")
                        if not isinstance(size, int) or not 0 <= size <= MESSAGE_LIMIT or not isinstance(data, str) or value["wire_chunk"] != len(parts) or expected_size not in (None, size):
                            raise ValueError("invalid response chunk")
                        expected_size = size
                        parts.append(data)
                        part_bytes += len(data.encode("utf-8"))
                        if part_bytes > size:
                            raise ValueError("response chunk overflow")
                        if value.get("last") is not True:
                            continue
                        if part_bytes != size:
                            raise ValueError("incomplete response chunks")
                        value = json.loads("".join(parts))
                        parts, part_bytes, expected_size = [], 0, None
                    elif parts:
                        raise ValueError("interleaved response chunks")
                    self.responses.put_nowait(value)
        except (OSError, ValueError, queue.Full):
            pass
        try:
            self.responses.put_nowait(None)
        except queue.Full:
            pass

    def call(self, operation, payload):
        with self.store_lock if operation.startswith("db_") and self.store_lock is not None else nullcontext():
            return self._call(operation, payload)

    def _call(self, operation, payload):
        encoded = json.dumps({"protocol": 1, "operation": operation, "payload": payload},
            ensure_ascii=False, allow_nan=False, separators=(",", ":")).encode("utf-8") + b"\n"
        if len(encoded) > MESSAGE_LIMIT:
            raise WorkerFailure("scheduler_kernel_frame_too_large", scope="task")
        with self.lock:
            if self.broken or self.process.poll() is not None:
                raise WorkerFailure("scheduler_kernel_unavailable", scope="task")
            # The watchdog also covers a child which stops reading stdin.
            timer = threading.Timer(self.timeout_s, self._abort)
            timer.daemon = True
            timer.start()
            try:
                self.transport_stats["calls"] += 1
                if len(encoded) <= FRAME_LIMIT:
                    self._write(encoded)
                else:
                    text = encoded.decode("utf-8")
                    for index, offset in enumerate(range(0, len(text), 1024 * 1024)):
                        end = min(len(text), offset + 1024 * 1024)
                        frame = {"wire_chunk": index, "size": len(encoded), "last": end == len(text), "data": text[offset:end]}
                        self._write(json.dumps(frame, ensure_ascii=False, separators=(",", ":")).encode("utf-8") + b"\n")
                response = self.responses.get(timeout=self.timeout_s)
                if response is None or not isinstance(response, dict):
                    raise OSError("kernel response unavailable")
                if response.get("error"):
                    error = response["error"]
                    if error.startswith("sqlite_integrity:"):
                        raise sqlite3.IntegrityError(error.split(":", 1)[1])
                    if error.startswith("sqlite_operational:"):
                        raise sqlite3.OperationalError(error.split(":", 1)[1])
                    if error.startswith("runtime_"):
                        if error == "runtime_revision_conflict":
                            self.invalidate_views()
                        if error == "runtime_merged_readonly":
                            raise ValueError("Merged source groups are permanently read-only")
                        if error == "runtime_event_conflict":
                            raise ValueError("event_id already identifies different immutable content")
                        if error == "runtime_event_capacity":
                            raise ValueError("Workflow coordination event capacity reached")
                        raise ValueError(error)
                    raise WorkerFailure("scheduler_kernel_rejected", scope="task")
                if "result" not in response:
                    raise OSError("kernel response invalid")
                return response["result"]
            except (OSError, queue.Empty):
                self._abort()
                raise WorkerFailure("scheduler_kernel_unavailable", scope="task") from None
            finally:
                timer.cancel()

    def _write(self, encoded):
        self.transport_stats["request_bytes"] += len(encoded)
        view = memoryview(encoded)
        while view:
            written = self.process.stdin.write(view)
            if not written:
                raise OSError("kernel pipe closed")
            view = view[written:]

    def _abort(self):
        self.broken = True
        try:
            self.process.kill()
        except OSError:
            pass

    def close(self):
        with self.lock:
            self._abort()
            self.process.wait(timeout=5)
            self.process.stdin.close()
            self.reader.join(timeout=5)
            self.process.stdout.close()

    @staticmethod
    def task(task):
        # Preserve Python Unicode-codepoint lengths; Rust never slices a prompt
        # at a UTF-8 byte offset. Bodies and runtime credentials stay in Python.
        value = task.public()
        value.update(output_cap=getattr(task, "_runtime_limits", {}).get("max_output_tokens", 524288),
            output_floor=getattr(task, "_output_floor", 256), output_target=getattr(task, "_output_target", 0),
            source_parallel=getattr(task, "_source_parallel", {}))
        return value

    def rank(self, task, routes, ledger, store, inflight, excluded, remaining_s, diversity, preferences):
        if self.persistent:
            with store.lock:
                payload = self.prepare_rank(task, routes, ledger, store, inflight, excluded, remaining_s, diversity, preferences)
                result = self.call("db_rank", payload)
            route_map = {r.id: r for r in routes}
            for candidate in result["candidates"]:
                candidate["route"] = route_map[candidate.pop("route_id")]
            return result["candidates"], result["rejected"]
        from .bridge import quota_for
        from .scheduling import settings, route_reason
        from .controls import DispatchBlocked
        try:
            selection, error = settings(store, selector_only=True), None
        except DispatchBlocked as failure:
            selection, error = {}, failure.code
        health, feedback = store.routing_snapshot(task.kind)
        route_map = {r.id: r for r in routes}
        values = [{**r.public(), "max_parallel": r.max_parallel, "quota": quota_for(r, ledger),
            "selection_reason": error or route_reason(selection, r)} for r in routes]
        result = self.call("rank", {"task": self.task(task), "routes": values, "health": health,
            "feedback": feedback, "selection": selection, "preferences": preferences or {},
            "now_ms": now_ms(), "inflight": inflight or {}, "excluded": sorted(excluded or []),
            "remaining_s": remaining_s if remaining_s is not None else (task.hard_deadline_s if task.timeout_mode == "activity" else task.deadline_s),
            "diversity": diversity or {}})
        for candidate in result["candidates"]:
            candidate["route"] = route_map[candidate.pop("route_id")]
        return result["candidates"], result["rejected"]

    def reserve(self, service, job, task, candidates, prompt_chars):
        history, _ = service.store.routing_snapshot(task.kind)
        rows = [{"route": {**c["route"].public(), "max_parallel": c["route"].max_parallel},
            "history_tokens": history.get("route:" + c["route"].id + ":kind:" + task.kind, {}).get("tokens_ema")}
            for c in candidates]
        fields = ("attempt_count", "tokens_used", "tokens_reserved", "exploration_tokens_used", "exploration_tokens_reserved")
        return self.call("reserve", {"task": self.task(task), "candidates": rows,
            "job": {**{k: getattr(job, k, 0) for k in fields}, "token_budget": job.task.token_budget,
                "max_attempts": job.task.max_attempts, "research_group": getattr(job, "research_group", False)},
            "prompt_chars": prompt_chars, "limits": service.limits, "inflight": service.inflight,
            "total_inflight": service.total_inflight, "active_codex": len(service._active_codex()),
            "exploration": getattr(task, "_exploration", False),
            "exploration_ratio": getattr(getattr(job, "request", None), "exploration_budget_ratio", 0)})

    @staticmethod
    def definition(definition):
        return {"nodes": [{"id": n["id"], "node_type": n["node_type"]} for n in definition["nodes"]],
            "edges": definition["edges"]}

    def workflow(self, run, cancelled=False):
        if self.persistent:
            base = getattr(run, "base", {})
            return self.call("db_workflow", {"run_id": run["id"], "cancelled": cancelled, "has_decisions": has_dispatch_decisions(run),
                "states": {key: value["state"] for key, value in run["nodes"].items()
                    if value["state"] != base.get("nodes", {}).get(key, {}).get("state")}})
        return self.call("workflow", {"definition": self.definition(run["definition"]),
            "nodes": {key: {"state": node["state"]} for key, node in run["nodes"].items()},
            "controlled": run.get("mode") == "operator_v1", "has_decisions": has_dispatch_decisions(run), "cancelled": cancelled})

    def job_state(self, job, require_nonempty=False):
        if self.persistent and job.get("job_id"):
            return self.call("db_job_state", {"job_id": job["job_id"], "require_nonempty": require_nonempty})
        return self.call("job_state", {"state": job["state"], "require_nonempty": require_nonempty,
            "results": [{"ok": r.get("ok", False)} for r in job.get("results", [])],
            "attempts": [{"unknown_paid_outcome": a.get("unknown_paid_outcome", False),
                "error": {"code": (a.get("error") or {}).get("code")}} for a in job.get("attempts", [])]})

    def invalidate_views(self):
        self.views.clear()

    def read_record(self, kind, ident):
        from .native_store import RuntimeRecord
        with self.store_lock:
            if self.broken or self.process.poll() is not None:
                raise WorkerFailure("scheduler_kernel_unavailable", scope="task")
            cached = self.views.get((kind, ident))
            # The single owner changes records only in acknowledged requests.
            # SQL writes and rollback invalidate these immutable client views;
            # every write still requires Rust's persisted revision check.
            if cached is not None:
                self.views.move_to_end((kind, ident))
                return RuntimeRecord(copy.deepcopy(cached[1]), cached[0], cached[1])
            reply = self.call("db_" + kind + "_get", {"id": ident, "since_revision": cached[0] if cached else None})
            if reply.get("missing"):
                self.views.pop((kind, ident), None)
                return None
            if reply.get("unchanged"):
                value = cached[1]
            else:
                value = reply["value"]
                self._cache(kind, ident, reply["revision"], value)
            return RuntimeRecord(copy.deepcopy(value), reply["revision"], value)

    def _cache(self, kind, ident, revision, value):
        self.views[(kind, ident)] = (revision, value)
        self.views.move_to_end((kind, ident))
        while len(self.views) > 128:
            self.views.popitem(last=False)

    def write_record(self, kind, ident, value, usage=(), events=()):
        from .native_store import patch
        with self.store_lock:
            if hasattr(value, "revision"):
                revision, base = value.revision, value.base
            elif (kind, ident) in self.views:
                revision, base = self.views[(kind, ident)]
            else:
                prior = self.read_record(kind, ident)
                revision, base = (prior.revision, prior.base) if prior is not None else (0, {})
            now = now_ms()
            changes = patch(base, dict(value))
            reply = self.call("db_" + kind + "_put", {"id": ident, "expected_revision": revision,
                "patch": changes, "now_ms": now, "usage": list(usage), "events": list(events)})
            saved = copy.deepcopy(dict(value))
            if kind == "job":
                saved.update(job_id=ident, created_at=reply["created_at"], updated_at=reply["updated_at"])
                if base:
                    saved["task"] = base["task"]
            self._cache(kind, ident, reply["revision"], saved)
            if hasattr(value, "revision"):
                value.revision, value.base = reply["revision"], saved
            return reply

    def prepare_rank(self, task, routes, ledger, store, inflight, excluded, remaining_s, diversity, preferences):
        from .scheduling import settings
        from .controls import DispatchBlocked
        values = [{**r.public(), "max_parallel": r.max_parallel, "quota_app": r.quota_app,
            "quota_account": r.quota_account} for r in routes]
        key = hashlib.sha256(json.dumps(values, ensure_ascii=False, separators=(",", ":")).encode()).hexdigest()
        if key != self.catalog_key:
            self.call("db_catalog", {"key": key, "routes": values})
            self.catalog_key = key
        try:
            selection, error = settings(store, selector_only=True), None
        except DispatchBlocked as failure:
            selection, error = {}, failure.code
        return {"task": self.task(task), "catalog_key": key, "selection": selection, "selection_error": error,
            "ledger": {"captured_at": ledger.get("captured_at"), "quota_ttl_s": ledger.get("quota_ttl_s", 300),
                "quotas": [{k: q.get(k) for k in ("app", "account_key", "captured_at", "resets_at", "remaining_percent")} for q in ledger.get("quotas", [])]},
            "preferences": preferences or {}, "now_ms": now_ms(), "inflight": inflight or {},
            "excluded": sorted(excluded or []), "diversity": diversity or {},
            "remaining_s": remaining_s if remaining_s is not None else (task.hard_deadline_s if task.timeout_mode == "activity" else task.deadline_s)}

    def admit(self, service, job, task, excluded, remaining_s, prompt_chars):
        with service.store.lock:
            payload = self.prepare_rank(task, service.catalog.routes, service.catalog.ledger, service.store,
                service.inflight, excluded, remaining_s, job.diversity, service.preference_for(task))
            fields = ("attempt_count", "tokens_used", "tokens_reserved", "exploration_tokens_used", "exploration_tokens_reserved")
            payload["reservation"] = {"job": {**{k: getattr(job, k, 0) for k in fields}, "token_budget": job.task.token_budget,
                "max_attempts": job.task.max_attempts, "research_group": getattr(job, "research_group", False)},
                "prompt_chars": prompt_chars, "limits": service.limits, "total_inflight": service.total_inflight,
                "active_codex": len(service._active_codex()), "exploration": getattr(task, "_exploration", False),
                "exploration_ratio": getattr(getattr(job, "request", None), "exploration_budget_ratio", 0)}
            return self.call("db_admit", payload)
