"""Native OpenCode HTTP and print-mode agents; credentials stay with their clients."""
import base64
from contextlib import contextmanager
import fnmatch
import json
import os
from pathlib import Path
import secrets
import socket
import threading
import time
import urllib.error
import urllib.request

from .adapters import ExecutionContext, ProcessChannel, classify_error, native_slot
from .models import WorkerFailure, number
from .usage import clean_usage


def opencode_usage(tokens):
    if not tokens:
        return {}
    cache = tokens.get("cache", {})
    result = {"input_tokens": tokens.get("input", 0),
              "output_tokens": number(tokens.get("output"), 0) + number(tokens.get("reasoning"), 0),
              "reasoning_tokens": tokens.get("reasoning", 0),
              "cache_read_tokens": cache.get("read", 0), "cache_write_tokens": cache.get("write", 0)}
    total = number(tokens.get("total"), sum(number(result[k], 0) for k in result if k != "reasoning_tokens"))
    result["total_tokens"] = total
    clean = clean_usage(result)
    return clean if any(value > 0 for value in clean.values()) else {}

AGENT = "gtt-bounded-worker"
AGENT_STEPS = 2
TOOL_PERMISSIONS = {name: "deny" for name in ["*", "read", "glob", "grep", "list", "bash", "edit", "write", "apply_patch",
                                            "webfetch", "websearch", "task", "skill", "question", "todoread", "todowrite"]}
EFFORT_ORDER = {"none": 0, "minimal": 1, "low": 2, "medium": 3, "high": 4, "xhigh": 5, "max": 6}


class OpenCodeServer:
    def __init__(self, source, context, output_tokens=8192):
        self.context = context
        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1", 0))
            port = reserved.getsockname()[1]
        password = secrets.token_urlsafe(32)
        env = os.environ.copy()
        env.update({"OPENCODE_SERVER_PASSWORD": password, "OPENCODE_SERVER_USERNAME": "gtt",
                    "OPENCODE_DISABLE_AUTOUPDATE": "true", "OPENCODE_DISABLE_CLAUDE_CODE": "true",
                    "OPENCODE_DISABLE_PROJECT_CONFIG": "true"})
        if output_tokens is not None:
            env["OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX"] = str(output_tokens)
        else:
            env.pop("OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX", None)
        overlay = {"autoupdate": False, "permission": TOOL_PERMISSIONS,
                   "mcp": {name: {"enabled": False} for name in source.get("mcp_names", [])},
                   "agent": {AGENT: {"mode": "primary", "steps": AGENT_STEPS, "permission": TOOL_PERMISSIONS,
                                     "prompt": "Use only the caller's supplied material. Return the requested JSON. No tools or delegation.",
                                     "options": {"maxOutputTokens": output_tokens}}}}
        if output_tokens is None:
            overlay["agent"][AGENT]["options"].pop("maxOutputTokens")
        env["OPENCODE_CONFIG_CONTENT"] = json.dumps(overlay)
        self.transport = ProcessChannel(source["command"] + ["serve", "--hostname", "127.0.0.1", "--port", str(port)], context, env)
        self.base = "http://127.0.0.1:" + str(port)
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.headers = {"Authorization": "Basic " + base64.b64encode(("gtt:" + password).encode()).decode(), "Content-Type": "application/json"}

    def request(self, path, body=None, limit=8388608):
        self.context.check()
        req = urllib.request.Request(self.base + path, data=json.dumps(body).encode() if body is not None else None, headers=self.headers)
        try:
            with self.opener.open(req, timeout=max(.1, min(2, self.context.deadline - time.monotonic()))) as reply:
                data = reply.read(limit + 1)
            self.context.check()
            if len(data) > limit:
                raise WorkerFailure("protocol_output_too_large")
            return json.loads(data) if data else None
        except urllib.error.HTTPError as error:
            # Error bodies can contain provider credentials; classify in memory only.
            raise classify_error(error.read(16000).decode("utf-8", "replace")) from None
        except (ValueError, OSError, urllib.error.URLError):
            raise WorkerFailure("native_connection_failed", scope="source", retry_after_s=30) from None

    def ready(self):
        while True:
            self.context.check()
            if self.transport.process.poll() is not None:
                raise classify_error(self.transport.stderr.decode("utf-8", "replace"))
            try:
                if self.request("/global/health", limit=8192).get("healthy") is True:
                    return
            except WorkerFailure as failure:
                if failure.code != "native_connection_failed":
                    raise
            self.context.cancel.wait(.1)

    def verify_permissions(self):
        agents = self.request("/agent", limit=1048576)
        own = next((a for a in agents if a.get("name") == AGENT), None)
        if not own or own.get("mode") != "primary" or own.get("steps") != AGENT_STEPS:
            raise WorkerFailure("native_permission_contract_failed", scope="source")
        rules = own.get("permission", [])
        for tool in TOOL_PERMISSIONS:
            if tool == "*":
                continue
            action = None
            for rule in rules:
                if fnmatch.fnmatchcase(tool, rule.get("permission", "")) and rule.get("pattern") == "*":
                    action = rule.get("action")
            if action != "deny":
                raise WorkerFailure("native_permission_contract_failed", scope="source")

    def close(self):
        self.transport.close()


