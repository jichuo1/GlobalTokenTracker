"""Research groups share the normal worker/capacity/usage path.

Leaders propose bounded research actions. Seat and budget changes require an
explicit caller approval; model output never grants execution authority.
"""

from concurrent.futures import ThreadPoolExecutor, as_completed, wait, FIRST_COMPLETED
from dataclasses import asdict
from types import SimpleNamespace
from contextlib import ExitStack
import json
import threading
import time
import uuid

from .group_contract import GroupRequest, parse_leader_decision
from .models import Task, WorkerFailure, now_ms, reject_secrets
from .policy import rank_routes
from .service import Job, reported_tokens
from .adapters import validate_result
from .usage import clean_usage
from .slicing import SliceQueue, digest


class GroupJob(Job):
    def __init__(self, request):
        master = Task(kind="research", prompt=request.topic, context=request.context,
                      deadline_s=request.deadline_s, max_output_tokens=None,
                      token_budget=request.token_budget, max_attempts=request.max_rounds * max(27, request.max_slices * 3) + 6,
                      workflow_id=request.workflow_id, work_key=request.work_key,
                      context_label=request.context_label, base_revision=request.base_revision,
                      preference_scope=request.preference_scope, hard_deadline_s=request.deadline_s,
                      price_weight=request.price_weight, prefer_free=request.prefer_free,
                      prefer_new_models=request.prefer_new_models)
        super().__init__(master)
        self.research_group = True
        self.request = request
        self.member_count = request.member_count or min(8, max(3, 3 + len(request.context) // 16000 + len(request.topic) // 2000))
        self.round = 0
        self.phase = "queued"
        self.revision = 0
        self.leader_route = None
        self.roles = []
        self.decisions = []
        self.next_tasks = []
        self.pending_action = None
        self.blocked_routes = set()
        self.report_worker = 0
        self.notes = []
        self.deadline_at = self.created_at + int(request.deadline_s * 1000)
        self.calls = []
        self.avoid_until = {}
        self.expected_slots = []
        self.message_cursors = {}
        self.merged_sources = []
        self.merged_into = None
        self.coordinating = False
        self.active_calls = 0
        self.slice_state = None
        self.exploration_rounds = 0
        self.exploration_tokens_start = None
        self.exploration_active = False
        self.exploration_tokens_used = 0
        self.exploration_tokens_reserved = 0
        self.research_leads = []

    def detail(self):
        base = super().detail()
        return {**base, "deadline_at": self.deadline_at,
            "soft_deadline_at": min(base.get("soft_deadline_at", self.deadline_at), self.deadline_at),
            "research_request": asdict(self.request), "group": {
            "member_count": self.member_count, "round": self.round, "phase": self.phase,
            "revision": self.revision, "leader_route": self.leader_route, "roles": self.roles,
            "decisions": self.decisions, "next_tasks": self.next_tasks, "pending_action": self.pending_action,
            "blocked_routes": sorted(self.blocked_routes), "report_worker": self.report_worker,
            "notes": self.notes, "budget_total": self.task.token_budget,
            "calls":self.calls,"avoid_until":self.avoid_until,
            "expected_slots":self.expected_slots,
            "message_cursors": self.message_cursors, "merged_sources": self.merged_sources,
            "merged_into": self.merged_into, "coordinating": self.coordinating, "active_calls": self.active_calls,
            "slice_state": self.slice_state, "exploration_rounds": self.exploration_rounds,
            "exploration_tokens_start": self.exploration_tokens_start, "research_leads": self.research_leads,
            "exploration_active": self.exploration_active, "exploration_tokens_used": self.exploration_tokens_used,
            "exploration_tokens_reserved": self.exploration_tokens_reserved,
            "deadline_at": self.deadline_at}}


class ResearchGroups:
    def __init__(self, service):
        self.service = service
        self.jobs = {}
        self.lock = threading.RLock()
        self.stopping = False
        self.pool = ThreadPoolExecutor(max_workers=int(service.limits.get("max_active_groups", 3)), thread_name_prefix="gtt-group")

    def _task(self, request, *, kind, prompt, context="", web=False, quality=0, preferred=None, role=None):
        if role in {"collector", "analyst", "checker"}:
            prompt += (" Optional communications in your report JSON: at most two objects {to_role:leader|members|member-N,"
                       "kind:assignment|evidence|conflict|progress|suggestion,summary,evidence_refs:[{group_id,worker?,message_id?,url?,anchor?}]}. "
                       "These are advisory report fields forwarded by GTT within this group, not permission to contact agents, invoke tools or change budgets.")
            prompt += (" If evidence is insufficient, report information_requests:[{question,reason,context_range?:[start,end],report_workers?:[IDs]}],"
                       " at most two. Use original material offsets/report IDs; request only the missing evidence. These requests go to the leader for targeted support.")
            prompt += (" Proactively report up to two concrete research_leads {question,rationale,priority:low|normal|high,expected_value,evidence_refs:[]}."
                       " Identify blind spots and useful next investigations supported by the supplied evidence, without expanding scope or inventing findings.")
        task = Task(kind=kind, prompt=prompt, context=context, needs_web=web,
                    min_quality=quality, max_output_tokens=None, max_attempts=3,
                    deadline_s=min(900, request.deadline_s), hard_deadline_s=request.deadline_s,
                    token_budget=request.token_budget,
                    workflow_id=request.workflow_id, work_key=request.work_key,
                    context_label=request.context_label, base_revision=request.base_revision,
                    preferred_route=preferred, allow_fallback=True,
                    preference_scope=request.preference_scope,
                    price_weight=request.price_weight, prefer_free=request.prefer_free,
                    prefer_new_models=request.prefer_new_models,
                    output_mode=request.output_mode,
                    routing_strategy={"leader": "quality", "collector": "speed", "analyst": "quality", "checker": "reliability"}.get(role, "auto"))
        task._research_group = True
        task._role = role
        task._queue_grace_s = request.deadline_s
        return task

    def _candidates(self, task, job=None):
        self.service.ensure_catalog()
        excluded = job.blocked_routes | {r for r,until in job.avoid_until.items() if until>now_ms()} if job else set()
        candidates, _ = rank_routes(task, self.service.catalog.routes, self.service.catalog.ledger,
                                    self.service.store, self.service.inflight, excluded,
                                    max(0, job.deadline - time.monotonic()) if job else task.deadline_s,
                                    job.diversity if job else {}, self.service.preference_for(task))
        return candidates

    def plan(self, request, _member_count=None):
        req = GroupRequest.parse(request)
        job = GroupJob(req)
        if _member_count is not None:
            if not isinstance(_member_count, int) or isinstance(_member_count, bool) or not 3 <= _member_count <= 32:
                raise ValueError("Internal merged group seat bound must be 3..32")
            job.member_count = _member_count
        excerpt = req.context[:req.slice_target_chars] if req.slice_mode == "adaptive" else req.context
        lead_task = self._task(req, kind="logic_check", prompt=req.topic, context=excerpt, quality=req.leader_min_quality, role="leader")
        leaders = self._candidates(lead_task)
        roles, used = [], set()
        if leaders:
            leader = leaders[0]["route"]
            roles.append({"id": "leader", "role": "leader", "route": leader.public()})
            used.add(leader.failure_domain)
        else:
            roles.append({"id":"leader","role":"leader","route":None})
        for index in range(job.member_count - 1):
            role = ["collector", "analyst", "checker"][index % 3]
            task = self._task(req, kind=("research" if req.needs_web else "extract") if role == "collector" else "logic_check",
                              prompt=req.topic, context=excerpt, web=req.needs_web and role == "collector",
                              quality=req.worker_min_quality if role == "collector" else req.leader_min_quality, role=role)
            candidates = self._candidates(task)
            candidates.sort(key=lambda c: (c["route"].failure_domain in used,
                                           -c["score"], c["eta_s"]))
            route = candidates[0]["route"] if candidates else None
            if route:
                used.add(route.failure_domain)
            roles.append({"id": f"member-{index+1}", "role": role, "route": route.public() if route else None})
        return {"available": bool(leaders), "member_count": job.member_count, "roles": roles,
                "request": req.public(), "group_budget": req.token_budget,
                "per_round_token_cap": None, "approval_required": ["budget_extension", "seat_change"],
                "preferences": self.service.preference_view(lead_task),
                "slicing": {"mode": req.slice_mode, "target_chars": req.slice_target_chars,
                            "min_chars": req.slice_min_chars, "max_chars": req.slice_max_chars,
                            "max_slices": req.max_slices, "max_parallel": req.max_parallel,
                            "estimated_initial_slices": (len(req.context) + req.slice_target_chars - 1) // req.slice_target_chars},
                "warnings": ["initial_slice_estimate_exceeds_record_budget"] if req.slice_mode == "adaptive" and len(req.context) > req.slice_target_chars * req.max_slices else [],
                "note": "Routing is advisory; native model/context/transport limits still apply"}

    def submit(self, request, *, _job_id=None, _cancellation_event=None):
        req = GroupRequest.parse(request)
        if _cancellation_event is not None and _cancellation_event.is_set():
            raise ValueError("workflow_cancelled")
        self.service.require_dispatch(req.workflow_id)
        self.service.ensure_catalog()
        with self.service.capacity, self.lock:
            if _cancellation_event is not None and _cancellation_event.is_set():
                raise ValueError("workflow_cancelled")
            if self.stopping or self.service.stopping:
                raise ValueError("Coordinator is stopping")
            active = [j for j in self.jobs.values() if j.state in {"queued", "running", "cancelling", "needs_codex"}]
            if req.work_key:
                for prior in active:
                    if (prior.task.workflow_id, prior.task.work_key) == (req.workflow_id, req.work_key):
                        return {"group_id": prior.id, "state": prior.state, "deduplicated": True}
            if len(active) >= int(self.service.limits.get("max_queued_groups", 32)):
                raise ValueError("Research group queue is full")
            completed = sorted((j for j in self.jobs.values() if j.state in {"succeeded","failed","cancelled"}),key=lambda j:j.created_at)
            for old in completed[:-int(self.service.limits.get("max_retained_groups",32))]:
                self.jobs.pop(old.id,None)
            job = GroupJob(req)
            if _job_id is not None:
                job.id = _job_id
            if _cancellation_event is not None:
                job.cancel = _cancellation_event
            covered = self.service._covered(job.task)
            self.jobs[job.id] = job
            if covered:
                job.state="needs_codex"
                job.pending_action={"type":"scope_covered_by_codex","detail":{"agents":covered},"caller_approval_required":True}
                self.service._save(job)
                return {"group_id":job.id,"state":job.state,"member_count":job.member_count,"scope_covered":True}
            self.service._save(job)
            self.pool.submit(self._run, job)
        return {"group_id": job.id, "state": job.state, "member_count": job.member_count,
                "token_budget": job.task.token_budget, "per_round_token_cap": None}

    def _seed_tasks(self, job, roles=None):
        tasks = []
        for role in (roles or job.roles)[1:]:
            label = role["role"]
            prompt = {"collector": "Gather primary evidence for the topic, marking missing information.",
                      "analyst": "Analyze mechanisms, tradeoffs and logical consistency using the supplied evidence.",
                      "checker": "Independently look for counterexamples, unsupported claims and contradictory evidence."}[label]
            tasks.append({"role": label, "kind": ("research" if job.request.needs_web else "extract") if label == "collector" else "logic_check",
                          "prompt": prompt + " Topic: " + job.request.topic, "needs_web": job.request.needs_web and label == "collector", "focus_urls": []})
        return tasks

    def _source_evidence(self, job):
        """Bounded references to immutable source reports, never bill them twice."""
        evidence, size, seen = [], 0, set()
        pending = [item["group_id"] for item in job.merged_sources]
        while pending and len(seen) < 32:
            group_id = pending.pop(0)
            if group_id in seen:
                continue
            seen.add(group_id)
            source = self.service.store.job(group_id)
            pending.extend(item["group_id"] for item in source["group"].get("merged_sources", []))
            for result in source.get("results", [])[-8:]:
                content = result.get("content") or {}
                row = {"origin_group": group_id, "worker": result.get("worker"), "round": result.get("round"),
                       "slot": result.get("slot"), "role": result.get("role"), "ok": result.get("ok"),
                       "source": (result.get("route") or {}).get("source", "unknown"),
                       "summary_excerpt": content.get("summary", "")[:500],
                       "uncertainties": [str(v)[:250] for v in content.get("uncertainties", [])[:4]],
                       "evidence_status": "untrusted_report_requires_independent_review"}
                amount = len(json.dumps(row, ensure_ascii=False))
                if size + amount > 12000:
                    return evidence
                evidence.append(row)
                size += amount
        return evidence

    def merge(self, group_ids, approved, reason, expected_revisions, token_budget=None):
        if approved is not True or not isinstance(reason, str) or not reason.strip() or len(reason) > 2000:
            raise ValueError("Explicit caller approval and a bounded reason are required")
        reject_secrets(reason)
        if not isinstance(group_ids, list) or not 2 <= len(group_ids) <= 4 or any(not isinstance(g, str) or not g or len(g) > 200 for g in group_ids) or len(set(group_ids)) != len(group_ids):
            raise ValueError("Merge requires 2..4 unique source group IDs")
        if not isinstance(expected_revisions, dict) or set(expected_revisions) != set(group_ids) or any(not isinstance(r, int) or isinstance(r, bool) or r < 0 for r in expected_revisions.values()):
            raise ValueError("Expected revisions must cover every source group exactly")
        # Stable lock order: capacity -> groups -> sorted job conditions -> DB.
        with self.service.capacity, self.lock, ExitStack() as locks:
            if self.stopping or self.service.stopping:
                raise ValueError("Coordinator is stopping")
            sources = [self._restore(g) for g in sorted(group_ids)]
            for job in sources:
                locks.enter_context(job.changed)
            for job in sources:
                if job.state not in {"idle", "needs_codex", "interrupted"} or job.cancel.is_set() or job.coordinating or job.active_calls or job.tokens_reserved or any(a.get("state") == "running" for a in job.attempts):
                    raise ValueError("Running, reserved, cancelled or active-future groups cannot be merged")
                if job.slice_state and SliceQueue(job.request, job.next_tasks, job.slice_state).incomplete():
                    raise ValueError("Unfinished source slices must be reconciled before sealing a merged source")
                if self._unresolved_requests(job):
                    raise ValueError("Unresolved source information requests must be supported before merge")
                if job.revision != expected_revisions[job.id]:
                    raise ValueError("Stale merge approval: source revision changed")
            workflow = {j.task.workflow_id for j in sources}
            scopes = {j.task.preference_scope for j in sources}
            if len(workflow) != 1 or len(scopes) != 1:
                raise ValueError("Merge sources must share workflow and preference scope")
            seats = sum(j.member_count for j in sources)
            source_budget, used, estimated = (sum(getattr(j.task, "token_budget") for j in sources),
                                       sum(j.tokens_used for j in sources), sum(j.tokens_estimated for j in sources))
            if token_budget is not None:
                if not isinstance(token_budget, int) or isinstance(token_budget, bool) or not 1000 <= token_budget <= 100000000:
                    raise ValueError("Merged token_budget override must be an integer in 1000..100000000")
                if token_budget > source_budget:
                    raise ValueError("Merged token_budget cannot exceed the source budget sum")
                budget = token_budget
            else:
                budget = source_budget
            # Estimated tokens are already a subset of used; never add twice.
            if seats > 32:
                raise ValueError("Merged seats exceed 32; provide a smaller composition")
            if budget > 100000000:
                raise ValueError("Source budget sum exceeds 100000000; an explicit caller-approved token_budget override is required")
            if used > budget or estimated > used:
                raise ValueError("Source accounting exceeds the merged budget")
            deadline_at = min(j.deadline_at for j in sources)
            remaining = (deadline_at - now_ms()) / 1000
            if remaining <= 0:
                raise ValueError("Earliest source hard deadline expired; caller extension required before merge")
            source_refs = [{"group_id": j.id, "revision": j.revision, "member_count": j.member_count,
                            "budget_total": j.task.token_budget, "tokens_used": j.tokens_used,
                            "tokens_estimated": j.tokens_estimated, "deadline_at": j.deadline_at}
                           for j in sources]
            parallel_caps = [j.request.max_parallel for j in sources if j.request.max_parallel is not None]
            slice_min, slice_max = max(j.request.slice_min_chars for j in sources), min(j.request.slice_max_chars for j in sources)
            if slice_min > slice_max:
                raise ValueError("Source slice size constraints are incompatible")
            request = GroupRequest(topic="Joint research: " + " | ".join(j.request.topic[:1000] for j in sources),
                context=json.dumps({"source_groups": source_refs, "caller_merge_reason": reason}, ensure_ascii=False),
                needs_web=any(j.request.needs_web for j in sources), deadline_s=max(5, int(remaining)),
                token_budget=min(50000000, budget), leader_min_quality=max(j.request.leader_min_quality for j in sources),
                worker_min_quality=max(j.request.worker_min_quality for j in sources),
                workflow_id=sources[0].task.workflow_id, preference_scope=sources[0].task.preference_scope,
                work_key="merged:" + uuid.uuid4().hex, price_weight=max(j.request.price_weight for j in sources),
                prefer_free=any(j.request.prefer_free for j in sources), prefer_new_models=any(j.request.prefer_new_models for j in sources),
                max_parallel=min(parallel_caps) if parallel_caps else None,
                output_mode="native" if all(j.request.output_mode == "native" for j in sources) else "adaptive",
                slice_mode="off" if any(j.request.slice_mode == "off" for j in sources) else "adaptive",
                slice_min_chars=slice_min, slice_max_chars=slice_max,
                slice_target_chars=max(slice_min, min(slice_max, min(j.request.slice_target_chars for j in sources))),
                max_slices=min(256, sum(j.request.max_slices for j in sources)),
                exploration_mode="off" if any(j.request.exploration_mode == "off" for j in sources) else "guided",
                exploration_budget_ratio=min(j.request.exploration_budget_ratio for j in sources),
                max_exploration_rounds=min(8, sum(j.request.max_exploration_rounds for j in sources)))
            merged = GroupJob(request)
            merged.member_count, merged.task.token_budget = seats, budget
            merged.tokens_used, merged.tokens_estimated = used, estimated
            merged.exploration_tokens_used = sum(j.exploration_tokens_used for j in sources)
            merged.exploration_rounds = sum(j.exploration_rounds for j in sources)
            merged.merged_sources = source_refs
            merged.deadline_at, merged.deadline = deadline_at, time.monotonic() + remaining
            merged.state, merged.phase = "needs_codex", "merged_ready"
            merged.pending_action = {"type": "merge_ready", "detail": {"source_groups": [j.id for j in sources]}, "caller_approval_required": False}
            merged.notes = ["Caller approved merging: " + reason]
            event = {"event_id": "merge:" + merged.id, "payload": {"type": "group_merge", "group_id": merged.id,
                     "source_groups": source_refs, "member_count": seats, "budget_total": budget,
                     "source_budget_sum": source_budget, "explicit_budget_override": token_budget is not None,
                     "tokens_used": used, "tokens_estimated": estimated, "deadline_at": deadline_at,
                     "approval": "caller_reported", "reason": reason}}
            self.service.store.merge_groups(merged, sources, expected_revisions, event)
            # No in-memory transition occurs until the complete DB commit.
            self.jobs[merged.id] = merged
            for job in sources:
                job.state, job.phase, job.merged_into = "merged", "merged", merged.id
                job.revision += 1
                job.changed.notify_all()
                self.jobs.pop(job.id, None)  # immutable reports remain in DB
        return {**self.status(merged.id), "requires_resume": True, "note": "Seats are cumulative roles; actual concurrency still obeys global/source/provider caps. Source attempts retain their original billing records."}

    def _call(self, job, task, slot, role):
        with job.changed:
            if job.cancel.is_set() or job.state != "running":
                return None
            worker = job.report_worker
            job.report_worker += 1
            job.calls.append({"worker":worker,"slot":slot,"role":role,"round":job.round,"phase":job.phase})
            job.active_calls += 1
            self.service._save(job)
        task.work_key = f"{job.task.work_key}:group:{job.id}:round:{job.round}:{role}:{slot}"
        task._exploration = job.exploration_active and role != "leader"
        try:
            result = self.service._worker(job, task, worker, job.deadline)
        finally:
            with job.changed:
                job.active_calls -= 1
        with job.changed:
            result.update({"role": role, "slot": slot, "round": job.round, "phase": job.phase, "task_kind":task.kind})
            member_slot = getattr(task, "_member_slot", slot)
            result["member_slot"] = member_slot
            result["exploration"] = getattr(task, "_exploration", False)
            result["responds_to"] = getattr(task, "_responds_to", [])
            result["responds_to_requests"] = getattr(task, "_responds_to_requests", [])
            result["support"] = getattr(task, "_support", None)
            if getattr(task, "_slice", None):
                result["slice"] = dict(task._slice)
            job.results.append(result)
            job.revision += 1
            if result.get("ok") and member_slot < len(job.roles):
                job.roles[member_slot]["route"] = result.get("route")
                if not getattr(task, "_slice", None):
                    job.roles[member_slot]["role"] = role
            self.service._save(job)
            if result.get("ok") and role != "leader" and hasattr(self.service, "coordination"):
                self._publish_member_report(job, result)
        return result

    def _advisory_event(self, job, event_id, payload):
        try:
            self.service.store.append_event(job.task.workflow_id, event_id, payload)
        except ValueError as error:
            if "capacity" not in str(error):
                raise
            self.service.store.event_gap(job.task.workflow_id, True)

    def _publish_member_report(self, job, result):
        worker = result["worker"]
        try:
            self.service.coordination.emit_report(job, result, result["content"].get("communications", []))
        except ValueError as error:
            result["communication_warning"] = str(error)[:200]
        try:
            requests = self._information_requests(job, result)
            result["information_requests"] = requests
        except ValueError as error:
            result["information_request_error"] = str(error)[:200]
            requests = []
        for number, request in enumerate(requests):
            self._advisory_event(job, f"information-request:{job.id}:{worker}:{number}",
                {"type": "information_request", "from_group": job.id, "worker": worker,
                 "slice_id": (result.get("slice") or {}).get("id"), "request": request, "authority": "advisory_only"})
        try:
            leads = parse_leader_decision({"action": "submit_codex", "research_leads": result["content"].get("research_leads", [])})["research_leads"]
            if len(leads) > 2:
                raise ValueError("At most two member research leads")
            for number, lead in enumerate(leads):
                refs = self.service.coordination._refs(job.task.workflow_id, lead["evidence_refs"])
                item = {**lead, "evidence_refs": refs, "origin_worker": worker, "slice_id": (result.get("slice") or {}).get("id")}
                job.research_leads.append(item)
                job.research_leads = job.research_leads[-32:]
                self._advisory_event(job, f"member-lead:{job.id}:{worker}:{number}",
                    {"type": "research_lead", "from_group": job.id, "worker": worker, "lead": item,
                     "to_role": "leader", "authority": "advisory_only"})
        except ValueError as error:
            result["research_lead_warning"] = str(error)[:200]
        self.service._save(job)

    def _leader(self, job, initial=False, reuse_result=None):
        if reuse_result is not None:
            return self._leader_decision(job, reuse_result)
        adaptive = job.request.slice_mode == "adaptive"
        supplied = job.request.context
        if adaptive and len(supplied) > job.request.slice_target_chars:
            supplied = supplied[:job.request.slice_target_chars]
        unresolved = self._unresolved_requests(job)
        context = {"topic": job.request.topic, "supplied_context": supplied,
                   "unresolved_information_requests": list(unresolved.values())[:64],
                   "unresolved_information_request_count": len(unresolved),
                   "material": {"source_hash": digest(job.request.context), "source_chars": len(job.request.context),
                                "excerpt_only": len(supplied) != len(job.request.context)},
                   "slices": {k: v for k, v in (job.slice_state or {}).items() if k != "templates"},
                   "slice_limits": {"target_chars": job.request.slice_target_chars, "min_chars": job.request.slice_min_chars,
                                    "max_chars": job.request.slice_max_chars, "max_records": job.request.max_slices},
                   "information_requests": [{"worker": r["worker"], "slot": r.get("slot"), "slice": self._evidence_slice(r),
                                             "requests": r.get("information_requests", []),
                                             "support_workers": [f["worker"] for f in job.results if f.get("ok") and r["worker"] in f.get("responds_to", [])]}
                                            for r in job.results[-job.request.max_slices:] if r.get("information_requests")],
                   "exploration": {"mode": job.request.exploration_mode, "rounds_used": job.exploration_rounds,
                                   "max_rounds": job.request.max_exploration_rounds,
                                   "budget_fraction": job.request.exploration_budget_ratio},
                   "research_leads": job.research_leads,
                   "round": job.round, "member_count": job.member_count,
                   "roles": job.roles,
                   "missing_member_slots":self._missing_slots(job),
                   "feedback_targets": [{"slot": r.get("slot"), "report_worker": r.get("worker")}
                       for r in job.results if r.get("round") == job.round and r.get("role") != "leader" and r.get("ok")],
                   "budget_remaining": max(0, job.task.token_budget - job.tokens_used - job.tokens_reserved),
                   "notes_from_codex": job.notes,
                   "messages": self.service.coordination.context_for(job, "leader", 0) if hasattr(self.service, "coordination") else [],
                   "peer_groups": self.service.coordination.peer_groups(job) if hasattr(self.service, "coordination") else [],
                   "merged_source_evidence": self._source_evidence(job),
                   "reports": [{"slot": r.get("slot"), "worker": r.get("worker"), "role": r.get("role"),
                                "slice": self._evidence_slice(r), "ok": r.get("ok"),
                                "evidence": self._evidence_excerpt(r), "error": r.get("error")}
                               for r in job.results if r.get("round",0) >= max(1,job.round-2) and r.get("role") != "leader"]}
        prompt = (f"You lead a research group of {job.member_count} roles. " + ("Plan initial division of work." if initial else "Review the independent evidence and decide the next step.") +
                  " Keep facts, inference, conflict, uncertain claims and common-source dependence distinct. "
                  "Do not assume agreement proves correctness. Select replacement roles for failed or low-quality work. "
                  "Return the normal report JSON with an additional group_decision object: "
                  "{action:continue_research|submit_codex|request_extension|request_seat_change|blocked|propose_merge, rationale:string, summary:string, "
                  "next_tasks:[{role:collector|analyst|checker,kind:research|extract|summarize|code_review|logic_check,prompt:string,needs_web:boolean,focus_urls:[]}],uncertainties:[]}. "
                  "Only continue_research may contain next_tasks, replace_roles (existing member IDs), and feedback [{worker:slot,accepted:boolean,reason:string}]. "
                  "In feedback, worker MUST be the integer slot from feedback_targets (example worker:1); it is not a report_worker ID or a member role string. "
                  f"Only request_seat_change may include member_count (3..{32 if job.merged_sources else 8}). Budget/seat/merge proposals require Codex approval. "
                  "propose_merge requires merge_groups (2..4 authorized group IDs, including this group), and never grants approval. "
                  "communications may contain at most four advisory objects {to_group?,to_role,kind:assignment|evidence|conflict|progress|suggestion|merge_proposal,summary,evidence_refs:[{group_id,worker?,message_id?,url?,anchor?}]}. "
                  "Use only peer groups listed in context; cite original group/report/message IDs. This JSON is a proposal for GTT, not permission to contact agents or invoke tools. "
                  "Use submit_codex when ready for Codex's final review; request_extension only when more total budget is needed. "
                  f"Never invent unavailable evidence, execute actions, or delegate outside the listed roles. At most {job.request.max_slices if adaptive else job.member_count-1} next tasks. "
                  "Work items are a queue, not additional seats. Optional context_range:[start,end] identifies exact character offsets of supplied material. "
                  "Optional evidence_workers:[report IDs] accepts at most 32 unique completed reports, with a shared 14000-character excerpt budget, not full reports. "
                  "Use at most eight information_request_ids:[exact IDs from unresolved_information_requests] to bind support to individual questions. "
                  "Resolve member information_requests with targeted context_range tasks, evidence_workers, or bounded source collection before accepting unsupported conclusions. "
                  "Include the requesting member's report ID in evidence_workers for support tasks so the response is linked to its request. "
                  "When material is excerpted, plan partitioned work without assuming the excerpt is the whole source. Review cross-slice dependencies explicitly. "
                  "Proactively inspect blind spots, counterexamples and adjacent improvement opportunities. Do not invent leads just to remain active. "
                  "Report research_leads:[{question,rationale,priority:low|normal|high,expected_value,evidence_refs:[]}], at most four. "
                  "If guided exploration has a concrete valuable lead within the topic and remaining limits, use continue_research with exploration:true and distinct targeted next_tasks. "
                  "Out-of-scope leads remain advisory for Codex; never expand the topic or total budget. Evidence excerpts are incomplete: request focused follow-up or explicitly mark uncertainty.")
        task = self._task(job.request, kind="logic_check", prompt=prompt,
                          context=json.dumps(context, ensure_ascii=False), quality=job.request.leader_min_quality,
                          preferred=job.leader_route, role="leader")
        result = self._call(job, task, 0, "leader")
        return self._leader_decision(job, result)

    def _leader_decision(self, job, result):
        if result and result.get("group_decision_applied"):
            return result["group_decision_applied"]
        adaptive = job.request.slice_mode == "adaptive"
        if not result or not result.get("ok"):
            reason = "group_budget_exhausted" if (result or {}).get("error", {}).get("code") == "token_budget_exhausted" else "leader_unavailable"
            self._handoff(job, reason, result.get("error") if result else None)
            return None
        job.leader_route = result["route"]["route_id"]
        try:
            decision = parse_leader_decision(result["content"].get("group_decision"), 32 if job.merged_sources else 8,
                                             job.request.max_slices if adaptive else None)
            for task in decision.get("next_tasks", []):
                if "context_range" in task and task["context_range"][1] > len(job.request.context):
                    raise ValueError("Next task range is outside this group's material")
            if hasattr(self.service, "coordination"):
                self.service.coordination.emit_report(job, result, decision.get("communications", []))
                if decision["action"] == "propose_merge":
                    if job.id not in decision["merge_groups"]:
                        raise ValueError("Merge proposal must include the proposing group")
                    for group_id in decision["merge_groups"]:
                        self.service.coordination._group(job.task.workflow_id, group_id, True)
                    self.service.store.append_event(job.task.workflow_id, "merge-proposal:" + job.id + ":" + str(result["worker"]),
                        {"type": "merge_proposal", "from_group": job.id, "from_role": "leader", "to_role": "supervisor",
                         "merge_groups": decision["merge_groups"], "summary": decision["rationale"][:1200],
                         "authority": "advisory_only", "worker": result["worker"]})
        except ValueError as error:
            self._handoff(job, "invalid_leader_decision", {"message": str(error)[:300]})
            return None
        with job.changed:
            job.decisions.append(decision)
            job.research_leads.extend(decision.get("research_leads", []))
            job.research_leads = job.research_leads[-32:]
            for number, lead in enumerate(decision.get("research_leads", [])):
                refs = self.service.coordination._refs(job.task.workflow_id, lead.get("evidence_refs", []))
                self._advisory_event(job, f"research-lead:{job.id}:{result['worker']}:{number}",
                    {"type": "research_lead", "from_group": job.id, "worker": result["worker"],
                     "lead": {**lead, "evidence_refs": refs}, "to_role": "caller", "authority": "advisory_only"})
            job.revision += 1
            result["group_decision_applied"] = decision
            self.service._save(job)
        return decision

    def _handoff(self, job, reason, detail=None):
        with job.changed:
            if job.cancel.is_set():
                job.state = "cancelled"
            else:
                job.state = "needs_codex"
                job.pending_action = {"type": reason, "detail": detail, "caller_approval_required": True}
            job.revision += 1
            self.service._save(job)

    def _apply_feedback(self, job, decision):
        current = {r["slot"]: r for r in job.results if r.get("round") == job.round and r.get("role") != "leader"}
        by_member = {r.get("member_slot", r["slot"]): r for r in job.results if r.get("round") == job.round and r.get("role") != "leader"}
        ids = {r["id"]: r for r in job.roles[1:]}
        if any(role_id not in ids for role_id in decision.get("replace_roles", [])):
            raise ValueError("Replacement must reference an existing group member")
        if any(not current.get(v["worker"], {}).get("ok") for v in decision.get("feedback", [])):
            raise ValueError("Leader feedback must reference a completed report in this round")
        for vote in decision.get("feedback", []):
            result = current.get(vote["worker"])
            if not result or not result.get("ok"):
                raise ValueError("Leader feedback must reference a completed report in this round")
            route = result["route"]
            self.service.store.record_feedback(route["route_id"], result.get("task_kind","research"),
                                               vote["accepted"], job.id, result["worker"])
            if not vote["accepted"]:
                job.blocked_routes.add(route["route_id"])
        for role_id in decision.get("replace_roles", []):
            if role_id not in ids:
                raise ValueError("Replacement must reference an existing group member")
            route = ids[role_id].get("route")
            slot = job.roles.index(ids[role_id])
            last = by_member.get(slot)
            if route and last and last.get("ok"):
                job.blocked_routes.add(route["route_id"])
            elif route:
                job.avoid_until[route["route_id"]]=now_ms()+1800000
                if last:
                    for attempt in job.attempts:
                        if attempt.get("worker")==last.get("worker"):
                            job.avoid_until[attempt["route_id"]]=now_ms()+1800000
            ids[role_id]["route"] = None

    def _missing_slots(self, job):
        if job.round==0:return []
        if job.slice_state:
            return [r["slot"] for r in job.slice_state["records"] if r["state"] not in {"succeeded", "split"}]
        completed={r["slot"] for r in job.results if r.get("round")==job.round and r.get("role")!="leader" and r.get("ok")}
        return sorted(set(job.expected_slots)-completed)

    @staticmethod
    def _evidence_excerpt(result, max_chars=3500):
        content = result.get("content", {})
        encoded = json.dumps(content, ensure_ascii=False)
        if len(encoded) <= max_chars:
            return content
        summary_size = min(1000, max_chars // 3)
        evidence = json.dumps({key: value for key, value in content.items() if key != "summary"}, ensure_ascii=False)
        return {"summary": content.get("summary", "")[:summary_size], "excerpt": evidence[:max(0, max_chars - summary_size - 100)],
                "excerpt_only": True, "full_report_worker": result.get("worker")}

    @staticmethod
    def _evidence_slice(result):
        value = result.get("slice")
        return {key: item for key, item in value.items() if key != "state"} if isinstance(value, dict) else value

    @staticmethod
    def _information_requests(job, result):
        items = result.get("content", {}).get("information_requests", [])
        if not isinstance(items, list) or len(items) > 2:
            raise ValueError("At most two information requests per member report")
        clean = []
        workers = {r["worker"] for r in job.results}
        for index, item in enumerate(items):
            if not isinstance(item, dict) or set(item) - {"question", "reason", "context_range", "report_workers"}:
                raise ValueError("Invalid information request fields")
            row = {"request_id": f"{result.get('worker', 0)}:{index}"}
            for key in ("question", "reason"):
                value = item.get(key)
                if not isinstance(value, str) or not value.strip() or len(value) > 1000:
                    raise ValueError("Information request text must be bounded")
                reject_secrets(value)
                row[key] = value
            if "context_range" in item:
                bounds = item["context_range"]
                if not isinstance(bounds, list) or len(bounds) != 2 or any(not isinstance(v, int) or isinstance(v, bool) for v in bounds) or not 0 <= bounds[0] <= bounds[1] <= len(job.request.context):
                    raise ValueError("Information request range is outside source material")
                row["context_range"] = bounds
            refs = item.get("report_workers", [])
            if not isinstance(refs, list) or len(refs) > 4 or any(not isinstance(v, int) or isinstance(v, bool) or v not in workers for v in refs):
                raise ValueError("Information request references unavailable reports")
            row["report_workers"] = refs
            clean.append(row)
        return clean

    @staticmethod
    def _request_rows(job):
        rows = {}
        for report in job.results:
            worker = report["worker"]
            for index, request in enumerate(report.get("information_requests", [])):
                request_id = request.get("request_id", f"{worker}:{index}")
                rows[request_id] = {**request, "request_id": request_id, "worker": worker}
            if report.get("information_request_error"):
                request_id = f"{worker}:invalid"
                rows[request_id] = {"request_id": request_id, "worker": worker,
                    "question": "Clarify the invalid information request", "reason": report["information_request_error"]}
        return rows

    def _unresolved_requests(self, job):
        requests = self._request_rows(job)
        resolved, coverage = set(), {}
        for report in job.results:
            if not report.get("ok") or report.get("information_request_error"):
                continue
            ids = report.get("responds_to_requests", [])
            if not ids:
                # Compatibility for old single-request handoffs; a worker ID
                # cannot close several distinct questions.
                for worker in report.get("responds_to", []):
                    matches = [key for key, row in requests.items() if row["worker"] == worker]
                    if len(matches) == 1:
                        resolved.add(matches[0])
                continue
            support = report.get("support")
            if not support:
                resolved.update(ids)
                continue
            for request_id in ids:
                key = (request_id, support["id"], support["start"], support["end"])
                record = report.get("slice") or {"start": support["start"], "end": support["end"]}
                coverage.setdefault(key, []).append((record["start"], record["end"]))
        for (request_id, _, start, end), ranges in coverage.items():
            cursor = start
            for first, last in sorted(ranges):
                if first > cursor:
                    break
                cursor = max(cursor, last)
            if cursor >= end:
                resolved.add(request_id)
        return {key: row for key, row in requests.items() if key not in resolved}

    def _member_task(self, job, item, slot, record=None):
        member_slot = 1 + (slot - 1) % (job.member_count - 1)
        role = item["role"]
        quality = job.request.worker_min_quality if role == "collector" and item["kind"] not in {"code_review", "logic_check"} else job.request.leader_min_quality
        previous = job.roles[member_slot].get("route")
        # Batch collectors can choose a free/fast available provider rather than
        # being permanently pinned to the provider originally assigned a seat.
        preferred = previous["route_id"] if previous and previous["route_id"] not in job.blocked_routes else None
        text = ("" if item.get("evidence_workers") or item.get("information_request_ids") else job.request.context) if record is None else job.request.context[record["start"]:record["end"]]
        if record is None and "context_range" in item:
            start, end = item["context_range"]
            if not 0 <= start <= end <= len(job.request.context):
                raise ValueError("Task context_range is outside supplied material")
            text = job.request.context[start:end]
        context = {"original_topic": job.request.topic, "supplied_context": text,
                   "prior_leader_summary": job.decisions[-1].get("summary", "") if job.decisions else "",
                   "messages": self.service.coordination.context_for(job, role, member_slot),
                   "prior_evidence": [{"worker": r.get("worker"), "slot": r.get("slot"),
                                       "slice": self._evidence_slice(r), "evidence": self._evidence_excerpt(r)}
                                      for r in job.results[-6:] if r.get("role") != "leader"]}
        requested = list(item.get("evidence_workers", []))
        reports = {r["worker"]: r for r in job.results if r.get("ok")}
        requests = self._request_rows(job)
        request_ids = item.get("information_request_ids", [])
        if any(key not in requests for key in request_ids):
            raise ValueError("Support task references an unavailable information request")
        for key in request_ids:
            if requests[key]["worker"] not in requested:
                requested.append(requests[key]["worker"])
        if len(requested) > 32:
            raise ValueError("Support task exceeds 32 report excerpts")
        if any(worker not in reports for worker in requested):
            raise ValueError("Follow-up evidence must reference completed reports")
        if not request_ids:
            request_ids = []
            for worker in requested:
                matches = [key for key, row in requests.items() if row["worker"] == worker]
                if len(matches) == 1:
                    request_ids.extend(matches)
                elif "context_range" in item:
                    request_ids.extend(key for key in matches if requests[key].get("context_range") == item["context_range"])
        context["support_requests"] = [requests[key] for key in request_ids]
        if request_ids or requested:
            context["prior_evidence"] = []
        excerpt_size = min(3500, 14000 // max(1, len(requested)))
        context["requested_evidence"] = [{"worker": worker, "slice": self._evidence_slice(reports[worker]),
                                          "evidence": self._evidence_excerpt(reports[worker], excerpt_size)} for worker in requested]
        if record is None and "context_range" in item:
            context["material_range"] = {"start": start, "end": end, "source_hash": digest(job.request.context),
                                         "content_hash": digest(text)}
        if record is not None:
            context["slice"] = {**record, "source_hash": digest(job.request.context),
                                "base_revision": job.request.base_revision, "context_label": job.request.context_label,
                                "offset_unit": "python_characters", "supplied_context_complete_for_range": True}
        prompt = item["prompt"]
        if record is not None:
            prompt += " Work only on this assigned range. Preserve source offsets. Report cross-range dependencies and missing definitions instead of assuming this fragment is standalone."
            prompt += " supplied_context is the full assigned source range; requested_evidence contains shortened reports. Do not confuse the two or count cases outside the half-open slice range. Offsets are Python characters from slice.start, not bytes."
        if request_ids:
            prompt += " This is targeted evidence support: answer only support_requests using the supplied range and requested reports. The original_topic is background, not a requirement to audit the entire group again. Do not request the same supplied range or an unrelated whole-source inventory; state precise remaining uncertainty instead."
        if requested:
            prompt += " Requested reports are bounded excerpts; excerpt_only means incomplete evidence, not a full report. Use full_report_worker and precise source anchors to request any missing part."
        task = self._task(job.request, kind=item["kind"], prompt=prompt, context=json.dumps(context, ensure_ascii=False),
                          web=item.get("needs_web", False), quality=quality, preferred=preferred, role=role)
        task.focus_urls = item.get("focus_urls", [])
        if role == "collector" and job.request.prefer_free:
            free = [c for c in self._candidates(task, job) if c["reasons"]["price"]["free"]]
            if free:
                task.preferred_route = free[0]["route"].id
        task._member_slot = member_slot
        task._responds_to = [worker for worker in requested if reports[worker].get("information_requests")]
        task._responds_to_requests = list(request_ids)
        if request_ids:
            first, last = item.get("context_range", [0, 0 if requested else len(job.request.context)])
            task._support = {"id": (record or {}).get("support_id", digest(json.dumps(item, sort_keys=True))),
                             "start": (record or {}).get("support_start", first),
                             "end": (record or {}).get("support_end", last)}
        if record is not None:
            task._slice = dict(context["slice"])
            task._output_floor = record.get("minimum_output_tokens", 256)
            task._output_target = record.get("target_output_tokens", 0)
            initial_window = 600 if task.kind in {"code_review", "logic_check", "cross_check"} else 180
            task.ttft_s = min(3600, max(initial_window, initial_window * (len(task.context) / 8000) ** .5))
        return task

    def _run_slice_queue(self, job, tasks):
        queue = SliceQueue(job.request, tasks, job.slice_state)
        job.slice_state = queue.state
        ceiling = min(job.request.max_parallel or self.service.preference_view(job.task)["recommended_parallel"],
                      self.service.preference_view(job.task)["effective_max_parallel"],
                      max(1, int(self.service.limits.get("global_parallel", 4)) - int(self.service.limits.get("group_reserved_foreground_slots", 1))))
        with ThreadPoolExecutor(max_workers=ceiling, thread_name_prefix="gtt-slice") as workers:
            pending = {}
            while True:
                while len(pending) < ceiling and not job.cancel.is_set() and time.monotonic() < job.deadline:
                    if not queue.has_work():
                        break
                    capacity_chars = None
                    if queue.state["partition_context"] and not any(r["state"] == "pending" for r in queue.state["records"]):
                        template = queue.state["templates"][len(queue.state["records"]) % queue.state["template_count"]]
                        quality = job.request.worker_min_quality if template["role"] == "collector" and template["kind"] not in {"code_review", "logic_check"} else job.request.leader_min_quality
                        probe = self._task(job.request, kind=template["kind"], prompt=template["prompt"],
                                           quality=quality, web=template.get("needs_web", False), role=template["role"])
                        candidates = self._candidates(probe, job)
                        window = candidates[0]["route"].context_window if candidates else None
                        capacity_chars = max(job.request.slice_min_chars, int(window * 1.5) - 8000) if window else None
                    with job.changed:
                        record = queue.take(capacity_chars)
                        if record is None:
                            break
                        item = queue.state["templates"][record["template"]]
                        record["state"] = "running"
                        job.expected_slots = [r["slot"] for r in queue.state["records"] if r["state"] != "split"]
                        self.service._save(job)
                    try:
                        task = self._member_task(job, item, record["slot"], record)
                    except ValueError as error:
                        with job.changed:
                            result = {"worker": job.report_worker, "slot": record["slot"], "role": item["role"],
                                "round": job.round, "phase": job.phase, "ok": False,
                                "error": {"code": "invalid_member_task", "message": str(error)[:200]}, "slice": dict(record)}
                            job.report_worker += 1
                            job.results.append(result)
                            queue.finish(record, result)
                            job.revision += 1
                            self.service._save(job)
                        continue
                    pending[workers.submit(self._call, job, task, record["slot"], item["role"])] = record
                if not pending:
                    break
                done, _ = wait(pending, timeout=.2, return_when=FIRST_COMPLETED)
                for future in done:
                    record = pending.pop(future)
                    result = future.result()
                    with job.changed:
                        queue.finish(record, result)
                        queue.support(record, result)
                        job.expected_slots = [r["slot"] for r in queue.state["records"] if r["state"] != "split"]
                        self.service._save(job)
        unassigned = queue.state["cursor"] < len(queue.text) if queue.state["partition_context"] else queue.state["template_cursor"] < queue.state["template_count"]
        if job.cancel.is_set():
            self._handoff(job, "cancelled")
            return False
        if unassigned or any(r["state"] in {"pending", "running", "unknown"} for r in queue.state["records"]):
            self._handoff(job, "incomplete_slice_evidence", queue.view())
            return False
        return True

    def _allow_exploration(self, job, decision):
        if not decision.get("exploration"):
            job.exploration_active = False
            return True
        if (job.request.exploration_mode == "off" or job.exploration_rounds >= job.request.max_exploration_rounds
                or not decision.get("research_leads") or not decision.get("next_tasks")):
            self._handoff(job, "exploration_limit", decision)
            return False
        if job.exploration_tokens_start is None:
            job.exploration_tokens_start = job.tokens_used
        job.exploration_rounds += 1
        job.exploration_active = True
        return True

    def _run(self, job):
        try:
            self.service.ensure_catalog()
            with job.changed:
                if job.cancel.is_set():
                    job.state = "cancelled"
                    return
                job.state = "running"
                job.coordinating = True
            covered = self.service._covered(job.task)
            if covered:
                self._handoff(job, "scope_covered_by_codex", {"agents": covered})
                return
            if not job.roles or job.round == 0 and job.phase in {"leader_plan", "leader_plan_retry"}:
                if not job.roles:
                    planned = self.plan({k:v for k,v in asdict(job.request).items() if k != "max_output_tokens"}, _member_count=job.member_count)
                    if not planned["available"]:
                        self._handoff(job, "no_qualified_leader")
                        return
                    job.roles = planned["roles"]
                    job.leader_route = job.roles[0]["route"]["route_id"]
                cached = next((r for r in reversed(job.results) if r.get("role") == "leader" and r.get("round") == 0 and r.get("phase") == "leader_plan" and r.get("ok")), None) if job.phase != "leader_plan_retry" else None
                job.phase = "leader_plan"
                initial = self._leader(job, True, cached)
                if initial is None:
                    return
                if initial["action"] in {"request_extension", "request_seat_change", "blocked", "submit_codex", "propose_merge"}:
                    self._handoff(job, initial["action"], initial)
                    return
                job.next_tasks = initial.get("next_tasks") or self._seed_tasks(job)
                if not self._allow_exploration(job, initial):
                    return
                job.phase = "initial_ready"
                self.service._save(job)
            resume_slices = job.slice_state is not None and job.phase == "research_round"
            cached_review = next((r for r in reversed(job.results) if r.get("role") == "leader" and r.get("round") == job.round and r.get("phase") == "leader_review" and r.get("ok")), None) if job.phase == "leader_review_cached" else None
            review_only = job.round > 0 and job.phase in {"research_round", "leader_review", "leader_review_cached"} and not resume_slices
            while True:
                with job.changed:
                    if job.cancel.is_set():
                        job.state = "cancelled"
                        return
                    if time.monotonic() >= job.deadline:
                        self._handoff(job, "group_deadline")
                        return
                    if job.round >= job.request.max_rounds and not review_only and not resume_slices:
                        self._handoff(job, "round_limit")
                        return
                    if job.tokens_used >= job.task.token_budget:
                        self._handoff(job, "group_budget_exhausted")
                        return
                    tasks = list(job.next_tasks)
                    seed = self._seed_tasks(job)
                    while len(tasks) < (2 if job.round==0 else 1):
                        tasks.append(seed[len(tasks) % len(seed)])
                    if len(tasks) > job.member_count - 1 and job.request.slice_mode == "off":
                        self._handoff(job, "request_seat_change", {"member_count": min(32 if job.merged_sources else 8, len(tasks)+1), "next_tasks": tasks})
                        return
                    if not review_only and not resume_slices:
                        job.round += 1
                        job.phase = "research_round"
                        job.expected_slots=list(range(1,len(tasks)+1))
                        job.slice_state = None
                    self.service._save(job)
                if not review_only:
                    if job.request.slice_mode == "adaptive":
                        if not self._run_slice_queue(job, tasks):
                            return
                    else:
                        batch_size = max(1, min(len(tasks), job.request.max_parallel or job.member_count - 1,
                            int(self.service.limits.get("global_parallel", 4)) - int(self.service.limits.get("group_reserved_foreground_slots", 1))))
                        with ThreadPoolExecutor(max_workers=batch_size, thread_name_prefix="gtt-member") as workers:
                            futures = [workers.submit(self._call, job, self._member_task(job, item, slot), slot, item["role"])
                                       for slot, item in enumerate(tasks, 1)]
                            for future in as_completed(futures):
                                future.result()
                resume_slices = False
                if job.cancel.is_set():
                    job.state = "cancelled"
                    return
                job.phase = "leader_review"
                review_only = False
                decision = self._leader(job, reuse_result=cached_review)
                cached_review = None
                if decision is None:
                    return
                self._apply_feedback(job, decision)
                unresolved = self._unresolved_requests(job)
                if decision["action"] == "submit_codex" and unresolved:
                    self._handoff(job, "unresolved_information_requests", {"workers": sorted({r["worker"] for r in unresolved.values()}),
                                  "request_ids": list(unresolved), "leader_decision": decision})
                    return
                if decision["action"]=="submit_codex" and self._missing_slots(job):
                    self._handoff(job,"incomplete_member_evidence",{"missing_slots":self._missing_slots(job),"leader_decision":decision})
                    return
                if decision["action"] != "continue_research":
                    self._handoff(job, decision["action"], decision)
                    return
                if not self._allow_exploration(job, decision):
                    return
                job.next_tasks = decision["next_tasks"] or self._seed_tasks(job)
        except Exception as error:
            self._handoff(job, "group_execution_error", {"type": type(error).__name__})
        finally:
            with job.changed:
                job.coordinating = False
            self.service._save(job)

    def status(self, group_id):
        data = self.service.store.job(group_id)
        if "group" not in data:
            raise ValueError("Not a research group")
        meta = data["group"]
        def decision_view(decision):
            if not isinstance(decision,dict):return decision
            view={k:v for k,v in decision.items() if k not in {"next_tasks","replace_roles","feedback","uncertainties","leader_decision"}}
            for key,size in (("summary",1200),("rationale",500)):
                if isinstance(view.get(key),str):view[key]=view[key][:size]
            view["uncertainty_count"]=len(decision.get("uncertainties",[]))
            view["next_tasks"]=[{k:v for k,v in t.items() if k!="prompt"} | {"prompt_excerpt":t["prompt"][:200]} for t in decision.get("next_tasks",[])]
            if "leader_decision" in decision:view["leader_decision"]=decision_view(decision["leader_decision"])
            return view
        public = {k:meta[k] for k in ("member_count","round","phase","revision","leader_route","roles","blocked_routes","budget_total","deadline_at")}
        public.update({k: meta.get(k, [] if k == "merged_sources" else None) for k in ("merged_sources", "merged_into")})
        public["accounting"] = {"includes_source_usage": bool(meta.get("merged_sources")),
                                "estimated_is_subset_of_used": True, "source_billing_unchanged": True}
        public["quiescent"] = data["state"] in {"idle", "needs_codex", "interrupted"} and not meta.get("coordinating", False) and not meta.get("active_calls", 0) and not data.get("tokens_reserved", 0)
        public["progress"] = self.service.coordination.public_progress(data.get("progress", {}), attempts=data.get("attempts", [])) if hasattr(self.service, "coordination") else {}
        snapshot = SimpleNamespace(results=data.get("results", []))
        requests = self._request_rows(snapshot)
        unresolved = self._unresolved_requests(snapshot)
        public["information_status"] = {"total": len(requests), "unresolved_count": len(unresolved),
            "resolved_request_ids": sorted(set(requests) - set(unresolved))[:64], "unresolved_request_ids": sorted(unresolved)[:64]}
        public["latest_decision"] = decision_view(meta["decisions"][-1]) if meta["decisions"] else None
        public["decision_count"] = len(meta["decisions"])
        slices = meta.get("slice_state")
        if slices:
            public["slicing"] = {"target_chars": slices["target_chars"], "source_hash": slices["source_hash"],
                "assigned_chars": slices["cursor"], "record_count": len(slices["records"]),
                "counts": {state: sum(r["state"] == state for r in slices["records"])
                           for state in ("pending", "running", "succeeded", "split", "failed", "unknown")}}
        public["exploration_rounds"] = meta.get("exploration_rounds", 0)
        public["exploration_accounting"] = {"tokens_used": meta.get("exploration_tokens_used", 0),
            "tokens_reserved": meta.get("exploration_tokens_reserved", 0), "leader_review_uses_main_budget": True}
        public["research_leads"] = meta.get("research_leads", [])
        public["roles"]=[{**r,"route":{k:v for k,v in (r.get("route") or {}).items() if k in {"route_id","source","provider","model","effort","output_limit"}} or None} for r in meta["roles"]]
        pending = meta.get("pending_action")
        public["pending_action"] = dict(pending) | {"detail":decision_view(pending.get("detail"))} if pending else None
        return {"group_id": group_id, "state": data["state"], **public,
                "tokens_used": data.get("tokens_used", 0), "tokens_estimated": data.get("tokens_estimated", 0),
                "tokens_reserved": data.get("tokens_reserved", 0), "completed_reports": len(data.get("results", []))}

    def collect(self, group_id, cursor=0, max_chars=12000, wait_ms=0):
        self.status(group_id)
        result = self.service.collect(group_id, wait_ms=wait_ms, cursor=cursor, max_chars=max_chars)
        result["group"] = self.status(group_id)
        result["note"] = "Leader conclusions require Codex's independent final review"
        return result

    def cancel(self, group_id):
        job = self.jobs.get(group_id)
        if job is None:
            return {"group_id": group_id, "state": self.status(group_id)["state"], "cancelled": False}
        with job.changed:
            if job.state in {"succeeded", "cancelled", "failed", "merged"}:
                return {"group_id": group_id, "state": job.state, "cancelled": False}
            job.cancel.set()
            job.state = "cancelled"
            self.service._save(job)
        with self.service.capacity:
            self.service.capacity.notify_all()
        return {"group_id": group_id, "state": "cancelled", "cancelled": True}

    def _restore(self, group_id):
        if group_id in self.jobs:
            if self.jobs[group_id].state == "merged" or self.jobs[group_id].merged_into:
                raise ValueError("Merged source groups are permanently read-only")
            return self.jobs[group_id]
        data = self.service.store.job(group_id)
        if data["state"] == "merged" or data.get("group", {}).get("merged_into"):
            raise ValueError("Merged source groups are permanently read-only")
        if "research_request" not in data or data["state"] not in {"interrupted", "needs_codex", "idle"}:
            raise ValueError("Group is not restorable")
        request_data = dict(data["research_request"])
        request_data.pop("max_output_tokens", None)
        req = GroupRequest.parse(request_data)
        job = GroupJob(req)
        job.id, job.created_at = group_id, data["created_at"]
        job.results, job.attempts, job.progress = data.get("results", []), data.get("attempts", []), data.get("progress", {})
        job.tokens_used, job.tokens_estimated = data.get("tokens_used", 0), data.get("tokens_estimated", 0)
        job.attempt_count = data.get("attempt_count", len(job.attempts))
        meta = data["group"]
        for name in ("member_count", "round", "phase", "revision", "leader_route", "roles", "decisions", "next_tasks", "pending_action", "report_worker", "notes"):
            setattr(job, name, meta[name])
        job.blocked_routes = set(meta.get("blocked_routes", []))
        job.task.token_budget = meta["budget_total"]
        job.deadline_at = meta["deadline_at"]
        job.calls = meta.get("calls",[])
        job.avoid_until = meta.get("avoid_until",{})
        job.expected_slots = meta.get("expected_slots",list(range(1,max(2,len(job.next_tasks))+1)))
        job.message_cursors = meta.get("message_cursors", {})
        job.slice_state = meta.get("slice_state")
        job.exploration_rounds = meta.get("exploration_rounds", 0)
        job.exploration_tokens_start = meta.get("exploration_tokens_start")
        job.exploration_active = meta.get("exploration_active", job.exploration_tokens_start is not None)
        job.exploration_tokens_used = meta.get("exploration_tokens_used", max(0, job.tokens_used - job.exploration_tokens_start) if job.exploration_tokens_start is not None else 0)
        job.research_leads = meta.get("research_leads", [])
        if job.slice_state:
            for record in job.slice_state["records"]:
                if record["state"] == "running":
                    matching = [r for r in job.results if r.get("round") == job.round and r.get("slot") == record["slot"]]
                    if matching and matching[-1].get("ok"):
                        record.update({"state": "succeeded", "worker": matching[-1]["worker"]})
                    else:
                        workers = {c["worker"] for c in job.calls if c.get("round") == job.round and c.get("slot") == record["slot"]}
                        started = any(a.get("worker") in workers for a in job.attempts)
                        record.update({"state": "unknown" if started else "pending", "error": "interrupted_result_unavailable"})
        job.merged_sources = meta.get("merged_sources", [])
        job.merged_into = meta.get("merged_into")
        job.deadline = time.monotonic() + max(0, (job.deadline_at - now_ms()) / 1000)
        for attempt in job.attempts:
            if (attempt.get("state") == "failed" and attempt.get("error", {}).get("code") == "timeout"
                    and str(attempt.get("native_dedup_key", "")).startswith("opencode:msg:")
                    and attempt.get("token_measurement") == "reported" and attempt.get("tokens_charged") == 0
                    and reported_tokens(attempt.get("usage")) == 0):
                estimate = attempt.get("decision", {}).get("reserved_token_estimate", 0)
                if isinstance(estimate, int) and not isinstance(estimate, bool) and estimate > 0:
                    attempt.update({"usage_placeholder": clean_usage(attempt.get("usage")), "usage": {},
                        "tokens_charged": estimate, "token_measurement": "estimated",
                        "metering_recovery": "opencode_pending_zero_placeholder"})
                    job.tokens_used += estimate
                    job.tokens_estimated += estimate
                    if attempt.get("exploration"):
                        job.exploration_tokens_used += estimate
            if attempt.get("state") == "running":
                if "tokens_charged" not in attempt:
                    estimate = attempt.get("decision", {}).get("reserved_token_estimate", 0)
                    attempt.update({"tokens_charged": estimate, "token_measurement": "estimated"})
                    job.tokens_used += estimate
                    job.tokens_estimated += estimate
                    if attempt.get("exploration"):
                        job.exploration_tokens_used += estimate
                attempt.update({"state":"failed","error":{"code":"interrupted_result_unavailable"}})
        returned={r["worker"] for r in job.results}
        for call in job.calls:
            if call["worker"] not in returned:
                job.results.append({**call,"ok":False,"error":{"code":"interrupted_member_result_unknown"}})
        job.state = data["state"]
        if job.tokens_used >= job.task.token_budget:
            job.state = "needs_codex"
            job.pending_action = {"type": "group_budget_exhausted",
                "detail": {"reason": "Restored usage, including conservative unknown outcome estimates, reaches the group budget"},
                "caller_approval_required": True}
        self.jobs[group_id] = job
        self.service._save(job)
        return job

    def _approved_job(self, group_id, approved, reason):
        if approved is not True or not isinstance(reason, str) or not reason.strip():
            raise ValueError("Explicit Codex approval and reason required")
        reject_secrets(reason)
        job = self._restore(group_id)
        if job.state not in {"needs_codex", "interrupted"} or job.cancel.is_set():
            raise ValueError("Group is not waiting for Codex")
        if job.coordinating or job.active_calls or job.tokens_reserved:
            raise ValueError("Group must be quiescent before continuation")
        self.service.require_dispatch(job.task.workflow_id)
        return job

    def _schedule_resume(self, job, reason, deadline_s=None):
        self.service.require_dispatch(job.task.workflow_id)
        if self.stopping or self.service.stopping:
            raise ValueError("Coordinator is stopping")
        if job.cancel.is_set() or job.state not in {"needs_codex","interrupted"}:
            raise ValueError("Group control transition is no longer valid")
        if job.merged_sources and deadline_s is None and job.deadline_at <= now_ms():
            raise ValueError("Merged hard deadline expired; explicit caller-approved deadline extension required")
        job.notes.append(reason)
        job.revision += 1
        job.pending_action = None
        if job.merged_sources and deadline_s is None:
            remaining = (job.deadline_at - now_ms()) / 1000
            if remaining <= 0:
                raise ValueError("Merged hard deadline expired; explicit caller-approved deadline extension required")
            job.deadline = time.monotonic() + remaining
        else:
            job.deadline = time.monotonic() + (deadline_s or job.request.deadline_s)
            job.deadline_at = now_ms() + int((deadline_s or job.request.deadline_s) * 1000)
        job.state = "queued"
        self.service._save(job)
        self.pool.submit(self._run, job)

    def extend(self, group_id, reason, token_budget=100000000, approved=False):
        with self.service.capacity, self.lock:
            job = self._approved_job(group_id, approved, reason)
            with job.changed:
                if job.cancel.is_set() or job.state not in {"needs_codex","interrupted"}:
                    raise ValueError("Group was cancelled or already resumed")
                if not job.pending_action or job.pending_action["type"] not in {"request_extension", "group_budget_exhausted"}:
                    raise ValueError("Leader has not requested budget extension")
                if not isinstance(token_budget, int) or isinstance(token_budget, bool) or not job.task.token_budget < token_budget <= 100000000:
                    raise ValueError("Extended group budget must increase and be at most 100000000")
                previous_budget = job.task.token_budget
                previous_phase, previous_tasks = job.phase, job.next_tasks
                job.task.token_budget = token_budget
                if job.round == 0 and job.phase == "leader_plan":
                    job.phase = "initial_ready"
                    job.next_tasks = job.next_tasks or self._seed_tasks(job)
                try:
                    self._schedule_resume(job, "Codex approved budget extension: " + reason)
                except Exception:
                    if job.state != "queued":
                        job.task.token_budget = previous_budget
                        job.phase, job.next_tasks = previous_phase, previous_tasks
                    raise
        return self.status(group_id)

    def adjust_seats(self, group_id, member_count, reason, approved=False):
        with self.service.capacity, self.lock:
            job = self._approved_job(group_id, approved, reason)
            with job.changed:
                if job.cancel.is_set() or job.state not in {"needs_codex","interrupted"}:
                    raise ValueError("Group was cancelled or already resumed")
                limit = 32 if job.merged_sources else 8
                if not isinstance(member_count, int) or isinstance(member_count, bool) or not 3 <= member_count <= limit:
                    raise ValueError("Requested seats exceed this group's authorized bound")
                if not job.pending_action or job.pending_action["type"] != "request_seat_change":
                    raise ValueError("Leader has not requested a seat change")
                pending = job.pending_action.get("detail") or {}
                request = {k:v for k,v in asdict(job.request).items() if k != "max_output_tokens"}
                planned = self.plan(request | {"member_count": min(8, member_count)}, _member_count=member_count)
                next_tasks = pending.get("next_tasks") or self._seed_tasks(job,planned["roles"])
                if len(next_tasks) > member_count-1:
                    raise ValueError("Approved seats cannot cover the pending tasks")
                previous = job.member_count, job.roles, job.next_tasks, job.phase
                job.member_count = member_count
                job.roles = planned["roles"]
                job.next_tasks = next_tasks
                job.phase = "seat_adjusted"
                try:
                    self._schedule_resume(job, "Codex approved seat change: " + reason)
                except Exception:
                    if job.state != "queued":
                        job.member_count, job.roles, job.next_tasks, job.phase = previous
                    raise
        return self.status(group_id)

    def resume(self, group_id, reason, deadline_s=None, approved_deadline=False, retry_unknown_slices=False):
        if not isinstance(retry_unknown_slices, bool):
            raise ValueError("retry_unknown_slices must be a boolean")
        with self.service.capacity, self.lock:
            job = self._approved_job(group_id, True, reason)
            with job.changed:
                if job.cancel.is_set() or job.state not in {"needs_codex","interrupted"}:
                    raise ValueError("Group was cancelled or already resumed")
                if job.merged_sources and deadline_s is not None and approved_deadline is not True:
                    raise ValueError("Merged deadline extension requires explicit caller approval")
                if job.pending_action and job.pending_action["type"] in {"request_extension", "request_seat_change", "group_budget_exhausted"}:
                    raise ValueError("Pending budget/seat approval must be resolved explicitly")
                if job.round == 0 and job.phase == "leader_plan" and job.pending_action:
                    job.phase = "leader_plan_retry"
                if job.slice_state and job.phase == "research_round":
                    if not retry_unknown_slices and any(r["state"] == "unknown" for r in job.slice_state["records"]):
                        raise ValueError("Unknown paid slice outcomes require explicit retry_unknown_slices=true or reconciliation")
                    for record in job.slice_state["records"]:
                        if record["state"] == "failed" or (retry_unknown_slices and record["state"] == "unknown"):
                            record["state"] = "pending"
                if job.pending_action and job.pending_action["type"]=="invalid_leader_decision":
                    if job.round == 0:
                        job.phase = "leader_plan_retry"
                    saved=[r for r in job.results if r.get("role")=="leader" and r.get("ok") and r.get("round")==job.round]
                    if saved:
                        try:
                            decision=parse_leader_decision(saved[-1]["content"].get("group_decision"), 32 if job.merged_sources else 8,
                                job.request.max_slices if job.request.slice_mode == "adaptive" else None)
                        except ValueError:
                            decision=None
                        if decision and decision["action"]=="submit_codex" and not self._missing_slots(job) and not self._unresolved_requests(job):
                            job.decisions.append(decision);job.notes.append(reason);job.revision+=1
                            job.state="needs_codex"
                            job.pending_action={"type":"submit_codex","detail":decision,"caller_approval_required":True}
                            self.service._save(job)
                            return self.status(group_id)
                        if decision and job.round > 0 and job.phase == "leader_review":
                            job.phase = "leader_review_cached"
                previous_rounds = job.request.max_rounds, job.task.max_attempts
                if job.pending_action and job.pending_action["type"] == "round_limit":
                    if job.request.max_rounds >= 64:
                        raise ValueError("Research group reached the maximum 64 rounds")
                    job.request.max_rounds = min(64,job.request.max_rounds+12)
                    job.task.max_attempts = job.request.max_rounds * max(27, job.request.max_slices * 3) + 6
                try:
                    self._schedule_resume(job, "Codex requested continuation: " + reason, deadline_s)
                except Exception:
                    if job.state != "queued":
                        job.request.max_rounds, job.task.max_attempts = previous_rounds
                    raise
        return self.status(group_id)

    def complete(self, group_id, accepted, note=""):
        with self.service.capacity, self.lock:
            job = self._restore(group_id)
            with job.changed:
                if job.cancel.is_set() or job.state != "needs_codex" or not job.pending_action or job.pending_action["type"] != "submit_codex":
                    raise ValueError("Group has no final report awaiting Codex review")
                if not isinstance(accepted, bool):
                    raise ValueError("accepted must be a boolean")
                reject_secrets(note)
                job.notes.append(note)
                job.revision += 1
                job.pending_action = {"type": "codex_review", "accepted": accepted, "source": "caller_reported"}
                job.state = "succeeded" if accepted else "needs_codex"
                self.service._save(job)
        return self.status(group_id)

    def complete_member(self, group_id, worker, model, content, usage=None):
        """Accept an explicit caller-side handoff, then require leader review."""
        with self.service.capacity, self.lock:
            job=self._restore(group_id)
            with job.changed:
                if job.state!="needs_codex" or job.cancel.is_set() or not isinstance(worker,int) or isinstance(worker,bool):
                    raise ValueError("Group has no active caller-side member handoff")
                matches=[r for r in job.results if r.get("worker")==worker and r.get("codex_fallback") and r.get("role")!="leader"]
                if len(matches)!=1 or not isinstance(model,str) or not model or len(model)>100:
                    raise ValueError("Unknown member handoff or model")
                reject_secrets(model)
                raw=validate_result({"text":json.dumps(content),"usage":clean_usage(usage),"observed_urls":[],"tool_calls":[]},False)
                requests = self._information_requests(job, {"content": raw["content"], "worker": worker})
                measured=reported_tokens(usage)
                amount=measured if measured is not None else 128000
                job.tokens_used+=amount
                job.revision += 1
                if measured is None:job.tokens_estimated+=amount
                old=matches[0]
                result={**raw,"worker":worker,"slot":old["slot"],"round":old["round"],"phase":old["phase"],
                    "role":old["role"],"ok":True,"route":{"source":"codex_agent","model":model,"route_id":"caller:"+model},
                    "execution_source":"caller_reported_codex_agent","token_measurement":"reported" if measured is not None else "estimated"}
                for name in ("slice", "member_slot", "responds_to", "responds_to_requests", "support", "exploration"):
                    if name in old:
                        result[name] = old[name]
                job.results[job.results.index(old)]=result
                if old.get("exploration"):
                    job.exploration_tokens_used += amount
                result["information_requests"] = requests
                if job.slice_state and old["round"] == job.round:
                    for record in job.slice_state["records"]:
                        if record["id"] == old.get("slice", {}).get("id"):
                            record.update({"state": "succeeded", "worker": worker})
                job.attempts.append({"worker":worker,"source":"codex_agent","model":model,"route_id":"caller:"+model,
                    "started_at":now_ms(),"state":"succeeded","tokens_charged":amount,"usage":clean_usage(usage),
                    "token_measurement":"reported" if measured is not None else "estimated","native_backed":True,
                    "exploration": old.get("exploration", False)})
                job.pending_action={"type":"caller_member_completed","detail":{"worker":worker,"source":"caller_reported"},"caller_approval_required":True}
                if job.tokens_used>job.task.token_budget:job.pending_action["type"]="group_budget_exhausted"
                self.service._save(job)
                self._publish_member_report(job, result)
        return self.status(group_id)

    def shutdown(self):
        self.stopping = True
        for job in list(self.jobs.values()):
            if job.state in {"queued", "running", "cancelling"}:
                job.cancel.set()
        self.pool.shutdown(wait=True, cancel_futures=False)
