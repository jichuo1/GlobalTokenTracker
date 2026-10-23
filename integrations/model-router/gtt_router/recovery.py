"""Sparse idle-time generation probes; do not permanently blacklist a source."""

from pathlib import Path
import copy
import threading
import time
import uuid

from .adapters import ExecutionContext, validate_result
from .bridge import quota_for
from .models import Task, WorkerFailure, now_ms
from .controls import BLOCK_CODES, DispatchBlocked
from .store import SOURCE_CONTRACT_ERRORS

CONNECTIVITY_ERRORS = {"auth_failed", "quota_exhausted", "rate_limited", "timeout", "provider_request_failed", "adapter_error", "model_unavailable", *SOURCE_CONTRACT_ERRORS}


class RecoveryController:
    def __init__(self, service):
        self.service = service
        self.settings = service.config.get("recovery", {})
        self.stopping = threading.Event()
        self.active_cancel = None
        self.thread = None
        self.configuration_lock = threading.Lock()
        self.configure(self.settings)

    def configure(self, settings):
        with self.configuration_lock:
            self.settings = settings
            if self.settings.get("enabled", False) and self.thread is None and not self.stopping.is_set():
                self.thread = threading.Thread(target=self._run, name="gtt-recovery", daemon=True)
                self.thread.start()

    def _run(self):
        while not self.stopping.wait(self.settings.get("poll_s", 60)):
            try:
                if self.settings.get("enabled", False):
                    self.tick()
            except Exception:
                # Recovery is auxiliary; foreground work and the MCP service
                # remain usable when a source discovery/check itself fails.
                pass

    def yield_to_foreground(self):
        if self.active_cancel is not None:
            self.active_cancel.set()

    def stop(self):
        self.stopping.set()
        self.yield_to_foreground()
        if self.thread is not None:
            self.thread.join(timeout=35)

    def _keys(self, route):
        return ["route:" + route.id, "provider:" + route.failure_domain, "source:" + route.source]

    def recheck(self, route_id, workflow_id="default", *, supervision_required=True):
        if not isinstance(route_id, str) or not route_id or len(route_id) > 200:
            raise ValueError("route_id must be a bounded route identity")
        # Explicit requests bypass old cooldowns, never capacity or probe budgets.
        if not isinstance(workflow_id, str) or not workflow_id or len(workflow_id) > 200:
            raise ValueError("workflow_id must be a bounded workflow identity")
        if not isinstance(supervision_required, bool):
            raise ValueError("supervision_required must be a boolean")
        return self.tick(route_id=route_id, workflow_id=workflow_id, supervision_required=supervision_required)

    def tick(self, route_id=None, workflow_id=None, supervision_required=True):
        service = self.service
        profile = service.call_profile()
        with service.capacity:
            try:
                service.require_external_interface()
                service.require_scheduling()
                if workflow_id is None:
                    supervisors = service._active_supervisors()
                    if not supervisors:
                        raise DispatchBlocked("luna_supervisor_required")
                    workflow_id = supervisors[0]["workflow_id"]
                if supervision_required or route_id is None:
                    service.require_supervisor(workflow_id)
            except DispatchBlocked as failure:
                return {"checked": False, "reason": failure.code}
            workers = [a for a in service._active_codex() if a.get("role") != "supervisor"]
            if service.stopping or service.total_inflight or workers or len(service._active_codex()) >= service.limits.get("combined_parallel", 8) or any(j.state in {"queued", "running", "cancelling"} for j in [*service.jobs.values(),*service.groups.jobs.values()]):
                return {"checked": False, "reason": "foreground_busy"}
        budget = service.store.probe_budget(now_ms() - 3600000)
        if budget["count"] >= self.settings.get("max_probes_per_hour", 2) or budget["tokens"] >= self.settings.get("max_tokens_per_hour", 24000):
            return {"checked": False, "reason": "recovery_hourly_budget"}
        service.ensure_catalog()
        candidates = []
        for route in service.catalog.routes:
            try:
                service.require_scheduling(route)
            except DispatchBlocked:
                continue
            if route_id is not None and route.id != route_id:
                continue
            if not route.auth_hint or not route.enabled:
                continue
            if route.max_parallel <= 0 or service.limits.get("global_parallel", 4) <= 0 or service.limits.get("source_parallel", {}).get(route.source, route.max_parallel) <= 0:
                continue
            states = [service.store.health(key) for key in self._keys(route)]
            due = [s for s in states if (s["last_error"] in CONNECTIVITY_ERRORS or s.get("verification_required")) and s["next_probe_at"] and s["next_probe_at"] <= now_ms() and s["cooldown_until"] <= now_ms()]
            if not due and route_id is None:
                continue
            quota = quota_for(route, service.catalog.ledger)
            if quota["known"] and quota["remaining_percent"] <= 0:
                continue
            # Native IDE agents carry framework context even for tiny prompts.
            estimate = 20000 if route.adapter == "antigravity_cli" else 4096
            if budget["tokens"] + estimate > self.settings.get("max_tokens_per_hour", 24000):
                continue
            candidates.append((min(s["next_probe_at"] for s in due) if due else 0, route, estimate))
        if not candidates:
            return {"checked": False, "reason": "no_due_route"}
        _, route, estimate = min(candidates, key=lambda c: (c[0], c[1].id))
        keys = self._keys(route)
        cancel = threading.Event()
        with service.capacity:
            if service.total_inflight or service.stopping or len(service._active_codex()) >= service.limits.get("combined_parallel", 8) or any(j.state in {"queued", "running", "cancelling"} for j in [*service.jobs.values(),*service.groups.jobs.values()]):
                return {"checked": False, "reason": "foreground_busy"}
            service.total_inflight += 1
            service.inflight[route.source] = service.inflight.get(route.source, 0) + 1
            provider_key = "provider:" + route.failure_domain
            service.inflight[provider_key] = service.inflight.get(provider_key, 0) + 1
            self.active_cancel = cancel
        prompt = 'Use no tools. Return exactly this JSON: {"summary":"GTT_RECOVERY_OK","findings":[],"sources":[],"uncertainties":[],"complete":true}'
        timeout = max(self.settings.get("probe_timeout_s", 30), min(120, (service.store.health("route:" + route.id).get("latency_ms") or 20000) / 1000 * 1.5))
        if route_id is not None:
            timeout = max(60, timeout)
        task = Task(kind="extract", prompt=prompt, max_output_tokens=min(route.output_ceiling(4096),profile["limits"]["max_output_tokens"]), deadline_s=timeout, workflow_id=workflow_id)
        task._workflow_supervision_opt_out = not supervision_required
        directory = service.state_dir / "recovery-probes" / uuid.uuid4().hex
        context = ExecutionContext(cancel, time.monotonic() + timeout, directory)
        charged = 0
        metering = {"token_measurement": "estimated"}
        started = False
        started_at = now_ms()
        start_time = time.monotonic()
        def record_probe(success, code, tokens):
            service.store.record_probe(route, success, code, tokens, {
                **metering, "started_at": started_at,
                "duration_ms": int((time.monotonic() - start_time) * 1000),
            })
        try:
            adapter = service.adapters.get(route.adapter)
            if adapter is None:
                raise WorkerFailure("adapter_error")
            if route.adapter == "antigravity_cli":
                route.runtime.setdefault("pool_root", str(service.state_dir / "native-worker-slots"))
            context.check()
            service.require_dispatch(workflow_id, runtime=True, route=route, task=task)
            started = True
            execution_route = copy.copy(route)
            execution_route.runtime = dict(route.runtime) | {"gtt_call_options":profile["source_options"].get(route.source,{})}
            raw = adapter(execution_route, prompt, task, context)
            from .service import reported_tokens
            measured = reported_tokens(raw.get("usage"))
            charged = estimate if measured is None else measured
            metering = {"usage": raw.get("usage", {}), "token_measurement": "reported" if measured is not None else "estimated",
                        **{k: raw[k] for k in ("native_dedup_key", "native_backed") if k in raw}}
            context.check()
            result = validate_result(raw, False)
            if result["content"]["summary"] != "GTT_RECOVERY_OK":
                raise WorkerFailure("invalid_probe_marker")
            service.store.recovered(keys)
            record_probe(True, "generation_probe_ok", charged)
            return {"checked": True, "route_id": route.id, "success": True, "generation_verified": True}
        except WorkerFailure as failure:
            if failure.code in BLOCK_CODES and not started:
                return {"checked": False, "reason": failure.code}
            from .service import reported_tokens
            if failure.metering.get("generation_not_started") is True:
                started, charged = False, 0
                metering = {"generation_not_started": True, "token_measurement": "not_started"}
            if failure.usage:
                measured = reported_tokens(failure.usage)
                charged = measured if measured is not None else estimate
                metering = {"usage": failure.usage, "token_measurement": "reported" if measured is not None else "estimated", **failure.metering}
            if failure.code == "cancelled":
                service.store.defer_probe(keys, 60)
                record_probe(False, "yielded_to_foreground", charged if metering["token_measurement"] == "reported" else (estimate if started else 0))
                return {"checked": False, "reason": "yielded_to_foreground"}
            if failure.code not in {"auth_failed", "model_unavailable", "quota_exhausted"} and not charged and metering["token_measurement"] not in {"reported", "not_started"}:
                charged = estimate
            state = service.store.health("route:" + route.id)
            exponent = min(state["probe_failures"], 5)
            delay = min(self.settings.get("max_backoff_s", 21600), self.settings.get("initial_delay_s", 1800) * 2 ** exponent)
            if failure.code in CONNECTIVITY_ERRORS:
                service.record_failure(route, failure)
            service.store.defer_probe(keys, delay, failed=True)
            record_probe(False, failure.code, charged)
            return {"checked": True, "route_id": route.id, "success": False, "code": failure.code, "retry_after_s": delay}
        except Exception:
            service.record_failure(route, WorkerFailure("adapter_error", retry_after_s=30))
            service.store.defer_probe(keys, 1800, failed=True)
            record_probe(False, "probe_adapter_error", charged if metering["token_measurement"] == "reported" else estimate)
            return {"checked": True, "route_id": route.id, "success": False, "code": "probe_adapter_error"}
        finally:
            with service.capacity:
                service.total_inflight -= 1
                service.inflight[route.source] -= 1
                service.inflight[provider_key] -= 1
                self.active_cancel = None
                service.capacity.notify_all()