@contextmanager
def opencode_server(source, context, output_tokens=8192):
    server = OpenCodeServer(source, context, output_tokens)
    try:
        server.ready()
        server.verify_permissions()
        yield server
    finally:
        server.close()


def discover_opencode(source):
    folder = Path(source["discovery_dir"])
    context = ExecutionContext(threading.Event(), time.monotonic() + 25, folder)
    with opencode_server(source, context) as server:
        data = server.request("/config/providers")
        rows = []
        # This endpoint contains configured/connected providers, unlike /provider
        # whose `all` includes the entire unauthenticated model database.
        for provider in data.get("providers", []):
            for model_id, model in provider.get("models", {}).items():
                if model.get("status") == "deprecated" or model.get("disabled") is True:
                    continue
                if model.get("capabilities", {}).get("output", {}).get("text") is False:
                    continue
                variants = model.get("variants", {})
                variants = {k: v for k, v in variants.items() if not isinstance(v, dict) or v.get("disabled") is not True}
                # Retain every model; choose the highest explicitly enumerated
                # reasoning variant. Never infer effort from its display name.
                recognized = [v for v in variants if v in EFFORT_ORDER]
                effort = max(recognized, key=EFFORT_ORDER.get) if recognized else ""
                rows.append({"provider": provider["id"], "id": model_id, "name": model.get("name", model_id),
                             "effort": effort, "variants": sorted(variants), "context_window": model.get("limit", {}).get("context"),
                             "output_limit": model.get("limit", {}).get("output"),
                             "pricing": model.get("cost", {}),
                             "catalog_metadata": {key: model[key] for key in ["family", "release_date", "status"]
                                                  if isinstance(model.get(key), str) and len(model[key]) <= 200}})
                if len(rows) > 1024:
                    raise ValueError("opencode_model_catalog_too_large")
        if not rows:
            raise ValueError("opencode_model_catalog_empty")
        return rows


