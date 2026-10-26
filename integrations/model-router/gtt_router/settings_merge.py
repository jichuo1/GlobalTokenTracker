"""Bounded configuration ancestry and conflict-preserving three-way saves."""
import copy
import hashlib
import json
import os
from pathlib import Path
import uuid

MISSING = object()
FILE_NAME = "scheduling-history.json"


def encoded(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)


def digest(value):
    return hashlib.sha256(encoded(value).encode()).hexdigest()


def clone(value):
    return MISSING if value is MISSING else copy.deepcopy(value)


def merge_settings(base, local, remote):
    conflicts = []
    def merge(before, ours, theirs, path):
        if ours == before:
            return clone(theirs)
        if theirs == before or ours == theirs:
            return clone(ours)
        if all(isinstance(v, dict) for v in (before, ours, theirs)):
            result = {}
            for key in dict.fromkeys([*theirs, *ours, *before]):
                value = merge(before.get(key, MISSING), ours.get(key, MISSING), theirs.get(key, MISSING), [*path, key])
                if value is not MISSING:
                    result[key] = value
            return result
        if all(isinstance(v, list) for v in (before, ours, theirs)) and path and path[-1] in {"pool", "workflows", "nodes", "edges"}:
            field = "route_id" if path[-1] == "pool" else "edge" if path[-1] == "edges" else "id"
            def identity(item):
                if not isinstance(item, dict):
                    return None
                return (item.get("from"), item.get("to"), item.get("kind", "task")) if field == "edge" else item.get(field)
            keys = [[identity(item) for item in values] for values in (before, ours, theirs)]
            if all(None not in values and len(set(values)) == len(values) for values in keys):
                bkeys, okeys, tkeys = keys
                if okeys != bkeys and tkeys != bkeys and okeys != tkeys:
                    conflicts.append(".".join(path) + ".order")
                    return clone(theirs)
                maps = [dict(zip(k, values)) for k, values in zip(keys, (before, ours, theirs))]
                order = list(dict.fromkeys([*(okeys if okeys != bkeys else tkeys), *tkeys, *bkeys]))
                result = []
                for key in order:
                    value = merge(*(mapping.get(key, MISSING) for mapping in maps), [*path, str(key)])
                    if value is not MISSING:
                        result.append(value)
                return result
        conflicts.append(".".join(path) or "settings")
        return clone(theirs)
    clean = lambda value: {k: v for k, v in value.items() if k != "revision"}
    result = merge(clean(base), clean(local), clean(remote), [])
    if conflicts:
        raise ValueError("scheduling_revision_conflict: conflicting fields: " + ", ".join(conflicts[:8]))
    result["revision"] = remote["revision"]
    return result


def read_history(path):
    path = Path(path)
    try:
        if path.stat().st_size > 16 * 1024 * 1024:
            return {}
        data = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(data, dict) or data.get("schema_version") != 1 or not isinstance(data.get("snapshots"), list) or len(data["snapshots"]) > 8:
            return {}
        values = {}
        for item in data["snapshots"]:
            value = item["value"]
            if (type(item["revision"]) is not int or item["revision"] < 0 or not isinstance(value, dict)
                    or value.get("revision") != item["revision"] or digest(value) != item["sha256"]):
                return {}
            values[item["revision"]] = value
        return values
    except (OSError, ValueError, KeyError, TypeError):
        return {}


def history_bytes(values):
    kept = sorted(values)[-8:]
    return encoded({"schema_version": 1, "snapshots": [{"revision": key, "value": values[key], "sha256": digest(values[key])} for key in kept]}).encode()


def remember(path, value):
    path = Path(path)
    values = read_history(path)
    values[value["revision"]] = copy.deepcopy(value)
    body = history_bytes(values)
    temporary = path.with_name(path.name + "." + uuid.uuid4().hex + ".tmp")
    try:
        with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as stream:
            stream.write(body)
        os.replace(temporary, path)
    finally:
        if temporary.exists():
            temporary.unlink()
