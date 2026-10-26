"""Trusted, journaled workspace operations proposed by text-only model workers.

Grants are supplied by the caller, never by model output. Native model tools
remain disabled. A registered command is trusted caller code, not an OS sandbox;
only the exact preauthorized argv can be selected by an operator.
"""

from contextlib import ExitStack, contextmanager
import copy
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import signal
import stat
import subprocess
import tempfile
import threading
import time

from .models import now_ms, redact, reject_secrets
from .processes import background_options, owned_process_command


_REGISTRY_LOCK = threading.Lock()
_PATH_LOCKS = {}
_WORKSPACE_LOCKS = {}
_PROTECTED = {".git", ".ssh", ".aws", "auth.json", "credentials.json", "provider_config.json",
              "daemon-endpoint.json", "id_rsa", "id_ed25519"}
_KEY_SUFFIXES = {".key", ".pem", ".pfx", ".p12"}
_DEVICES = {"con", "prn", "aux", "nul", "conin$", "conout$",
            *{f"com{i}" for i in range(1, 10)}, *{f"lpt{i}" for i in range(1, 10)}}
_TERMINAL = {"applied", "conflict", "unknown", "cancelled", "failed"}


class WorkspaceBusy(ValueError):
    pass


def _json(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":"))


def _digest(value):
    return hashlib.sha256(value).hexdigest()


def _text(value, name, maximum=200, empty=False):
    if not isinstance(value, str) or len(value) > maximum or (not empty and not value.strip()) or "\x00" in value:
        raise ValueError(name + " must be bounded text")
    reject_secrets(value)
    return value


def _integer(value, name, low, high):
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        raise ValueError(name + " is out of range")
    return value


def _relative(value, *, directory=False):
    _text(value, "workspace path", 1024, empty=directory)
    if directory and value in {"", "."}:
        return "."
    parts = PurePosixPath(value).parts
    if (not value or "\\" in value or ":" in value or value.startswith("/")
            or any(c in value for c in "*?[]") or not parts
            or value != "/".join(parts) or any(p in {".", ".."} for p in parts)):
        raise ValueError("workspace paths must be exact POSIX relative filenames")
    for part in parts:
        lower = part.casefold()
        if (part.rstrip(" .") != part or lower in _PROTECTED or lower.startswith(".env")
                or Path(lower).suffix in _KEY_SUFFIXES or lower.split(".", 1)[0] in _DEVICES):
            raise ValueError("protected workspace path")
    return "/".join(parts)


def _no_links(path):
    """Reject symlinks and Windows junction/reparse points before resolving."""
    for component in reversed((path, *path.parents)):
        try:
            info = component.lstat()
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400):
            raise ValueError("workspace symlinks and reparse points are forbidden")


def _root(value):
    path = Path(_text(value, "workspace root", 4096))
    if not path.is_absolute():
        raise ValueError("workspace root must be absolute")
    _no_links(path)
    resolved = path.resolve(strict=True)
    if not resolved.is_dir() or resolved.name.casefold() in _PROTECTED:
        raise ValueError("workspace root must be an existing workspace directory")
    return resolved


def _target(root, relative, *, directory=False):
    name = _relative(relative, directory=directory)
    path = root if name == "." else root.joinpath(*PurePosixPath(name).parts)
    _no_links(path)
    resolved = path.resolve(strict=False)
    if not resolved.is_relative_to(root):
        raise ValueError("workspace path escaped grant root")
    if path.exists():
        if directory and not path.is_dir():
            raise ValueError("command cwd must be a directory")
        if not directory and not path.is_file():
            raise ValueError("operator path must be a regular file")
        if not directory and path.stat().st_nlink > 1:
            raise ValueError("operator hard-linked files are forbidden")
    elif directory:
        raise ValueError("command cwd must exist")
    return path