def opencode(route, prompt, task, context):
    with native_slot(route, context) as slot:
        with opencode_server(route.runtime, slot, task.max_output_tokens) as server:
            session = server.request("/session", {"title": "GTT bounded auxiliary task",
                                                   "permission": [{"permission": "*", "pattern": "*", "action": "deny"}]}, limit=65536)
            session_id = session.get("id")
            if not isinstance(session_id, str) or not session_id.startswith("ses_") or not session_id.replace("_", "").isalnum():
                raise WorkerFailure("invalid_native_session")
            tool_ids = server.request("/experimental/tool/ids", limit=65536)
            tools = {name: False for name in tool_ids}
            tools["*"] = False
            body = {"agent": AGENT, "model": {"providerID": route.provider, "modelID": route.model},
                    "tools": tools, "parts": [{"type": "text", "text": prompt}]}
            if route.effort:
                body["variant"] = route.effort
            server.request("/session/" + session_id + "/prompt_async", body, limit=65536)
            last_usage, last_metering = {}, {}
            while True:
                try:
                    slot.check()
                    messages = server.request("/session/" + session_id + "/message?limit=8", limit=16777216 if getattr(task, "_research_group", False) else slot.protocol_frame_bytes())
                except WorkerFailure as failure:
                    # Pending responses may already report billable reasoning.
                    # Cancellation/idle timeout must retain that observed usage.
                    if failure.usage is None and last_usage:
                        failure.usage = last_usage
                    failure.metering.update(last_metering)
                    raise
                for message in reversed(messages):
                    info = message.get("info", {})
                    if info.get("role") != "assistant":
                        continue
                    usage = opencode_usage(info.get("tokens", {}))
                    metering = {"native_backed": True, "actual_model": info.get("modelID", route.model)}
                    if isinstance(info.get("id"), str) and info["id"].startswith("msg_") and info["id"].replace("_", "").isalnum():
                        metering["native_dedup_key"] = "opencode:msg:" + info["id"]
                    if usage:
                        last_usage = usage
                        slot.last_usage.update(usage)
                    last_metering = metering
                    if info.get("error"):
                        failure = classify_error(info["error"])
                        failure.usage, failure.metering = usage, metering
                        raise failure
                    parts = message.get("parts", [])
                    if any(part.get("type") == "tool" for part in parts):
                        raise WorkerFailure("unexpected_tool_call", usage=usage, metering=metering)
                    # Polling only reads this request's existing conversation;
                    # it never makes an extra generation to ask if it is busy.
                    # Updated timestamps and identical busy/empty snapshots do
                    # not renew progress. Content must grow or a real native
                    # step must change, with stable message/part identities.
                    message_key = str(info.get("id", "assistant"))
                    for index, part in enumerate(parts):
                        if not isinstance(part, dict):
                            continue
                        key = message_key + ":" + str(part.get("id", index))
                        if part.get("type") in {"text", "reasoning", "thinking"}:
                            slot.report_content("output" if part["type"] == "text" else "reasoning", key, part.get("text"))
                        elif part.get("type") in {"step-start", "step-finish"}:
                            slot.report_activity("native_phase", key=key, version=part["type"])
                    if not info.get("time", {}).get("completed"):
                        continue
                    if info.get("providerID") != route.provider or info.get("modelID") != route.model:
                        raise WorkerFailure("model_selection_mismatch", usage=usage, metering=metering)
                    if info.get("finish") in {"length", "max_tokens"}:
                        from .adapters import truncation_failure
                        raise truncation_failure(task.max_output_tokens, "".join(p.get("text", "") for p in parts if p.get("type") == "text"), usage, metering)
                    tokens = info.get("tokens", {})
                    total = sum(number(tokens.get(k), 0) for k in ["input", "output", "reasoning"])
                    total += sum(number(tokens.get("cache", {}).get(k), 0) for k in ["read", "write"])
                    total = number(tokens.get("total"), total)
                    return {"text": "".join(p.get("text", "") for p in parts if p.get("type") == "text"),
                            "finish_reason": info.get("finish") if info.get("finish") in {"stop", "length", "error"} else "unknown",
                            "usage": usage if total > 0 else {}, "native_usage": tokens, **metering,
                            "actual_selection": {"provider": info["providerID"], "model": info["modelID"], "effort": route.effort},
                            "tool_calls": [], "observed_urls": []}
                slot.cancel.wait(.15)


