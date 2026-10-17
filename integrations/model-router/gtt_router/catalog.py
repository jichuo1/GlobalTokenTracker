"""Discover callable routes separately from GTT's historical software inventory."""

import copy
import json
from pathlib import Path
import re
import subprocess
import threading
from urllib.parse import urlsplit, urlunsplit

from .benchmarks import load_profiles
from .observations import load_observation
from .models import Route, WorkerFailure, now_ms, number
from .processes import background_options, hidden_console_command
from .pricing import normalize_pricing
from .model_updates import annotate_updates


def read_json(path):
    if not path:
        return {}
    try:
        return json.loads(Path(path).expanduser().read_text(encoding="utf-8-sig"))
    except (OSError, ValueError):
        return {}


def merge(base, overlay):
    result = copy.deepcopy(base)
    for key, value in overlay.items():
        result[key] = merge(result.get(key, {}), value) if isinstance(value, dict) and isinstance(result.get(key, {}), dict) else copy.deepcopy(value)
    return result


def zcode_output_limit(builtin, personal, provider_id, provider, model):
    """Resolve the native ordered output-range rules without exporting config."""
    limit = None
    config = provider.get("config", {})
    template_id = provider.get("templateId")
    templates = builtin.get("providerConfigRules", {}).get("templateRules", [])
    for template in templates:
        if template.get("templateId") == template_id:
            config = merge(template.get("config", {}), config)
    api = config.get("api", {}) or {}
    base = api.get("baseUrl", "")
    base = base if isinstance(base, str) else ""
    if base:
        parsed = urlsplit(base)
        base = urlunsplit((parsed.scheme, parsed.netloc, parsed.path.rstrip("/"), parsed.query, parsed.fragment))
    def matches(pattern, value, insensitive=False):
        try:
            return isinstance(pattern, str) and isinstance(value, str) and re.fullmatch(pattern, value, re.I if insensitive else 0) is not None
        except re.error:
            return False
    groups = ["modelRules", "modelApiRules", "providerSiteRules", "templateModelRules", "builtinProviderModelRules", "providerModelRules", "manualProviderModelRules"]
    for document in (builtin, personal):
        for group in groups:
            for row in document.get("modelConfigRules", {}).get(group, []):
                if group in {"builtinProviderModelRules", "providerModelRules", "manualProviderModelRules"}:
                    match = row.get("providerId") == provider_id and row.get("modelId") == model
                elif group == "templateModelRules":
                    match = row.get("templateId") == template_id and row.get("modelId") == model
                else:
                    match = matches(row.get("modelMatch"), model, True)
                    if row.get("apiTypeMatch") is not None:
                        match = match and matches(row["apiTypeMatch"], api.get("type", ""))
                    if group == "providerSiteRules":
                        match = match and matches(row.get("baseUrlMatch"), base)
                if not match:
                    continue
                if group == "manualProviderModelRules":
                    limit = None  # native manual overrides clear editable max
                spec = (row.get("config", {}).get("optionSpecs") or {}).get("maxOutputTokens") or {}
                if "max" in spec:
                    value = spec["max"]
                    limit = value if isinstance(value, int) and not isinstance(value, bool) and value > 0 else None
    return limit


def zcode_routes(source):
    builtin = read_json(source.get("builtin_config")).get("config", {})
    personal = read_json(source.get("provider_config")).get("config", {})
    providers = {}
    for config in [builtin, personal]:
        for row in config.get("providerConfigRules", {}).get("providerRules", []):
            key = row.get("providerId")
            if key:
                providers[key] = merge(providers.get(key, {}), row)
    rules = {}
    for config in [builtin, personal]:
        model_rules = config.get("modelConfigRules", {})
        for row in model_rules.get("providerModelRules", []) + model_rules.get("manualProviderModelRules", []):
            if row.get("providerId") and row.get("modelId"):
                key = (row["providerId"], row["modelId"])
                rules[key] = merge(rules.get(key, {}), row.get("config", {}))
    result = []
    for provider_id, provider in providers.items():
        config = provider.get("config", {})
        if provider.get("enabled", True) is False or config.get("visibility") == "hidden":
            continue
        # Account-plan provisioning depends on desktop runtime headers; don't promise
        # a callable account route from a static entitlement record.
        access = config.get("access", {})
        if access.get("type") != "api-key":
            continue
        ids = list(dict.fromkeys(config.get("builtinModelIds", []) + config.get("personalModelIds", [])))
        for model in ids:
            spec = rules.get((provider_id, model), {})
            if spec.get("enabled", True) is False:
                continue
            levels = spec.get("optionSpecs", {}).get("reasoningLevel", {}).get("values", [])
            effort = levels[-1] if levels else ""
            window = spec.get("properties", {}).get("contextWindow")
            result.append(Route(source=source["id"], provider=provider_id, model=model, effort=effort,
                                label=provider.get("providerName", provider_id), auth_hint=bool(access.get("apiKey")),
                                context_window=int(window) if number(window) else None,
                                output_limit=zcode_output_limit(builtin, personal, provider_id, provider, model),
                                adapter="zcode_rpc", runtime=source, quota_app="zcode", max_parallel=source.get("max_parallel", 2)))
    return result


