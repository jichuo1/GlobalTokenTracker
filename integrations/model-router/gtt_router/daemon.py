"""One user-local resident service shared by all Codex MCP clients."""

from contextlib import contextmanager
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import time
import urllib.error
import urllib.request

from .api import dispatch
from .service import RouterService
from .processes import background_options
from . import __version__

RUNTIME_CONTRACT_VERSION = 2
CONTRACT_METHODS = {"router_plan", "router_submit", "router_group_plan", "router_group_submit", "router_group_resume",
                    "router_group_extend", "router_group_adjust_seats", "router_group_merge", "router_recheck"}


def load_config(path):
    path = Path(path).expanduser().resolve()
    config = json.loads(path.read_text(encoding="utf-8-sig"))
    if config.get("schema_version") != 1:
        raise ValueError("Unsupported coordinator configuration version")
    for key in ["state_dir", "ledger_path", "benchmark_file"]:
        if not isinstance(config.get(key), str) or not Path(config[key]).is_absolute():
            raise ValueError(key + " must be an absolute path")
    sources = config.get("sources", [])
    if not isinstance(sources, list) or len(sources) > 64:
        raise ValueError("sources must be a bounded list")
    ids = set()
    registry = config.get("source_registry", [])
    if not isinstance(registry,list) or len(registry)>64 or any(not isinstance(row,dict) or not isinstance(row.get("id"),str) for row in registry):
        raise ValueError("Source registry must be a bounded metadata list")
    for source in sources:
        if not isinstance(source.get("id"), str) or source["id"] in ids:
            raise ValueError("Source identities must be unique")
        ids.add(source["id"])
        if source.get("enabled", True):
            command = source.get("command")
            if not isinstance(command, list) or not command or any(not isinstance(arg, str) for arg in command):
                raise ValueError("Source commands must be argument arrays")
            if not Path(command[0]).is_absolute():
                raise ValueError("Source executable must be an absolute path")
            if source.get("adapter") == "opencode_http":
                if not isinstance(source.get("discovery_dir"), str) or not Path(source["discovery_dir"]).is_absolute():
                    raise ValueError("OpenCode discovery_dir must be an absolute path")
                names = source.get("mcp_names", [])
                if not isinstance(names, list) or len(names) > 64 or any(not isinstance(n, str) or len(n) > 128 for n in names):
                    raise ValueError("OpenCode MCP names must be a bounded list")
            process_env = source.get("process_env", {})
            if not isinstance(process_env, dict) or any(not isinstance(k,str) or not isinstance(v,str) or "\0" in k+v for k,v in process_env.items()):
                raise ValueError("Source process_env must contain string values")
            if source.get("adapter") == "dsh_headless":
                entries=source.get("profile_entries",[])
                if not isinstance(entries,list) or not 1<=len(entries)<=512 or any(not isinstance(r,dict) or not isinstance(r.get("id"),str) or not isinstance(r.get("name"),str) for r in entries):
                    raise ValueError("DSH profile entries must be a bounded identity list")
    recovery = config.get("recovery", {})
    if not isinstance(recovery, dict) or not isinstance(recovery.get("enabled", False), bool):
        raise ValueError("recovery.enabled must be a boolean")
    for key, low, high in [("poll_s", 10, 3600), ("initial_delay_s", 60, 86400),
                           ("max_backoff_s", 60, 604800), ("probe_timeout_s", 5, 120),
                           ("max_probes_per_hour", 0, 10), ("max_tokens_per_hour", 0, 200000)]:
        if key in recovery:
            value = recovery[key]
            if not isinstance(value, (int, float)) or isinstance(value, bool) or not low <= value <= high:
                raise ValueError("Invalid recovery setting: " + key)
    interface = config.get("gtt_interface", {})
    if not isinstance(interface, dict) or any(type(interface.get(k, True)) is not bool for k in ["enabled", "allow_legacy_fallback"]):
        raise ValueError("Invalid gtt_interface settings")
    timeout = interface.get("timeout_s", 5)
    if not isinstance(timeout, (int, float)) or isinstance(timeout, bool) or not 1 <= timeout <= 30:
        raise ValueError("Invalid gtt_interface.timeout_s")
    from .observations import interface_command
    interface_command(interface)
    ttl = config.get("limits", {}).get("quota_ttl_s", 300)
    if not isinstance(ttl, int) or isinstance(ttl, bool) or not 1 <= ttl <= 86400:
        raise ValueError("Invalid limits.quota_ttl_s")
    config["_config_path"] = str(path)
    for key in ("max_active_groups","max_queued_groups","max_retained_groups"):
        value=config.get("limits",{}).get(key,3 if key=="max_active_groups" else 32)
        upper=8 if key=="max_active_groups" else 128
        if not isinstance(value,int) or isinstance(value,bool) or not 1<=value<=upper:
            raise ValueError("Invalid group limit: "+key)
    foreground_slots=config.get("limits",{}).get("group_reserved_foreground_slots",1)
    if not isinstance(foreground_slots,int) or isinstance(foreground_slots,bool) or not 0<=foreground_slots<=32:
        raise ValueError("Invalid group_reserved_foreground_slots")
    return config