def validate_grant(value):
    """Normalize explicit caller authority without changing the workspace."""
    allowed = {"root", "writable_paths", "commands", "max_file_bytes", "max_operations"}
    if not isinstance(value, dict) or set(value) - allowed:
        raise ValueError("invalid operator grant fields")
    root = _root(value.get("root"))
    paths = value.get("writable_paths")
    if not isinstance(paths, list) or len(paths) > 256:
        raise ValueError("writable_paths must be a bounded exact file list")
    paths = [_relative(p) for p in paths]
    if len({os.path.normcase(p) for p in paths}) != len(paths):
        raise ValueError("duplicate grant paths")
    for name in paths:
        _target(root, name)
    commands = value.get("commands", {})
    if not isinstance(commands, dict) or len(commands) > 32:
        raise ValueError("commands must be a bounded caller registry")
    normalized = {}
    for ident, command in commands.items():
        _text(ident, "command id", 100)
        if not isinstance(command, dict) or set(command) - {"argv", "cwd", "timeout_s"}:
            raise ValueError("invalid registered command")
        argv = command.get("argv")
        if not isinstance(argv, list) or not 1 <= len(argv) <= 64:
            raise ValueError("registered command argv must be a bounded list")
        argv = [_text(arg, "command argument", 8192, empty=True) for arg in argv]
        if len(_json(argv)) > 32768 or not Path(argv[0]).is_absolute() or not Path(argv[0]).is_file():
            raise ValueError("registered command requires an existing absolute executable")
        cwd = _relative(command.get("cwd", "."), directory=True)
        _target(root, cwd, directory=True)
        normalized[ident] = {"argv": argv, "cwd": cwd,
                             "timeout_s": _integer(command.get("timeout_s", 60), "command timeout", 1, 300)}
    return {"root": str(root), "writable_paths": paths, "commands": normalized,
            "max_file_bytes": _integer(value.get("max_file_bytes", 1048576), "max_file_bytes", 1, 4194304),
            "max_operations": _integer(value.get("max_operations", 32), "max_operations", 1, 128)}


def _path_lock(path):
    key = os.path.normcase(str(path))
    with _REGISTRY_LOCK:
        return _PATH_LOCKS.setdefault(key, threading.RLock())


@contextmanager
def _acquire(lock, cancel=None, *, blocking=True):
    if not blocking:
        if not lock.acquire(blocking=False):
            raise WorkspaceBusy("workspace_busy")
        try:
            yield
        finally:
            lock.release()
        return
    while not lock.acquire(timeout=.05):
        if cancel is not None and cancel.is_set():
            raise _Stopped("cancelled")
    try:
        if cancel is not None and cancel.is_set():
            raise _Stopped("cancelled")
        yield
    finally:
        lock.release()


@contextmanager
def _workspace_lease(root, exclusive=False, cancel=None, *, blocking=True):
    with _REGISTRY_LOCK:
        condition, state = _WORKSPACE_LOCKS.setdefault(os.path.normcase(str(root)),
            (threading.Condition(), {"readers": 0, "writer": False, "waiting_writers": 0}))
    with condition:
        if exclusive:
            state["waiting_writers"] += 1
        try:
            while state["writer"] or (state["readers"] if exclusive else state["waiting_writers"]):
                if not blocking:
                    raise WorkspaceBusy("workspace_busy")
                if cancel is not None and cancel.is_set():
                    raise _Stopped("cancelled")
                condition.wait(.05)
            if cancel is not None and cancel.is_set():
                raise _Stopped("cancelled")
            if exclusive:
                state["writer"] = True
            else:
                state["readers"] += 1
        finally:
            if exclusive:
                state["waiting_writers"] -= 1
            condition.notify_all()
    try:
        yield
    finally:
        with condition:
            if exclusive:
                state["writer"] = False
            else:
                state["readers"] -= 1
            condition.notify_all()