def print_agent(route, prompt, task, context):
    with native_slot(route, context) as slot:
        env = os.environ.copy()
        env.update(route.runtime.get("process_env", {}))
        if task.max_output_tokens is not None:
            env["CLAUDE_CODE_MAX_OUTPUT_TOKENS"] = str(task.max_output_tokens)
        settings = {"disableAllHooks": True}
        args = route.runtime["command"] + ["--print", "--input-format", "text", "--output-format", "stream-json", "--verbose",
                                           "--tools", "", "--disallowedTools", "*", "--strict-mcp-config",
                                           "--mcp-config", '{"mcpServers":{}}', "--no-session-persistence", "--max-turns", "1",
                                           "--permission-mode", "plan", "--model", route.model, "--settings", json.dumps(settings),
                                           "--system-prompt", "You are a bounded text-only auxiliary worker. Use only supplied material. Return the caller's JSON contract. No tools or delegation."]
        if route.adapter == "claude_cli":
            # stream-json alone returns whole messages. This documented native
            # flag exposes the actual text/thinking deltas for idle monitoring.
            # Other print clients must explicitly confirm support first.
            args += ["--safe-mode", "--include-partial-messages"]
        if route.effort:
            args += ["--effort", route.effort]
        transport = ProcessChannel(args, slot, env)
        last_usage = {}
        try:
            transport.send_text(prompt)
            observed_model = None
            while True:
                event = transport.read()
                if event.get("type") == "system" and event.get("subtype") == "init":
                    observed_model = event.get("model")
                if event.get("type") == "assistant":
                    if any(p.get("type") == "tool_use" for p in event.get("message", {}).get("content", [])):
                        raise WorkerFailure("unexpected_tool_call")
                if event.get("type") != "result":
                    continue
                usage = clean_usage(event.get("usage", {}))
                last_usage = usage
                if event.get("subtype") in {"error_max_turns", "error_max_output_tokens"}:
                    from .adapters import truncation_failure
                    raise truncation_failure(task.max_output_tokens, event.get("result", ""), usage)
                if event.get("is_error"):
                    failure = classify_error(event.get("result") or event.get("errors") or event.get("subtype"))
                    failure.usage = usage
                    raise failure
                if event.get("subtype") not in {None, "success"}:
                    raise WorkerFailure("incomplete_native_result")
                actual = list(event.get("modelUsage", {}))
                if len(actual) > 1:
                    raise WorkerFailure("model_selection_mismatch")
                actual_model = actual[0] if len(actual) == 1 else observed_model
                allowed = {route.model, *route.runtime.get("model_aliases", {}).get(route.model, [])}
                # Stable native CLI aliases may resolve to a newer canonical
                # model. Require the declared family and init/result agreement.
                alias_prefix = {"opus":"claude-opus-","sonnet":"claude-sonnet-","haiku":"claude-haiku-"}.get(route.model)
                if route.adapter == "claude_cli" and alias_prefix and actual_model and actual_model.startswith(alias_prefix) and observed_model == actual_model:
                    allowed.add(actual_model)
                if actual_model and actual_model not in allowed:
                    raise WorkerFailure("model_selection_mismatch", usage=usage, metering={"actual_model": actual_model})
                usage = event.get("usage", {})
                total = sum(number(usage.get(k), 0) for k in ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"])
                return {"text": event.get("result", ""), "usage": {**usage, "total_tokens": int(total)} if total > 0 else {},
                        "actual_selection": {"model": actual_model or route.model, "effort": route.effort},
                        "tool_calls": [], "observed_urls": []}
        except WorkerFailure as failure:
            if failure.usage is None and (last_usage or slot.last_usage):
                failure.usage = last_usage or slot.last_usage
            raise
        finally:
            transport.close()


def dsh_overlay(source, route, workspace, output_tokens):
    entries = source.get("profile_entries", [])
    if not entries or len(entries) > 512 or any(not str(row.get("name", "")).startswith("@deepseek-ai/") for row in entries):
        raise WorkerFailure("unverified_native_profile", scope="source")
    ids = {row["id"] for row in entries}
    if not {"agent-loop", "agent-default-model", "tools"} <= ids:
        raise WorkerFailure("native_permission_contract_failed", scope="source")
    blocked = {ident for ident in ids if ident.startswith("tool-") or ident.startswith("mcp")
               or ident in {"agent-instructions", "skill-filesystem", "skills", "skill-instructions", "session-title-llm"}}
    # Factory services remain mounted; their callable tools are disabled.
    # Disabling the factory itself would prevent the primary worker starting.
    patches = [{"id": ident, "disabled": True} for ident in sorted(blocked)]
    model = {"provider": route.provider, "model": route.model, "reasoningEffort": route.effort or "max"}
    patches += [{"id": "agent-default-model", "config": model},
                {"id": "agent-loop", "config": {"agents": [{"id": "main", **model, "maxTokens": output_tokens, "cwd": str(workspace)}]}}]
    if output_tokens is None:
        patches[-1]["config"]["agents"][0].pop("maxTokens")
    return patches


def dsh(route, prompt, task, context):
    with native_slot(route, context) as slot:
        overlay = slot.workspace / "gtt-dsh-overlay.json"
        overlay.write_text(json.dumps(dsh_overlay(route.runtime, route, slot.workspace, task.max_output_tokens)), encoding="utf-8")
        env = os.environ.copy()
        env.update(route.runtime.get("process_env", {}))
        args = route.runtime["command"] + ["--profile", "headless", "--patch", str(overlay), "--json"]
        transport = ProcessChannel(args, slot, env)
        usage = {}
        try:
            transport.send_text(prompt)
            while True:
                event = transport.read()
                kind = event.get("type")
                if kind == "error":
                    raise classify_error(event.get("message", "native_agent_failed"))
                event_kind = str(kind or "") + " " + str(event.get("phase", ""))
                if any(word in event_kind.lower() for word in ["tool", "permission", "approval"]):
                    raise WorkerFailure("unexpected_tool_call")
                if kind == "status" and isinstance(event.get("usage"), dict):
                    native = event["usage"]
                    total = number(native.get("totalTokens"))
                    if total is not None:
                        usage = {"total_tokens": int(total)}
                if kind == "final":
                    return {"text": event.get("text", ""), "usage": usage,
                            "actual_selection": {"provider": route.provider, "model": route.model, "effort": route.effort,
                                                 "evidence": "validated_process_overlay"}, "tool_calls": [], "observed_urls": []}
        finally:
            transport.close()
