"""MCP JSON-RPC stdio transport, forwarding to the resident local service."""

import json
import sys

from .daemon import LocalClient
from .models import TASK_KINDS, MAX_OUTPUT_TOKENS, MAX_TASK_TOKENS, DEFAULT_REPORT_CHARS, MAX_REPORT_CHARS
from .group_contract import GROUP_SCHEMA
from .preferences import SCHEMA as PREFERENCE_SCHEMA
from . import __version__
from .profiles import ABS_LIMITS


TASK_SCHEMA = {
    "type": "object", "additionalProperties": False, "required": ["kind", "prompt"],
    "properties": {
        "kind": {"type": "string", "enum": sorted(TASK_KINDS)},
        "preference_scope": {"type": "string", "maxLength": 200,
                             "description": "Unique expiring current-round scope from router_preferences; attach to every ordinary/group task in that round"},
        "routing_strategy": {"type": "string", "enum": ["auto", "quality", "speed", "reliability", "token_efficiency"], "default": "auto",
                             "description": "Soft ranking preference only; never bypasses quality, quota, verified recovery, capability, budget or deadline gates"},
        "prompt": {"type": "string", "description": "Bounded task, not an entire repository or conversation"},
        "context": {"type": "string", "description": "Only necessary redacted source excerpts or evidence"},
        "deadline_s": {"type": "number", "minimum": 5, "maximum": ABS_LIMITS["max_deadline_s"]},
        "timeout_mode": {"type": "string", "enum": ["activity", "hard"], "default": "activity"},
        "hard_deadline_s": {"type": "number", "minimum": 5, "maximum": ABS_LIMITS["max_hard_deadline_s"], "default": 3600},
        "ttft_s": {"type": ["number", "null"], "minimum": 1, "maximum": ABS_LIMITS["max_ttft_s"]},
        "idle_s": {"type": "number", "minimum": 1, "maximum": ABS_LIMITS["max_idle_s"], "default": 180},
        "stall_grace_s": {"type": "number", "minimum": 0, "maximum": 300, "default": 15},
        "price_weight": {"type": "number", "minimum": 0, "maximum": 100, "default": 30},
        "prefer_free": {"type": "boolean", "default": True},
        "prefer_new_models": {"type": "boolean", "default": True},
        "output_mode": {"type": "string", "enum": ["fixed", "adaptive", "native"],
                        "description": "Adaptive scales reasoning with excerpt size; explicit max_output_tokens defaults to fixed. Native opts into the reported model ceiling."},
        "queue_wait_s": {"type": "number", "minimum": 0, "maximum": ABS_LIMITS["max_queue_wait_s"], "default": 120},
        "report_max_chars": {"type": "integer", "minimum": 1000, "maximum": ABS_LIMITS["max_report_chars"], "default": DEFAULT_REPORT_CHARS,
                             "description": "Explicit final JSON report capacity; large reports still require complete evidence and bounded collection."},
        "fanout": {"type": "integer", "minimum": 1, "maximum": ABS_LIMITS["max_fanout"]},
        "verify": {"type": "boolean", "description": "Independent reports followed by a discrepancy check; defaults true for fanout>1"},
        "needs_web": {"type": "boolean"}, "min_quality": {"type": "number", "minimum": 0, "maximum": 100},
        "max_attempts": {"type": "integer", "minimum": 1, "maximum": ABS_LIMITS["max_attempts"], "default": 6},
        "max_output_tokens": {"type": "integer", "minimum": 256, "maximum": ABS_LIMITS["max_output_tokens"],
                              "description": "Total generation ceiling including hidden reasoning; logical tasks default to 32768, other tasks to 8192"},
        "priority": {"type": "integer", "minimum": 0, "maximum": 100},
        "preferred_route": {"type": "string"}, "allow_fallback": {"type": "boolean", "default": True,
            "description": "Keep true for ordinary work; false is an explicit single-route diagnostic constraint, with no automatic replacement"},
        "allowed_routes":{"type":["array","null"],"maxItems":128,"items":{"type":"string"},"description":"Optional ordered hard route pool; an empty list forbids all routes. Codex fallback cannot escape it."},
        "base_revision": {"type": "string"}, "context_label": {"type": "string"},
        "workflow_id": {"type": "string"}, "work_key": {"type": "string", "description": "Explicit goal+purpose key to suppress duplicated work"},
        "token_budget": {"type": "integer", "minimum": 1000, "maximum": ABS_LIMITS["max_task_tokens"],
                         "description": "Shared job ceiling including retries. Defaults stay capped at 200000; explicit large output/native mode can scale to 4000000."},
        "focus_urls": {"type": "array", "maxItems": 4, "items": {"type": "string"}, "description": "Known authoritative URLs; avoids broad searches when sources are already known"},
        "focus_terms": {"type": "array", "maxItems": 8, "items": {"type": "string"}, "description": "Keywords for a bounded fetched-page excerpt"},
        "web_tool_budget": {"type": "integer", "minimum": 2, "maximum": 12},
    },
}