class Catalog:
    def __init__(self, config):
        self.config = config
        self.routes = []
        self.ledger = {}
        self.discovery_errors = []
        self.refreshed_at = 0
        self._model_cache = {}
        self.lock = threading.RLock()

    def _agy_models(self, source, force):
        cached = self._model_cache.get(source["id"])
        if cached and not force and now_ms() - cached[0] < 1800000:
            return cached[1]
        command = source.get("command", [])
        if not command or not Path(command[0]).is_file():
            raise ValueError("executable_missing")
        result = subprocess.run(hidden_console_command(command) + ["models"], stdin=subprocess.DEVNULL, capture_output=True,
                                text=True, encoding="utf-8", errors="replace", timeout=25,
                                **background_options())
        if result.returncode != 0:
            raise ValueError("model_discovery_failed")
        models = []
        for line in result.stdout.splitlines():
            if re.fullmatch(r"[A-Za-z0-9._:/\-]+\t.+", line):
                models.append(line.split("\t", 1))
        if not models:
            raise ValueError("model_catalog_empty")
        self._model_cache[source["id"]] = (now_ms(), models)
        return models

    def refresh(self, force=False):
        with self.lock:
            profiles = load_profiles(self.config["benchmark_file"])
            routes, errors = [], []
            for source in self.config.get("sources", []):
                if not source.get("enabled", True):
                    continue
                try:
                    command = source.get("command", [])
                    if not command or not Path(command[0]).is_file():
                        raise ValueError("executable_missing")
                    adapter = source.get("adapter")
                    if adapter == "zcode_rpc":
                        routes.extend(zcode_routes(source))
                    elif adapter == "antigravity_cli":
                        for model, label in self._agy_models(source, force):
                            effort = model.rsplit("-", 1)[-1] if model.rsplit("-", 1)[-1] in {"high", "medium", "low"} else "high"
                            routes.append(Route(source=source["id"], provider="google-account", model=model, effort=effort,
                                                label=label, auth_hint=True, adapter=adapter, runtime=source,
                                                capabilities=["text", "web"], quota_app="gemini_antigravity", max_parallel=source.get("max_parallel", 2)))
                    elif adapter == "command_json":
                        if not source.get("trusted"):
                            raise ValueError("command_adapter_not_trusted")
                        for model in source.get("models", []):
                            routes.append(Route(source=source["id"], provider=source.get("provider", source["id"]), model=model["id"],
                                                effort=model.get("effort", ""), label=model.get("name", model["id"]), auth_hint=True,
                                                adapter=adapter, runtime=source, capabilities=source.get("capabilities", ["text"]),
                                                context_window=model.get("context_window"), output_limit=model.get("output_limit"), quota_app=source.get("quota_app", ""),
                                                max_parallel=source.get("max_parallel", 1)))
                    elif adapter in {"opencode_http", "claude_cli", "codebuddy_cli", "dsh_headless"}:
                        models = source.get("models", [])
                        if adapter == "opencode_http":
                            cached = self._model_cache.get(source["id"])
                            if force or not cached or now_ms() - cached[0] > 1800000:
                                from .native_agents import discover_opencode
                                models = discover_opencode(source)
                                self._model_cache[source["id"]] = (now_ms(), models)
                            else:
                                models = cached[1]
                        for model in models:
                            routes.append(Route(source=source["id"], provider=model.get("provider", source.get("provider", "native-account")),
                                                model=model["id"], effort=model.get("effort", ""), label=model.get("name", model["id"]),
                                                auth_hint=source.get("auth_hint", True), adapter=adapter, runtime=source,
                                                capabilities=["text"], context_window=model.get("context_window"), output_limit=model.get("output_limit"), quota_app=source.get("quota_app", source["id"]),
                                                pricing=normalize_pricing(model.get("pricing"), "native_opencode_catalog") if adapter == "opencode_http" else {},
                                                catalog_metadata=model.get("catalog_metadata", {}),
                                                max_parallel=source.get("max_parallel", 1)))
                    else:
                        raise ValueError("adapter_not_registered")
                except (OSError, ValueError, subprocess.SubprocessError, WorkerFailure) as error:
                    if isinstance(error, WorkerFailure):
                        errors.append({"source": source.get("id"), "reason": error.code})
                        continue
                    errors.append({"source": source.get("id"), "reason": str(error) if isinstance(error, ValueError) else type(error).__name__})
            seen = set()
            for route in routes:
                if route.id in seen:
                    raise ValueError("Duplicate route identity")
                seen.add(route.id)
                # Effort-specific aliases take precedence. A max-effort
                # benchmark cannot silently grade a lower-effort route.
                profile_key = route.model + "-" + route.effort if route.effort else route.model
                route.profile = copy.deepcopy(profiles.get(profile_key, profiles.get(route.model, {})))
                measured_effort = route.profile.get("effort")
                if measured_effort in {"minimal","low","medium","high","xhigh","max"} and measured_effort != route.effort:
                    for metric in ["intelligence","terminal_bench"]:
                        route.profile.pop(metric,None)
                    route.profile["benchmark_effort_mismatch"] = True
                override = self.config.get("profile_overrides", {}).get(route.model, {})
                route.profile.update(override)
                price_override = self.config.get("pricing_overrides", {}).get(route.id)
                if price_override is not None:
                    route.pricing = normalize_pricing(price_override, "caller_configured_route")
                route.catalog_metadata["activity_observability"] = "final_only" if route.adapter == "zcode_rpc" else "stream_or_native_messages"
                route.quota_account = route.runtime.get("quota_account_key")
                if route.context_window is None:
                    route.context_window = route.profile.get("context_window")
            annotate_updates(routes)
            self.routes, self.discovery_errors = routes, errors
            self.ledger = load_observation(self.config)
            self.refreshed_at = now_ms()
            return self.public()

    def public(self):
        return {"refreshed_at": self.refreshed_at, "routes": [r.public() for r in self.routes],
                "ledger": self.ledger, "discovery_errors": self.discovery_errors,
                "availability_note": "auth_hint and model listing are not successful-generation evidence"}
