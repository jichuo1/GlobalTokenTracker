"""Scoped advisory communications and read-only supervision, never tool authority."""

import hashlib
import json
import math
import threading
import uuid
from urllib.parse import parse_qsl, urlsplit

from .hook_guard import public_url
from .models import now_ms, redact, reject_secrets

KINDS = {"assignment", "evidence", "conflict", "progress", "suggestion", "merge_proposal"}
ROLES = {"caller", "supervisor", "leader", "collector", "analyst", "checker", "members"}
MESSAGE_KEYS = {"to_group", "to_role", "kind", "summary", "evidence_refs"}


def text(value, name, limit=300, required=True):
    if not isinstance(value, str) or len(value) > limit or (required and not value.strip()):
        raise ValueError(name + " must be a bounded nonempty string")
    reject_secrets(value)
    return value


def integer(value, name, low=0, high=2**63-1):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or int(value) != value or not low <= value <= high:
        raise ValueError(name + " must be a bounded integer")
    return int(value)


def validate_communication(item):
    if not isinstance(item, dict) or set(item) - MESSAGE_KEYS:
        raise ValueError("Invalid communication fields; messages do not grant execution permissions")
    role = text(item.get("to_role", "leader"), "to_role", 100)
    if role not in ROLES and not (role.startswith("member-") and role[7:].isdigit() and 1 <= int(role[7:]) <= 31):
        raise ValueError("Unknown recipient role")
    kind = item.get("kind", "progress")
    if not isinstance(kind, str) or kind not in KINDS:
        raise ValueError("Unknown communication kind")
    refs = item.get("evidence_refs", [])
    if not isinstance(refs, list) or len(refs) > 4:
        raise ValueError("At most four evidence references per message")
    clean = []
    for ref in refs:
        if not isinstance(ref, dict) or set(ref) - {"group_id", "worker", "message_id", "url", "anchor"}:
            raise ValueError("Invalid evidence reference")
        row = {"group_id": text(ref.get("group_id"), "evidence group_id", 200)}
        if "worker" in ref:
            row["worker"] = integer(ref["worker"], "worker")
        if "message_id" in ref:
            row["message_id"] = text(ref["message_id"], "message_id", 200)
        if "url" in ref:
            if not isinstance(ref["url"], str) or len(ref["url"]) > 600 or not public_url(ref["url"]):
                raise ValueError("Evidence URL must be public")
            reject_secrets(ref["url"])
            parsed = urlsplit(ref["url"])
            sensitive = {"token", "accesstoken", "apikey", "key", "password", "authorization", "signature", "xamzsignature", "xamzcredential"}
            for key, value in parse_qsl(parsed.query) + parse_qsl(parsed.fragment):
                if "".join(c for c in key.lower() if c.isalnum()) in sensitive:
                    raise ValueError("Evidence URL contains credential/signature parameters")
                reject_secrets(value)
            row["url"] = ref["url"]
        if "anchor" in ref:
            row["anchor"] = text(ref["anchor"], "anchor", 300)
        if not ("worker" in row or "message_id" in row):
            raise ValueError("Evidence must identify its original report or message")
        clean.append(row)
    result = {"to_role": role, "kind": kind, "summary": text(item.get("summary"), "summary", 1200), "evidence_refs": clean}
    if item.get("to_group") is not None:
        result["to_group"] = text(item["to_group"], "to_group", 200)
    return result