def tool(name, description, properties, required=(), read_only=False):
    return {"name": name, "description": description,
            "inputSchema": {"type": "object", "properties": properties, "required": list(required), "additionalProperties": False},
            "annotations": {"readOnlyHint": read_only, "destructiveHint": False, "openWorldHint": not read_only}}


ID = {"type": "string"}
REFS = {"type": "array", "maxItems": 4, "items": {"type": "object", "additionalProperties": False, "required": ["group_id"],
    "properties": {"group_id": ID, "worker": {"type": "integer", "minimum": 0}, "message_id": ID,
                   "url": {"type": "string", "maxLength": 600}, "anchor": {"type": "string", "maxLength": 300}}}}
MESSAGE_KIND = {"type": "string", "enum": ["assignment", "evidence", "conflict", "progress", "suggestion", "merge_proposal"]}
TOOLS = [
    tool("router_dispatch", "Unified durable intake for task, group or workflow (inline or saved_workflow_id). Submit with a stable request_id; query it after ambiguous transport. Never replay unknown paid outcomes. status/collect/report/list/cancel remain available when dispatch is disabled. Large reports are losslessly paged with report, worker, node_id and offset. Existing supervision, route pool, budget and capacity gates apply.",
         {"action": {"type": "string", "enum": ["plan", "submit", "status", "collect", "report", "list", "cancel", "topology", "events", "coordinate"]},
          "request": {"type": "object", "additionalProperties": False, "required": ["kind"], "properties": {
              "kind": {"type": "string", "enum": ["task", "group", "workflow"]}, "task": TASK_SCHEMA, "request": GROUP_SCHEMA,
              "definition": {"type": "object"}, "saved_workflow_id": ID, "expected_revision": {"type": "integer", "minimum": 0},
              "input_text": {"type": "string", "maxLength": ABS_LIMITS["max_workflow_input_chars"]}, "workflow_id": ID,
              "execution": {"type": "object", "description": "Caller-owned workspace root, exact writable_paths and optional fixed command registry; models cannot expand this grant"}}},
          "request_id": ID, "dispatch_id": ID, "acknowledge_unknown_risk": {"type": "boolean"}, "workflow_id": ID,
          "cursor": {"type": "integer", "minimum": 0}, "limit": {"type": "integer", "minimum": 1, "maximum": 64},
          "max_chars": {"type": "integer", "minimum": 2000, "maximum": 30000}, "node_id": ID,
          "worker": {"type": "integer", "minimum": 0}, "offset": {"type": "integer", "minimum": 0},
          "expected_revision": {"type": "integer", "minimum": 0}, "event_id": ID,
          "decision": {"type": "object", "description": "Actual caller Codex assignment, exact review proposal correction, or evidence-digest acceptance; messages alone never approve edits"}}, ["action"]),
    tool("router_scheduling", "Read/edit user dispatch switches, expiring ordered model pool and saved DAGs. Refresh scans native catalogs only; check uses the existing bounded generation probe and real Luna gate. Save requires the current revision.",
         {"action": {"type": "string", "enum": ["get", "save", "refresh", "check"]}, "value": {"type": "object"},
          "expected_revision": {"type": "integer", "minimum": 0}, "route_id": ID, "workflow_id": ID, "luna_supervision": {"type": "boolean", "default": True}}),
    tool("router_workflow_plan", "Validate a saved visual DAG and inspect every exact node route without generation.",
         {"definition": {"type": "object"}, "workflow_id": ID}, ["definition"], True),
    tool("router_workflow_submit", "Execute an explicitly selected bounded visual DAG. Exact node routes, dependency success, shared budgets/capacity and real Luna supervision remain mandatory; unknown paid nodes are not replayed.",
         {"definition": {"type": "object"}, "input_text": {"type": "string", "maxLength": ABS_LIMITS["max_workflow_input_chars"]}, "workflow_id": ID,
          "request_id":ID,"acknowledge_unknown_risk":{"type":"boolean","default":False}}, ["definition"]),
    tool("router_workflow_status", "Read a durable visual DAG run and its evidence. Interrupted/unknown nodes require caller review.", {"run_id": ID}, ["run_id"], True),
    tool("router_workflow_cancel", "Cancel only this visual workflow's pending nodes and owned jobs.", {"run_id": ID}, ["run_id"]),
    tool("router_inventory", "List discovered external model routes, native-call health, and redacted GTT quota signals. Default output is compact; model listing alone is not a generation probe.", {"refresh": {"type": "boolean"}, "detailed": {"type": "boolean"}}, read_only=True),
    tool("router_plan", "Explain external-first route choices before sending a bounded task; respects quality, deadline, quota, concurrency, and caller-reported Codex coverage.", {"task": TASK_SCHEMA}, ["task"], True),
    tool("router_recheck", "Run one small generation check after authentication/quota recovery. Requires the external interface switch and a leased Luna supervisor in workflow_id (default: default); respects fresh quota, shared idle capacity and probe budgets.", {"route_id": ID, "workflow_id": ID}, ["route_id"]),
    tool("router_preferences", "Create, merge-update, inspect or clear a unique expiring current-round preference scope. Exact source/model/route IDs get a soft boost. Concurrency bias/cap affect future reservations; never cancel paid work, bypass gates, change seats or increase global limits.",
         {"action": {"type": "string", "enum": ["set", "get", "clear"]}, "scope_id": {"type": "string", "maxLength": 200},
          "preferences": PREFERENCE_SCHEMA, "ttl_s": {"type": "integer", "minimum": 60, "maximum": 604800}}, ["action"]),
    tool("router_submit", "Submit an async bounded external task. First spawn a real Luna monitor and register role=supervisor in the same workflow. Requires the external interface switch; returns immediately with a job ID.", {"task": TASK_SCHEMA}, ["task"]),
    tool("router_status", "Read compact progress and attempt history without reloading worker reports or prompts.", {"job_id": ID}, ["job_id"], True),
    tool("router_collect", "Collect completed evidence reports early, with cursor and bounded output. wait_ms is capped at 5000; conclusions remain advisory.", {"job_id": ID, "wait_ms": {"type": "integer", "maximum": 5000}, "cursor": {"type": "integer"}, "max_chars": {"type": "integer", "minimum": 2000, "maximum": 30000}}, ["job_id"], True),
    tool("router_cancel", "Cancel only this coordinator job's process trees and pending attempts.", {"job_id": ID}, ["job_id"]),
    tool("router_feedback", "After independently reviewing a completed report, record whether its evidence was accepted; updates task-specific model calibration.", {"job_id": ID, "worker": {"type": "integer"}, "accepted": {"type": "boolean"}}, ["job_id", "worker", "accepted"]),
    tool("router_sync_codex_agents", "Register actual caller-executed Codex agents for a short lease. Scheduling requires a running Luna role=supervisor in the same workflow. This is caller-reported liveness, not identity authentication; renew while the real monitor runs. One owner's list replaces its previous list.",
         {"owner": ID, "lease_s": {"type": "integer", "minimum": 15, "maximum": 600}, "agents": {"type": "array", "maxItems": 16, "items": {"type": "object", "additionalProperties": False, "required": ["id", "status"], "properties": {"id": ID, "model": ID, "workflow_id": ID, "work_key": ID, "preference_scope": ID, "role": {"type": "string", "enum": ["worker", "supervisor"], "default": "worker"}, "status": {"type": "string", "enum": ["running", "idle", "finished"]}}}}}, ["owner", "agents"]),
    tool("router_complete_codex", "Return a caller-executed Codex subagent report for an explicit ordinary/group-member handoff; use group_id as job_id for members. Group leader review is still required. The server does not spawn Codex or verify its model.", {"job_id": ID, "worker": {"type": "integer"}, "model": ID, "content": {"type": "object"}, "usage": {"type": "object"}}, ["job_id", "worker", "model", "content"]),
    tool("router_refresh_benchmarks", "Optionally refresh exact mapped model priors using the official Artificial Analysis API and local AA_API_KEY. Missing credentials preserve the cache; never request secrets in chat.", {}),
    tool("router_events", "Read append-only redacted workflow events/messages with a bounded cursor. TTL, gaps and provenance remain explicit; messages never grant execution permission.",
         {"workflow_id": ID, "cursor": {"type": "integer", "minimum": 0}, "limit": {"type": "integer", "minimum": 1, "maximum": 64}, "group_id": ID, "role": ID}, ["workflow_id"], True),
    tool("router_message", "Submit an authorized caller suggestion or scoped evidence excerpt. Native leaders/members use bounded report JSON; this API cannot impersonate them or a supervisor.",
         {"workflow_id": ID, "to_group": ID, "to_role": ID, "kind": MESSAGE_KIND, "summary": {"type": "string", "minLength": 1, "maxLength": 1200}, "evidence_refs": REFS,
          "from_group": ID, "event_id": {"type": "string", "maxLength": 200}, "ttl_s": {"type": "integer", "minimum": 60, "maximum": 604800}}, ["workflow_id", "to_role", "kind", "summary"]),
    tool("router_supervisor", "Read real workflow revisions/native activity and report a caller-executed supervisor's bounded advisory proposal. Never spawns Codex, reads hidden thought or automatically changes paid work, budget or seats.",
         {"action": {"type": "string", "enum": ["status", "report"]}, "workflow_id": ID, "group_ids": {"type": "array", "maxItems": 32, "uniqueItems": True, "items": ID},
          "event_id": ID, "actor_state": {"type": "object", "additionalProperties": False, "required": ["phase", "last_progress_at"], "properties": {
              "phase": {"type": "string", "enum": ["thinking", "coding", "tool", "waiting", "idle"]}, "last_progress_at": {"type": "integer", "minimum": 0},
              "active_until": {"type": "integer", "minimum": 0}, "deadline_at": {"type": "integer", "minimum": 0}, "idle_threshold_s": {"type": "integer", "minimum": 30, "maximum": 3600}}},
          "proposal": {"type": "object", "additionalProperties": False, "required": ["summary"], "properties": {"kind": MESSAGE_KIND,
              "summary": {"type": "string", "maxLength": 1200}, "group_ids": {"type": "array", "maxItems": 4, "uniqueItems": True, "items": ID}, "evidence_refs": REFS}}}, ["action", "workflow_id"]),
    tool("router_group_merge", "Prepare an atomic caller-approved merge of 2..4 quiescent groups in one workflow. Cumulative seats up to 32 share existing concurrency caps; usage stays billed once to sources, budget <=100m and earliest hard deadline is retained. If source budgets exceed 100m, explicitly set token_budget between consumed usage and source budget sum. Requires resume after review.",
         {"group_ids": {"type": "array", "minItems": 2, "maxItems": 4, "uniqueItems": True, "items": ID}, "approved": {"type": "boolean", "const": True},
          "reason": {"type": "string", "minLength": 1, "maxLength": 2000}, "expected_revisions": {"type": "object", "additionalProperties": {"type": "integer", "minimum": 0}},
          "token_budget": {"type": "integer", "minimum": 1000, "maximum": 100000000}}, ["group_ids", "approved", "reason", "expected_revisions"]),
    tool("router_group_plan", "Plan a bounded multi-model research group without generating content. Group token budget defaults to and cannot exceed 50000000 at creation.", {"request": GROUP_SCHEMA}, ["request"], True),
    tool("router_group_submit", "Submit a bounded research group with shared role, round, deadline and token budgets.", {"request": GROUP_SCHEMA}, ["request"]),
    tool("router_group_status", "Read research group state, role progress and shared accounting.", {"group_id": ID}, ["group_id"], True),
    tool("router_group_collect", "Collect bounded research group results; wait_ms is capped at 5000.", {"group_id": ID, "cursor": {"type": "integer", "minimum": 0}, "max_chars": {"type": "integer", "minimum": 2000, "maximum": 30000}, "wait_ms": {"type": "integer", "minimum": 0, "maximum": 5000}}, ["group_id"], True),
    tool("router_group_cancel", "Cancel one research group and its pending role attempts.", {"group_id": ID}, ["group_id"]),
    tool("router_group_extend", "Extend a group's token budget only with explicit caller approval; the hard maximum is 100000000.", {"group_id": ID, "reason": {"type": "string", "minLength": 1, "maxLength": 2000}, "token_budget": {"type": "integer", "minimum": 1000, "maximum": 100000000}, "approved": {"type": "boolean", "const": True}}, ["group_id", "reason", "approved"]),
    tool("router_group_adjust_seats", "Change roles only with caller approval; ordinary groups allow 3..8, merged groups up to 32 while concurrency remains capped.", {"group_id": ID, "member_count": {"type": "integer", "minimum": 3, "maximum": 32}, "reason": {"type": "string", "minLength": 1, "maxLength": 2000}, "approved": {"type": "boolean", "const": True}}, ["group_id", "member_count", "reason", "approved"]),
    tool("router_group_resume", "Resume a paused/interrupted group. Unknown paid slice outcomes are never replayed unless retry_unknown_slices explicitly accepts that risk. Merged deadlines require approval.", {"group_id": ID, "reason": {"type": "string", "minLength": 1, "maxLength": 2000}, "deadline_s": {"type": "integer", "minimum": 5, "maximum": 604800}, "approved_deadline": {"type": "boolean"}, "retry_unknown_slices": {"type": "boolean", "default": False}}, ["group_id", "reason"]),
    tool("router_group_complete", "Record the caller's reviewed disposition of a completed group.", {"group_id": ID, "accepted": {"type": "boolean"}, "note": {"type": "string", "maxLength": 2000}}, ["group_id", "accepted"]),
]