def secure_write(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as file:
        json.dump(value, file, ensure_ascii=False, indent=2)
    tmp.replace(path)
    if os.name != "nt":
        path.chmod(0o600)


@contextmanager
def startup_lock(state_dir):
    path = Path(state_dir) / "daemon-start.lock"
    path.parent.mkdir(parents=True, exist_ok=True)
    file = path.open("a+b")
    if path.stat().st_size == 0:
        file.write(b"0")
        file.flush()
    file.seek(0)
    try:
        if os.name == "nt":
            import msvcrt
            msvcrt.locking(file.fileno(), msvcrt.LK_LOCK, 1)
        else:
            import fcntl
            fcntl.flock(file.fileno(), fcntl.LOCK_EX)
        yield
    finally:
        file.seek(0)
        if os.name == "nt":
            import msvcrt
            msvcrt.locking(file.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            import fcntl
            fcntl.flock(file.fileno(), fcntl.LOCK_UN)
        file.close()


class LocalClient:
    def __init__(self, config_path, autostart=True):
        self.config_path = str(Path(config_path).resolve())
        self.config = load_config(config_path)
        self.endpoint_path = Path(self.config["state_dir"]) / "daemon-endpoint.json"
        if autostart:
            self.ensure_running()

    def _endpoint(self):
        data = json.loads(self.endpoint_path.read_text(encoding="utf-8"))
        port = data.get("port")
        if not isinstance(port, int) or not 1 <= port <= 65535 or data.get("config_path") != self.config_path:
            raise ValueError("Invalid local endpoint")
        return data

    def _request(self, method=None, params=None, timeout=30):
        endpoint = self._endpoint()
        url = "http://127.0.0.1:" + str(endpoint["port"]) + ("/rpc" if method else "/health")
        body = json.dumps({"method": method, "params": params or {}}).encode() if method else None
        request = urllib.request.Request(url, data=body,
                                         headers={"Authorization": "Bearer " + endpoint["token"], "Content-Type": "application/json"})
        # Loopback credentials must never travel through a configured HTTP proxy.
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(request, timeout=timeout) as response:
            result = json.loads(response.read(4 * 1024 * 1024))
        if result.get("error"):
            raise ValueError(result["error"])
        return result.get("result", result)

    def ensure_running(self):
        with startup_lock(self.config["state_dir"]):
            try:
                if self._request(timeout=2).get("service") == "gtt-model-router":
                    return
            except (OSError, ValueError, urllib.error.URLError):
                pass
            entry = Path(__file__).resolve().parent.parent / "router.py"
            args = [sys.executable, str(entry), "daemon", "--config", self.config_path]
            log = Path(self.config["state_dir"]) / "daemon.log"
            with log.open("ab") as output:
                process = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=output, stderr=output,
                                           **background_options(detached=True), start_new_session=os.name != "nt")
            until = time.monotonic() + 12
            while time.monotonic() < until:
                if process.poll() is not None:
                    raise ValueError("Resident coordinator failed to start; inspect the local daemon log")
                try:
                    if self._request(timeout=1).get("service") == "gtt-model-router":
                        return
                except (OSError, ValueError, urllib.error.URLError):
                    pass
                time.sleep(.15)
            raise ValueError("Resident coordinator startup deadline exceeded")

    def call(self, method, params=None):
        try:
            if method in CONTRACT_METHODS:
                health = self.runtime_info()
                if health.get("runtime_contract_version", 0) != RUNTIME_CONTRACT_VERSION:
                    raise ValueError("resident_protocol_mismatch: installed coordinator contract differs from this client; align the runtime version and reconnect; no task dispatched")
                task = (params or {}).get("task", {})
                if isinstance(task, dict) and "report_max_chars" in task and "report_capacity_v1" not in health.get("features", []):
                    raise ValueError("resident_feature_missing: installed coordinator does not support report_max_chars; align the runtime version; no task dispatched")
            return self._request(method, params, timeout=150 if method == "router_recheck" else 30)
        except (OSError, urllib.error.URLError):
            # Submission is never automatically replayed after an ambiguous outcome.
            raise ValueError("Local coordinator unavailable; inspect status before resubmitting") from None

    def runtime_info(self):
        return self._request(timeout=2)


def serve(config_path):
    config = load_config(config_path)
    service = RouterService(config)
    token = secrets.token_urlsafe(32)
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format, *args):
            pass

        def authorized(self):
            supplied = self.headers.get("Authorization", "")
            return hmac.compare_digest(supplied, "Bearer " + token)

        def respond(self, status, value):
            body = json.dumps(value, ensure_ascii=False).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if not self.authorized():
                self.respond(403, {"error": "Unauthorized local request"})
            elif self.path == "/health":
                self.respond(200, {"service": "gtt-model-router", "pid": os.getpid(), "version": __version__,
                                   "runtime_contract_version": RUNTIME_CONTRACT_VERSION, "features": ["report_capacity_v1"]})
            else:
                self.respond(404, {"error": "Unknown local route"})

        def do_POST(self):
            if not self.authorized() or self.path != "/rpc":
                self.respond(403, {"error": "Unauthorized local request"})
                return
            try:
                self.connection.settimeout(10)
                length = int(self.headers.get("Content-Length", "0"))
                if not 0 < length <= 16 * 1024 * 1024:
                    raise ValueError("Request size exceeds the coordinator limit")
                payload = json.loads(self.rfile.read(length))
                result = dispatch(service, payload["method"], payload.get("params", {}))
                self.respond(200, {"result": result})
            except (ValueError, TypeError, KeyError) as error:
                self.respond(200, {"error": str(error)[:300]})
            except Exception as error:
                self.respond(200, {"error": "Coordinator error: " + type(error).__name__})
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    endpoint_path = Path(config["state_dir"]) / "daemon-endpoint.json"
    secure_write(endpoint_path, {"port": server.server_port, "token": token, "pid": os.getpid(), "config_path": str(Path(config_path).resolve())})
    try:
        server.serve_forever(poll_interval=.5)
    finally:
        server.server_close()
        service.shutdown()