def _read(path, maximum):
    if not path.exists():
        return {"exists": False, "sha256": None, "content": ""}
    with path.open("rb") as stream:
        body = stream.read(maximum + 1)
    if len(body) > maximum:
        raise ValueError("operator file exceeds grant byte limit")
    try:
        content = body.decode("utf-8")
    except UnicodeDecodeError:
        raise ValueError("operator files must be UTF-8") from None
    reject_secrets(content)
    return {"exists": True, "sha256": _digest(body), "content": content}


class WorkspaceBroker:
    def __init__(self, service):
        self.service = service
        self.store = service.store
        with self.store.lock, self.store.conn:
            self.store.conn.execute("CREATE TABLE IF NOT EXISTS operator_operations("
                "identity TEXT PRIMARY KEY,run_id TEXT NOT NULL,node_id TEXT NOT NULL,assignment_id TEXT NOT NULL,"
                "operation_id TEXT NOT NULL,fingerprint TEXT NOT NULL,state TEXT NOT NULL,payload TEXT NOT NULL,"
                "UNIQUE(run_id,node_id,assignment_id,operation_id))")
            self.store.conn.execute("CREATE INDEX IF NOT EXISTS operator_assignment ON operator_operations(run_id,node_id,assignment_id)")
            rows = self.store.conn.execute("SELECT payload FROM operator_operations WHERE state='applying'").fetchall()
            for row in rows:
                record = json.loads(row[0])
                record.update(state="unknown", reason="interrupted_operation_requires_reconciliation", completed_at=now_ms())
                self._save(record)

    validate_grant = staticmethod(validate_grant)

    def snapshot(self, grant, paths, *, blocking=True):
        grant = validate_grant(grant)
        if not isinstance(paths, list) or len(paths) > 256:
            raise ValueError("snapshot paths must be bounded")
        names = [_relative(p) for p in paths]
        if len(set(names)) != len(names) or not set(names) <= set(grant["writable_paths"]):
            raise ValueError("snapshot paths are outside grant")
        root = Path(grant["root"])
        with ExitStack() as stack:
            stack.enter_context(_workspace_lease(root, blocking=blocking))
            for name in sorted(names):
                stack.enter_context(_acquire(_path_lock(root / name), blocking=blocking))
            return [{"path": name, **_read(_target(root, name), grant["max_file_bytes"])} for name in names]

    def receipts(self, run_id, node_id, assignment_id):
        for name, value in [("run id", run_id), ("node id", node_id), ("assignment id", assignment_id)]:
            _text(value, name)
        with self.store.lock:
            rows = self.store.conn.execute("SELECT payload FROM operator_operations WHERE run_id=? AND node_id=? "
                "AND assignment_id=? ORDER BY rowid", (run_id, node_id, assignment_id)).fetchall()
        return [self._public(json.loads(row[0])) for row in rows]

    def _save(self, record, on_progress=None):
        with self.store.lock, self.store.conn:
            self.store.conn.execute("INSERT INTO operator_operations VALUES(?,?,?,?,?,?,?,?) "
                "ON CONFLICT(identity) DO UPDATE SET state=excluded.state,payload=excluded.payload",
                (record["operation_key"], record["run_id"], record["node_id"], record["assignment_id"],
                 record["id"], record["fingerprint"], record["state"], _json(record)))
        if on_progress is not None:
            # Progress delivery cannot undo an issued operation. The durable
            # receipt is authoritative even when the caller's event log is full.
            try:
                on_progress({"operation_id": record["id"], **{k: v for k, v in record.items()
                    if k in {"state", "path", "command_id", "changed", "started_at", "completed_at", "reason", "exit_code"}}})
            except Exception:
                record["progress_delivery_failed"] = True
                with self.store.lock, self.store.conn:
                    self.store.conn.execute("UPDATE operator_operations SET payload=? WHERE identity=?",
                                            (_json(record), record["operation_key"]))

    def _existing(self, key):
        with self.store.lock:
            row = self.store.conn.execute("SELECT payload FROM operator_operations WHERE identity=?", (key,)).fetchone()
        return json.loads(row[0]) if row else None

    @staticmethod
    def _public(record):
        return copy.deepcopy({k: v for k, v in record.items() if k not in {"root", "fingerprint"}})

    def _gate(self, cancel):
        if cancel.is_set():
            raise _Stopped("cancelled")
        self.service.require_external_interface()
        self.service.require_scheduling()

    @staticmethod
    def _operations(grant, allowed_paths, operations):
        if not isinstance(allowed_paths, list) or len(allowed_paths) > 256:
            raise ValueError("assignment paths must be bounded")
        names = [_relative(p) for p in allowed_paths]
        if len(set(names)) != len(names) or not set(names) <= set(grant["writable_paths"]):
            raise ValueError("assignment paths exceed grant")
        if not isinstance(operations, list) or not 1 <= len(operations) <= grant["max_operations"]:
            raise ValueError("operation batch exceeds grant")
        parsed, ids, paths = [], set(), set()
        fields = {"write_text": {"id", "op", "path", "base_sha256", "content"},
                  "replace_text": {"id", "op", "path", "base_sha256", "old_text", "new_text"},
                  "run_command": {"id", "op", "command_id"}}
        for item in operations:
            if not isinstance(item, dict) or item.get("op") not in fields or set(item) - fields[item["op"]]:
                raise ValueError("unsupported operator operation fields")
            item = copy.deepcopy(item)
            ident = _text(item.get("id"), "operation id")
            if ident in ids:
                raise ValueError("duplicate operation id")
            ids.add(ident)
            if item["op"] == "run_command":
                command = _text(item.get("command_id"), "command id", 100)
                if command not in grant["commands"]:
                    raise ValueError("command is outside caller registry")
            else:
                name = _relative(item.get("path"))
                if name not in names:
                    raise ValueError("operation path is outside assignment")
                key = os.path.normcase(name)
                if key in paths:
                    raise ValueError("overlapping operation paths require a new batch")
                paths.add(key)
                item["path"] = name
                base = item.get("base_sha256")
                if base is not None and (not isinstance(base, str) or re.fullmatch(r"[0-9a-f]{64}", base) is None):
                    raise ValueError("base_sha256 must be a lowercase SHA-256 or null")
                item["base_sha256"] = base
                for field in (["content"] if item["op"] == "write_text" else ["old_text", "new_text"]):
                    text = _text(item.get(field), field, grant["max_file_bytes"], empty=field != "old_text")
                    if len(text.encode("utf-8")) > grant["max_file_bytes"]:
                        raise ValueError("operation text exceeds grant byte limit")
            parsed.append(item)
        return names, parsed

    def plan_operations(self, grant, allowed_paths, operations, snapshots):
        """Bundle independent same-baseline edits into one checked atomic replacement."""
        grant = validate_grant(grant)
        if not isinstance(operations, list) or not 1 <= len(operations) <= grant["max_operations"]:
            raise ValueError("operation batch exceeds grant")
        parsed, ids, groups = [], set(), {}
        for original in operations:
            _, one = self._operations(grant, allowed_paths, [original])
            operation = one[0]
            if operation["id"] in ids:
                raise ValueError("duplicate operation id")
            ids.add(operation["id"])
            parsed.append(operation)
            if operation["op"] != "run_command":
                groups.setdefault(os.path.normcase(operation["path"]), []).append(operation)
        sources = {os.path.normcase(item["path"]): item for item in snapshots}
        if any(len(edits) > 1 for edits in groups.values()) and any(op["op"] == "run_command" for op in parsed):
            raise ValueError("Commands and bundled file edits require separate reviewed batches")
        bundled, metadata = {}, []
        for path, edits in groups.items():
            if len(edits) == 1:
                continue
            source = sources.get(path)
            if (not source or not source.get("exists") or any(e["op"] != "replace_text" or
                    e["base_sha256"] != source["sha256"] for e in edits)):
                raise ValueError("Same-file edits need one existing immutable baseline and independent replacements")
            body = source["content"]
            spans = []
            for edit in edits:
                if body.count(edit["old_text"]) != 1:
                    raise ValueError("Bundled edits require unique matches in the authorized snapshot")
                start = body.index(edit["old_text"])
                spans.append((start, start + len(edit["old_text"]), edit["new_text"]))
            spans.sort()
            if any(left[1] > right[0] for left, right in zip(spans, spans[1:])):
                raise ValueError("Overlapping or chained edits require a new reviewed batch")
            first, last = spans[0][0], spans[-1][1]
            pieces, cursor = [], first
            for start, end, replacement in spans:
                pieces.extend((body[cursor:start], replacement))
                cursor = end
            pieces.append(body[cursor:last])
            operation = {"id": "bundle-" + _digest(_json(edits).encode())[:32], "op": "replace_text",
                "path": edits[0]["path"], "base_sha256": source["sha256"],
                "old_text": body[first:last], "new_text": "".join(pieces)}
            bundled[path] = operation
            metadata.append({"id": operation["id"], "path": operation["path"], "base_sha256": source["sha256"],
                "source_operation_ids": [e["id"] for e in edits], "snapshot_excerpt_only": bool(source.get("excerpt_only")),
                "old_span_sha256": _digest(operation["old_text"].encode()), "new_span_sha256": _digest(operation["new_text"].encode())})
        result, seen = [], set()
        for operation in parsed:
            path = os.path.normcase(operation["path"]) if operation["op"] != "run_command" else None
            if path in bundled:
                if path not in seen:
                    result.append(bundled[path])
                    seen.add(path)
            else:
                result.append(operation)
        self._operations(grant, allowed_paths, result)
        return result, metadata

    def execute(self, run_id, node_id, assignment_id, grant, allowed_paths, operations, cancel_event=None, on_progress=None):
        for name, value in [("run id", run_id), ("node id", node_id), ("assignment id", assignment_id)]:
            _text(value, name)
        grant = validate_grant(grant)
        names, operations = self._operations(grant, allowed_paths, operations)
        root = Path(grant["root"])
        cancel = cancel_event if cancel_event is not None else threading.Event()
        # Commands may touch every caller-authorized file; hold all those path
        # locks. Independent file-only assignments can still run concurrently.
        lock_names = grant["writable_paths"] if any(op["op"] == "run_command" for op in operations) else names
        receipts, changed = [], False
        with ExitStack() as stack:
            try:
                stack.enter_context(_workspace_lease(root, any(op["op"] == "run_command" for op in operations), cancel))
                for name in sorted(lock_names, key=os.path.normcase):
                    stack.enter_context(_acquire(_path_lock(root / name), cancel))
            except _Stopped:
                return {"state": "cancelled", "receipts": [], "partial": False, "reason": "cancelled_while_waiting_for_workspace"}
            prepared = []
            for operation in operations:
                key = _digest(_json([run_id, node_id, assignment_id, operation["id"]]).encode())
                fingerprint = _digest(_json({"grant": grant, "allowed_paths": names, "operation": operation}).encode())
                prior = self._existing(key)
                if prior is not None and prior["fingerprint"] != fingerprint:
                    raise ValueError("operator_operation_id_conflict")
                if prior is not None and prior["state"] == "applying":
                    prior.update(state="unknown", reason="operation_outcome_unknown", completed_at=now_ms())
                    self._save(prior)
                if prior is not None and prior["state"] in _TERMINAL:
                    if prior["state"] == "unknown" and prior["op"] != "run_command" and prior.get("after_sha256"):
                        current = _read(_target(root, prior["path"]), grant["max_file_bytes"])
                        if current["sha256"] == prior["after_sha256"]:
                            prior.update(state="applied", reconciled=True, reason="reconciled_after_hash", completed_at=now_ms())
                            self._save(prior)
                    receipt = self._public(prior)
                    receipt["deduplicated"] = True
                    if prior["state"] == "applied" and prior["op"] != "run_command":
                        current = _read(_target(root, prior["path"]), grant["max_file_bytes"])
                        if current["sha256"] != prior.get("after_sha256"):
                            return {"state": "conflict", "receipts": [*receipts, receipt], "partial": changed,
                                    "reason": "applied_result_changed"}
                    receipts.append(receipt)
                    if prior["state"] != "applied":
                        possible_effect = prior["state"] == "unknown" or (prior["op"] == "run_command" and "started_at" in prior)
                        return {"state": prior["state"], "receipts": receipts, "partial": changed or possible_effect}
                    changed |= prior.get("changed", prior["op"] == "run_command")
                    continue
                record = {"operation_key": key, "run_id": run_id, "node_id": node_id, "assignment_id": assignment_id,
                          "id": operation["id"], "op": operation["op"], "fingerprint": fingerprint,
                          "root": str(root), "state": "prepared", "created_at": now_ms()}
                if operation["op"] != "run_command":
                    path = _target(root, operation["path"])
                    before = _read(path, grant["max_file_bytes"])
                    if before["sha256"] != operation["base_sha256"]:
                        return {"state": "conflict", "receipts": receipts, "partial": changed,
                                "reason": "operator_base_hash_mismatch", "path": operation["path"]}
                    if operation["op"] == "replace_text":
                        if not before["exists"] or before["content"].count(operation["old_text"]) != 1:
                            return {"state": "conflict", "receipts": receipts, "partial": changed,
                                    "reason": "replace_text_requires_unique_match", "path": operation["path"]}
                        content = before["content"].replace(operation["old_text"], operation["new_text"], 1)
                    else:
                        content = operation["content"]
                    body = content.encode("utf-8")
                    if len(body) > grant["max_file_bytes"]:
                        raise ValueError("result file exceeds grant byte limit")
                    record.update(path=operation["path"], before_sha256=before["sha256"], after_sha256=_digest(body),
                                  changed=before["sha256"] != _digest(body))
                    prepared.append((operation, record, body))
                else:
                    record.update(command_id=operation["command_id"], command_sha256=_digest(_json(grant["commands"][operation["command_id"]]).encode()))
                    prepared.append((operation, record, None))
            # Only after every path, baseline and result size passes preflight
            # may a new batch produce filesystem or command side effects.
            for operation, record, body in prepared:
                self._save(record, on_progress)
                try:
                    self._gate(cancel)
                    if body is not None:
                        self._write(root, operation, record, body, grant, cancel, on_progress)
                    else:
                        record.update(state="applying", started_at=now_ms())
                        self._save(record, on_progress)
                        self._gate(cancel)
                        record.update(self._command(root, grant["commands"][operation["command_id"]], cancel))
                    record["completed_at"] = now_ms()
                    self._save(record, on_progress)
                except _Stopped:
                    record.update(state="cancelled", reason="cancelled_before_operation", completed_at=now_ms())
                    self._save(record, on_progress)
                except ValueError as error:
                    record.update(state="failed" if record["state"] == "prepared" else "unknown",
                                  reason=getattr(error, "code", "operator_validation_failed"),
                                  completed_at=now_ms())
                    self._save(record, on_progress)
                except OSError:
                    record.update(state="failed" if record["state"] == "prepared" else "unknown",
                                  reason="operator_io_failed", completed_at=now_ms())
                    self._save(record, on_progress)
                except Exception:
                    record.update(state="unknown", reason="operator_unexpected_failure", completed_at=now_ms())
                    self._save(record, on_progress)
                receipt = self._public(record)
                receipts.append(receipt)
                if record["state"] != "applied":
                    possible_effect = record["state"] == "unknown" or (record["op"] == "run_command" and "started_at" in record)
                    return {"state": record["state"], "receipts": receipts, "partial": changed or possible_effect}
                changed |= record.get("changed", record["op"] == "run_command")
        return {"state": "applied", "receipts": receipts, "partial": False}

    def _write(self, root, operation, record, body, grant, cancel, on_progress):
        path = _target(root, operation["path"])
        if _read(path, grant["max_file_bytes"])["sha256"] != record["before_sha256"]:
            record.update(state="conflict", reason="operator_base_hash_mismatch")
            return
        if not record["changed"]:
            record.update(state="applied", started_at=now_ms())
            return
        self._gate(cancel)
        record.update(state="applying", started_at=now_ms())
        self._save(record, on_progress)
        self._gate(cancel)
        path.parent.mkdir(parents=True, exist_ok=True)
        _target(root, operation["path"])
        descriptor, temporary_name = tempfile.mkstemp(prefix=".gtt-operator-", suffix=".tmp", dir=path.parent)
        temporary = Path(temporary_name)
        try:
            if path.exists():
                os.chmod(temporary, stat.S_IMODE(path.stat().st_mode))
            with os.fdopen(descriptor, "wb") as stream:
                descriptor = None
                stream.write(body)
                stream.flush()
                os.fsync(stream.fileno())
            # The global dispatch switch and the per-assignment cancellation
            # are rechecked at the actual atomic replacement boundary.
            self._gate(cancel)
            _target(root, operation["path"])
            if _read(path, grant["max_file_bytes"])["sha256"] != record["before_sha256"]:
                record.update(state="conflict", reason="operator_base_hash_mismatch")
                return
            os.replace(temporary, path)
            record["state"] = "applied"
        finally:
            if descriptor is not None:
                os.close(descriptor)
            if temporary.exists():
                temporary.unlink()

    @staticmethod
    def _command(root, command, cancel):
        if cancel.is_set():
            raise _Stopped("cancelled")
        cwd = _target(root, command["cwd"], directory=True)
        process = subprocess.Popen(owned_process_command(command["argv"]), cwd=cwd, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, shell=False, bufsize=0,
            start_new_session=os.name != "nt", **background_options())
        output = {name: {"sha": hashlib.sha256(), "excerpt": bytearray(), "bytes": 0, "lock": threading.Lock()}
                  for name in ("stdout", "stderr")}
        def read(name, pipe):
            target = output[name]
            while True:
                try:
                    block = os.read(pipe.fileno(), 65536)
                except (OSError, ValueError):
                    break
                if not block:
                    break
                with target["lock"]:
                    target["sha"].update(block)
                    target["bytes"] += len(block)
                    target["excerpt"].extend(block[:max(0, 4096 - len(target["excerpt"]))])
        threads = [threading.Thread(target=read, args=(name, getattr(process, name)), daemon=True)
                   for name in output]
        for thread in threads:
            thread.start()
        end = time.monotonic() + command["timeout_s"]
        reason = None
        try:
            while process.poll() is None:
                if cancel.is_set() or time.monotonic() >= end:
                    reason = "command_cancelled_outcome_unknown" if cancel.is_set() else "command_timeout_outcome_unknown"
                    break
                cancel.wait(.05)
        finally:
            # Windows' owned helper has kill-on-close Job ownership. Unix uses
            # this invocation's fresh process group, never a name-wide kill.
            if os.name == "nt":
                if process.poll() is None:
                    process.kill()
            else:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            process.wait(timeout=5)
            for thread in threads:
                thread.join(timeout=1)
            for pipe in (process.stdout, process.stderr):
                pipe.close()
        result = {"state": "unknown" if reason else "applied" if process.returncode == 0 else "failed",
                  "exit_code": process.returncode, "changed": True}
        if reason:
            result["reason"] = reason
        if any(t.is_alive() for t in threads):
            result.update(state="unknown", reason="command_output_incomplete")
        for name, values in output.items():
            with values["lock"]:
                result[name] = {"excerpt": redact(values["excerpt"].decode("utf-8", "replace")),
                                "sha256": values["sha"].hexdigest(), "bytes": values["bytes"],
                                "excerpt_only": values["bytes"] > len(values["excerpt"])}
        return result


class _Stopped(Exception):
    pass