# Cached clients can still call these aliases. New clients discover one intake,
# so no second submit policy can compete with the configured scheduler.
from .dispatch import LEGACY_SUBMISSIONS
COMPAT_TOOLS = [t for t in TOOLS if t["name"] in LEGACY_SUBMISSIONS]
TOOLS = [t for t in TOOLS if t["name"] not in LEGACY_SUBMISSIONS]


def serve(config_path, client=None):
    client = client or LocalClient(config_path)
    initialized = False
    ready = False
    valid_tools = {t["name"] for t in TOOLS + COMPAT_TOOLS}
    def respond(value):
        sys.stdout.write(json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n")
        sys.stdout.flush()
    while True:
        raw = sys.stdin.buffer.readline(16 * 1024 * 1024 + 1)
        if not raw:
            return
        if len(raw) > 16 * 1024 * 1024:
            respond({"jsonrpc": "2.0", "id": None, "error": {"code": -32600, "message": "MCP message exceeds limit"}})
            return
        request_id = None
        try:
            request = json.loads(raw)
            if not isinstance(request, dict) or request.get("jsonrpc") != "2.0":
                raise ValueError("Invalid JSON-RPC request")
            request_id = request.get("id")
            method, params = request.get("method"), request.get("params", {})
            if method == "notifications/initialized":
                ready = initialized
                continue
            if request_id is None:
                continue
            if method == "initialize":
                version = params.get("protocolVersion")
                if version not in {"2025-03-26", "2025-06-18", "2025-11-25"}:
                    version = "2025-11-25"
                result = {"protocolVersion": version, "serverInfo": {"name": "gtt-model-router", "version": __version__}, "capabilities": {"tools": {}}}
                initialized = True
            elif method == "ping":
                result = {}
            elif not ready:
                respond({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32002, "message": "MCP initialization is incomplete"}})
                continue
            elif method == "tools/list":
                result = {"tools": TOOLS}
            elif method == "tools/call":
                name = params.get("name")
                if name not in valid_tools:
                    raise ValueError("Unknown MCP tool")
                try:
                    content = client.call(name, params.get("arguments", {}))
                    result = {"content": [{"type": "text", "text": json.dumps(content, ensure_ascii=False, separators=(",", ":"))}], "structuredContent": content}
                except (ValueError, TypeError, KeyError) as error:
                    result = {"isError": True, "content": [{"type": "text", "text": str(error)[:300]}]}
            else:
                respond({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "Method not found"}})
                continue
            respond({"jsonrpc": "2.0", "id": request_id, "result": result})
        except json.JSONDecodeError:
            respond({"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": "Parse error"}})
        except (ValueError, TypeError, KeyError) as error:
            respond({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32602, "message": str(error)[:300]}})