class Coordination:
    def __init__(self, service):
        self.service, self.store = service, service.store
        self.lock = threading.RLock()
        self.samples = {}

    def _group(self, workflow_id, group_id, writable=False):
        row = self.store.job(text(group_id, "group_id", 200))
        if "group" not in row or row["task"].get("workflow_id") != workflow_id:
            raise ValueError("Group is outside the authorized workflow")
        if writable and row["state"] == "merged":
            raise ValueError("Merged source groups are read-only")
        return row

    def _refs(self, workflow_id, refs):
        result = []
        for ref in refs:
            group = self._group(workflow_id, ref["group_id"])
            row = dict(ref)
            if "worker" in ref:
                matches = [r for r in group.get("results", []) if r.get("worker") == ref["worker"]]
                if len(matches) != 1:
                    raise ValueError("Evidence report is missing or ambiguous")
                report = matches[0]
                row.update({k: report.get(k) for k in ("round", "slot", "role", "ok")})
                row["source"] = (report.get("route") or {}).get("source", "unknown")
                row["route_id"] = (report.get("route") or {}).get("route_id")
                row["attempt_ids"] = [str(ref["group_id"]) + ":" + str(i) for i, a in enumerate(group.get("attempts", [])) if a.get("worker") == ref["worker"]][-4:]
                row["uncertainties"] = [redact(str(u))[:250] for u in (report.get("content") or {}).get("uncertainties", [])[:4]]
            if "message_id" in ref:
                event = self.store.event(workflow_id, ref["message_id"])
                if event.get("from_group") != ref["group_id"]:
                    raise ValueError("Message provenance does not match its original group")
                row["event_id"] = event["event_id"]
            result.append(row)
        return result

    def _message(self, workflow_id, item, from_role, from_group=None, event_id=None, ttl_s=21600, origin=None):
        workflow_id = text(workflow_id, "workflow_id")
        item = validate_communication(item)
        if from_group:
            self._group(workflow_id, from_group, True)
        to_group = item.get("to_group")
        if to_group:
            target = self._group(workflow_id, to_group, True)
            if item["to_role"].startswith("member-") and not any(r["id"] == item["to_role"] for r in target["group"]["roles"]):
                raise ValueError("Unknown recipient seat")
        elif item["to_role"] not in {"caller", "supervisor"}:
            raise ValueError("A group recipient requires to_group")
        payload = {"type": "message", **item, "from_group": from_group,
                   "from_role": from_role, "evidence_refs": self._refs(workflow_id, item["evidence_refs"]),
                   "authority": "advisory_only", "origin": origin or {"source": "caller_reported"}}
        event_id = text(event_id or uuid.uuid4().hex, "event_id", 200)
        ttl_s = integer(ttl_s, "ttl_s", 60, 604800)
        result = self.store.append_event(workflow_id, event_id, payload, ttl_s)
        return {**result, "workflow_id": workflow_id, "advisory_only": True}

    def message(self, workflow_id, to_role, kind, summary, to_group=None, evidence_refs=None,
                from_group=None, from_role="caller", event_id=None, ttl_s=21600):
        if from_role != "caller":
            raise ValueError("Caller message API cannot impersonate a supervisor or native role")
        return self._message(workflow_id, {"to_group": to_group, "to_role": to_role, "kind": kind,
                             "summary": summary, "evidence_refs": evidence_refs or []}, from_role,
                             from_group, event_id, ttl_s)

    def emit_report(self, job, result, communications):
        if not isinstance(communications, list) or len(communications) > (4 if result.get("role") == "leader" else 2):
            raise ValueError("Too many role communications")
        clean = [validate_communication(item) for item in communications]
        for index, item in enumerate(clean):
            item.setdefault("to_group", job.id if item["to_role"] not in {"caller", "supervisor"} else None)
            if result.get("role") != "leader" and item.get("to_group") not in {None, job.id}:
                raise ValueError("Members communicate within their own group; peer sharing goes through leaders")
            self._message(job.task.workflow_id, item, result.get("role", "member"), job.id,
                "role:" + job.id + ":" + str(result["worker"]) + ":" + str(index),
                origin={"group_id": job.id, "worker": result["worker"], "round": result.get("round"),
                        "slot": result.get("slot"), "source": (result.get("route") or {}).get("source", "unknown")})

    def events(self, workflow_id, cursor=0, limit=32, group_id=None, role=None):
        workflow_id = text(workflow_id, "workflow_id")
        cursor, limit = integer(cursor, "cursor"), integer(limit, "limit", 1, 64)
        if group_id:
            self._group(workflow_id, group_id)
        if role is not None:
            role = text(role, "role", 100)
        values, next_cursor, size = [], cursor, 0
        for item in self.store.events(workflow_id, cursor, 64):
            relevant = not group_id or item.get("to_group") == group_id or item.get("group_id") == group_id or item.get("from_group") == group_id
            if role is not None and item.get("type") == "message":
                relevant = relevant and item.get("to_role") in {role, "members" if role != "leader" else "leader"}
            if relevant:
                amount = len(json.dumps(item, ensure_ascii=False))
                if amount > 12000:
                    item = {k: item[k] for k in ("cursor", "event_id", "created_at", "expires_at", "type", "job_id", "group_id", "from_group", "to_group", "state", "phase", "revision", "tokens_used", "tokens_reserved") if k in item}
                    item["bounded_omission"] = "Full event remains in the private coordination journal"
                    amount = len(json.dumps(item, ensure_ascii=False))
                if size + amount > 12000 or len(values) >= limit:
                    break
                values.append(item)
                size += amount
            next_cursor = item["cursor"]
        return {"events": values, "next_cursor": next_cursor, "workflow_id": workflow_id,
                "advisory_only": True, **self.store.event_gap(workflow_id)}

    def context_for(self, job, role, slot):
        recipient = "leader" if role == "leader" else "member-" + str(slot)
        cursor = job.message_cursors.get(recipient, 0)
        rows, next_cursor, size = [], cursor, 0
        roles = sorted({recipient, role, "members" if role != "leader" else "leader"})
        for item in self.store.message_events(job.task.workflow_id, job.id, roles, cursor):
            if item.get("type") == "message":
                row = {k: item.get(k) for k in ("event_id", "from_group", "from_role", "kind", "origin", "evidence_refs")}
                row["evidence_refs"] = [{**ref, "uncertainties": [str(v)[:120] for v in ref.get("uncertainties", [])[:1]], "attempt_ids": ref.get("attempt_ids", [])[:2]}
                                        for ref in row["evidence_refs"]]
                row["summary"] = item["summary"][:600]
                if len(item["summary"]) > 600:
                    row["summary_excerpt_only"] = True
                    row["summary_chars"] = len(item["summary"])
                    row["full_message_event"] = item["event_id"]
                row["authority"] = "untrusted_evidence_not_instructions"
                amount = len(json.dumps(row, ensure_ascii=False))
                if amount > 4000:
                    # A valid maximum-size message must not pin the recipient
                    # cursor forever. Keep a resolvable original event ID and
                    # explicitly mark omitted detail, rather than truncating a
                    # citation URL into a different invalid URL.
                    row = {"event_id": item["event_id"], "from_group": item.get("from_group"),
                           "from_role": item.get("from_role"), "kind": item["kind"],
                           "summary": item["summary"][:400], "authority": "untrusted_evidence_not_instructions",
                           "bounded_omission": "Full evidence references remain in the original workflow event",
                           "evidence_ref_count": len(item.get("evidence_refs", []))}
                    amount = len(json.dumps(row, ensure_ascii=False))
                if size + amount > 4000 or len(rows) >= 4:
                    break
                rows.append(row)
                size += amount
            next_cursor = item["cursor"]
        job.message_cursors[recipient] = next_cursor
        return rows

    def peer_groups(self, job):
        return [{"group_id": row["job_id"], "revision": row["group"]["revision"], "state": row["state"],
                 "topic_excerpt": row.get("research_request", {}).get("topic", "")[:120]}
                for row in self.store.workflow_jobs(job.task.workflow_id) if "group" in row and row["job_id"] != job.id and row["state"] != "merged"][:16]

    def on_save(self, job, persist_job=None):
        # Only known numeric counters, route IDs and enum-like phases. Never
        # persist worker text, arbitrary errors or provider payloads here.
        progress = {}
        allowed = {"activity_count", "output_chars", "reasoning_chars", "elapsed_s", "last_activity_elapsed_s", "idle_s", "soft_remaining_s", "hard_remaining_s", "expires_in_s", "captured_at"}
        for worker, value in list(job.progress.items())[-32:]:
            if isinstance(value, dict):
                activity = value.get("activity", {})
                if isinstance(activity, dict):
                    progress[str(worker)] = {k: v for k, v in activity.items() if k in allowed and (v is None or isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v))}
        payload = {"type": "job_state", "job_id": job.id, "group_id": job.id if getattr(job, "research_group", False) else None,
                   "state": job.state, "revision": getattr(job, "revision", 0),
                   "phase": getattr(job, "phase", job.state), "tokens_used": job.tokens_used,
                   "tokens_estimated": job.tokens_estimated, "tokens_reserved": job.tokens_reserved,
                   "progress": progress, "attempt_states": [{"worker": a.get("worker"), "state": a.get("state"), "route_id": a.get("route_id")} for a in job.attempts[-32:]]}
        for brief, attempt in zip(payload["attempt_states"], job.attempts[-32:]):
            code = (attempt.get("error") or {}).get("code")
            if isinstance(code, str) and len(code) <= 80 and code.isascii() and code.replace("_", "").isalnum():
                brief["error_code"] = code
            if attempt.get("token_measurement") in {"reported", "estimated"}:
                brief["token_measurement"] = attempt["token_measurement"]
            if attempt.get("slice_provenance"):
                brief["slice"] = attempt["slice_provenance"]
                brief["support_request_ids"] = attempt.get("support_request_ids", [])
            if attempt.get("role"):
                brief["role"] = attempt["role"]
        resolved = []
        if getattr(job, "research_group", False):
            requests = self.service.groups._request_rows(job)
            unresolved = self.service.groups._unresolved_requests(job)
            resolved = sorted(set(requests) - set(unresolved))
            payload["information_status"] = {"total": len(requests), "unresolved_count": len(unresolved),
                "resolved_request_ids": resolved[:64], "unresolved_request_ids": sorted(unresolved)[:64]}
        digest = hashlib.sha256(json.dumps(payload, sort_keys=True).encode()).hexdigest()
        structure = {k: v for k, v in payload.items() if k != "progress"}
        structure["activity_kinds"] = {str(worker): value.get("activity", {}).get("last_kind")
            for worker, value in job.progress.items() if isinstance(value, dict) and
            isinstance(value.get("activity"), dict) and value["activity"].get("last_kind") in
            {"output", "reasoning", "tool_start", "tool_complete", "tool_result", "native_phase"}}
        structural_digest = hashlib.sha256(json.dumps(structure, sort_keys=True).encode()).hexdigest()
        with self.lock:
            previous = self.samples.get(job.id)
            if previous and (previous[0] == digest or previous[1] == structural_digest and now_ms() - previous[2] < 30000):
                if persist_job is not None:
                    persist_job([])
                return
            gap_recorded = False
            try:
                events = [("state:" + job.id + ":" + digest, payload)]
                events.extend(("information-response:" + job.id + ":" + request_id,
                    {"type": "information_response", "from_group": job.id, "request_id": request_id,
                     "status": "resolved", "authority": "evidence_delivery_requires_review"}) for request_id in resolved)
                if persist_job is not None:
                    now = now_ms()
                    result = persist_job([{"workflow_id": job.task.workflow_id, "event_id": ident,
                        "payload": json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")),
                        "now_ms": now, "ttl_s": 21600} for ident, value in events])
                    gap_recorded = result.get("event_gap_recorded", False)
                    if result.get("event_error") == "runtime_event_capacity":
                        raise ValueError("Workflow coordination event capacity reached")
                    if result.get("event_error"):
                        raise ValueError("event_id already identifies different immutable content")
                else:
                    for ident, value in events:
                        self.store.append_event(job.task.workflow_id, ident, value)
            except ValueError as error:
                if "capacity" not in str(error):
                    raise
                if not gap_recorded:
                    self.store.event_gap(job.task.workflow_id, True)
                return  # journal saturation never stops paid model cleanup
            self.samples[job.id] = (digest, structural_digest, now_ms())
            if len(self.samples) > 4096:
                self.samples.pop(next(iter(self.samples)))

    @staticmethod
    def public_progress(values, frozen_at=None, attempts=None):
        public = {}
        latest = {str(a.get("worker")): a for a in attempts or []}
        for worker, value in list(values.items())[-32:]:
            if not isinstance(value, dict):
                continue
            row = {}
            if isinstance(value.get("phase"), str) and value["phase"] in {"native_step", "model_activity", "thinking", "output", "tool", "waiting", "running"}:
                row["phase"] = value["phase"]
            activity = value.get("activity", {})
            if not isinstance(activity, dict):
                activity = {}
            freeze = frozen_at
            attempt = latest.get(str(worker), {})
            if attempt.get("state") in {"succeeded", "failed", "cancelled"}:
                started, duration = attempt.get("started_at"), attempt.get("duration_ms", 0)
                ended = started + duration if isinstance(started, (int, float)) and isinstance(duration, (int, float)) else activity.get("captured_at", now_ms())
                freeze = ended if freeze is None else min(freeze, ended)
            projection_at = now_ms() if freeze is None else min(now_ms(), freeze)
            if isinstance(activity, dict):
                row["activity"] = {k: v for k, v in activity.items() if k in {
                    "activity_count", "output_chars", "reasoning_chars", "elapsed_s", "last_activity_elapsed_s", "idle_s", "soft_remaining_s", "hard_remaining_s", "expires_in_s"}
                    and (v is None or isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v))}
                if isinstance(activity.get("last_kind"), str) and activity["last_kind"] in {"output", "reasoning", "tool_start", "tool_complete", "tool_result", "native_phase"}:
                    row["activity"]["last_kind"] = activity["last_kind"]
                if isinstance(activity.get("timeout_mode"), str) and activity["timeout_mode"] in {"activity", "hard"}:
                    row["activity"]["timeout_mode"] = activity["timeout_mode"]
                captured = activity.get("captured_at")
                if isinstance(captured, int) and not isinstance(captured, bool) and 0 <= captured <= now_ms() + 5000:
                    # Display-only extrapolation from the last captured fact.
                    # It never invokes ActivityWatch.record(), persists a new
                    # event, extends a lease or produces an extra usage record.
                    prior_projection = activity.get("projected_at", captured)
                    if not isinstance(prior_projection, int) or isinstance(prior_projection, bool):
                        prior_projection = captured
                    base = max(captured, prior_projection)
                    age = max(0, projection_at - base) / 1000
                    for field in ("elapsed_s", "idle_s"):
                        if isinstance(row["activity"].get(field), (int, float)):
                            row["activity"][field] = round(row["activity"][field] + age, 2)
                    for field in ("soft_remaining_s", "hard_remaining_s", "expires_in_s"):
                        if isinstance(row["activity"].get(field), (int, float)):
                            row["activity"][field] = round(max(0, row["activity"][field] - age), 2)
                    row["activity"].update({"captured_at": captured, "projected_at": max(base, projection_at),
                                            "age_source": "derived_from_last_capture", "frozen": freeze is not None})
            public[str(worker)] = row
        return public

    def supervisor(self, action, workflow_id, group_ids=None, actor_state=None, proposal=None, event_id=None):
        workflow_id = text(workflow_id, "workflow_id")
        if action not in {"status", "report"}:
            raise ValueError("Supervisor action must be status or report")
        if action == "status" and (proposal is not None or event_id is not None):
            raise ValueError("Read-only supervisor status does not accept a proposal/event write")
        if group_ids is not None and (not isinstance(group_ids, list) or len(group_ids) > 32 or any(not isinstance(g, str) for g in group_ids) or len(set(group_ids)) != len(group_ids)):
            raise ValueError("group_ids must be a unique bounded list")
        for group_id in group_ids or []:
            self._group(workflow_id, group_id)
        if action == "report":
            if not isinstance(proposal, dict) or set(proposal) - {"kind", "summary", "group_ids", "evidence_refs"}:
                raise ValueError("Supervisor reports are bounded advisory proposals")
            groups = proposal.get("group_ids", [])
            if not isinstance(groups, list) or len(groups) > 4 or any(not isinstance(g, str) for g in groups) or len(set(groups)) != len(groups):
                raise ValueError("At most four groups in a supervisor proposal")
            for group_id in groups:
                self._group(workflow_id, group_id)
            item = {"to_role": "caller", "kind": proposal.get("kind", "suggestion"),
                    "summary": proposal.get("summary"), "evidence_refs": proposal.get("evidence_refs", [])}
            result = self._message(workflow_id, item, "supervisor", event_id=event_id,
                                   origin={"source": "caller_reported_supervisor", "group_ids": groups})
            return {**result, "approval_required_for_changes": True}
        jobs = self.store.workflow_jobs(workflow_id)
        frozen_states = {"succeeded", "failed", "partial", "cancelled", "merged", "needs_codex", "interrupted", "idle"}
        groups = []
        for row in jobs:
            if "group" in row and (group_ids is None or row["job_id"] in group_ids):
                group = self.service.groups.status(row["job_id"])
                group["progress"] = self.public_progress(row.get("progress", {}), row["updated_at"] if row["state"] in frozen_states else None, row.get("attempts", []))
                groups.append(group)
        reminders = []
        if actor_state is not None:
            if not isinstance(actor_state, dict) or set(actor_state) - {"phase", "last_progress_at", "active_until", "deadline_at", "idle_threshold_s"}:
                raise ValueError("actor_state may only report a bounded caller phase/window")
            phase = actor_state.get("phase")
            if not isinstance(phase, str) or phase not in {"thinking", "coding", "tool", "waiting", "idle"}:
                raise ValueError("Unknown caller phase")
            last = integer(actor_state.get("last_progress_at"), "last_progress_at")
            until = integer(actor_state.get("active_until", last), "active_until")
            deadline = integer(actor_state.get("deadline_at", 0), "deadline_at")
            threshold = integer(actor_state.get("idle_threshold_s", 300), "idle_threshold_s", 30, 3600) * 1000
            if last > now_ms() + 5000 or until > now_ms() + 3600000:
                raise ValueError("Caller progress timestamps exceed clock-skew/bounded phase lease")
            # Active thinking is caller-observed, never hidden chain-of-thought.
            # Silence alone is insufficient: there must also be deadline risk.
            if not (phase in {"thinking", "coding", "tool"} and until > now_ms()) and now_ms() - last >= threshold and 0 < deadline - now_ms() <= threshold:
                reminders.append({"kind": "mainline_deadline_risk", "reason": "Caller phase lease expired, no valid progress in its stated window, and deadline is near", "advisory_only": True})
        return {"workflow_id": workflow_id,
                "supervision": {"required_for_dispatch": True, "identity_verified": False,
                    "source": "caller_reported_lease", "active_monitors": [
                        {k: a[k] for k in ("id", "model", "workflow_id")}
                        for a in self.service._active_supervisors(workflow_id)]},
                "groups": groups[:32], "jobs": [{"job_id": r["job_id"], "state": r["state"], "updated_at": r["updated_at"],
                    "progress": self.public_progress(r.get("progress", {}), r["updated_at"] if r["state"] in frozen_states else None, r.get("attempts", []))} for r in jobs if "group" not in r][:32],
                "truncated": len(jobs) > 32, "reminders": reminders, "authority": "read_only_advisory", "hidden_thinking_access": False,
                "mainline_state_source": "caller_reported_unverified",
                **self.store.event_gap(workflow_id)}
