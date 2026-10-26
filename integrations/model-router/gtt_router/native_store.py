"""SQLite compatibility for the Rust-owned store; no Python write connection.

Hot paths use typed native transactions. This adapter preserves existing SQL
callers during the gradual migration, including commit/rollback and Row access.
"""
import copy
import os
from pathlib import Path


class StoreOwner:
    def __init__(self, path):
        path = Path(path).resolve().with_suffix(".owner.lock")
        self.file = path.open("a+b")
        try:
            if os.name == "nt":
                import msvcrt
                self.file.seek(0)
                msvcrt.locking(self.file.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(self.file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            self.file.close()
            raise ValueError("state_owner_busy") from None

    def close(self):
        self.file.close()


def patch(old, new, path=()):
    """Typed JSON changes; array append/remove avoids resending old reports."""
    if type(old) is not type(new):
        return [{"op": "set", "path": list(path), "value": new}]
    if isinstance(new, dict):
        result = [{"op": "remove", "path": [*path, key]} for key in old if key not in new]
        for key, value in new.items():
            result.extend(patch(old[key], value, (*path, key)) if key in old else [{"op": "set", "path": [*path, key], "value": value}])
        return result
    if isinstance(new, list):
        result = []
        for index in range(min(len(old), len(new))):
            result.extend(patch(old[index], new[index], (*path, index)))
        result.extend({"op": "remove", "path": [*path, i]} for i in range(len(old) - 1, len(new) - 1, -1))
        result.extend({"op": "set", "path": [*path, i], "value": new[i]} for i in range(len(old), len(new)))
        return result
    return [] if old == new else [{"op": "set", "path": list(path), "value": new}]


class Row:
    def __init__(self, columns, values):
        self.columns, self.values = columns, values

    def keys(self):
        return self.columns

    def __getitem__(self, key):
        return self.values[self.columns.index(key) if isinstance(key, str) else key]

    def __iter__(self):
        return iter(self.values)

    def __len__(self):
        return len(self.values)


class Cursor:
    def __init__(self, result):
        self.rows = [Row(result["columns"], [bytes(v["$blob"]) if isinstance(v, dict) and "$blob" in v else v for v in row]) for row in result["rows"]]
        self.index = 0
        self.rowcount = result["rowcount"]
        self.lastrowid = result["lastrowid"]

    def fetchone(self):
        if self.index == len(self.rows):
            return None
        row = self.rows[self.index]
        self.index += 1
        return row

    def fetchall(self):
        rows = self.rows[self.index:]
        self.index = len(self.rows)
        return rows

    def __iter__(self):
        return iter(self.fetchall())


class Connection:
    def __init__(self, kernel, path, lock):
        self.kernel, self.lock = kernel, lock
        self.closed = False
        self.in_transaction = False
        kernel.store_lock = lock
        kernel.call("db_open", {"path": str(Path(path).resolve())})
        kernel.persistent = True

    def execute(self, sql, params=()):
        encoded = [{"$blob": list(v)} if isinstance(v, bytes) else v for v in params]
        with self.lock:
            result = self.kernel.call("db_exec", {"sql": sql, "params": encoded})
            if result.get("runtime_changed"):
                self.kernel.invalidate_views()
            self.in_transaction = result["in_transaction"]
            return Cursor(result)

    def executescript(self, sql):
        with self.lock:
            self.kernel.call("db_script", {"sql": sql})
            self.kernel.invalidate_views()
            self.in_transaction = False

    def commit(self):
        with self.lock:
            try:
                self.kernel.call("db_commit", {})
            except Exception:
                self.kernel.invalidate_views()
                raise
            finally:
                self.in_transaction = False

    def rollback(self):
        with self.lock:
            self.kernel.call("db_rollback", {})
            self.kernel.invalidate_views()
            self.in_transaction = False

    def close(self):
        with self.lock:
            if not self.closed and not self.kernel.broken and self.kernel.process.poll() is None:
                self.kernel.call("db_close", {})
            self.closed = True
            self.kernel.persistent = False
            self.kernel.invalidate_views()

    def __enter__(self):
        self.lock.acquire()
        return self

    def __exit__(self, kind, value, trace):
        try:
            if kind is None:
                self.commit()
            elif not self.kernel.broken and self.kernel.process.poll() is None:
                self.rollback()
        finally:
            self.lock.release()
        return False


class RuntimeRecord(dict):
    """Revision/base are private metadata, never part of persisted/public JSON."""
    def __init__(self, value, revision, base):
        super().__init__(value)
        self.revision = revision
        self.base = base

    def __deepcopy__(self, memo):
        return RuntimeRecord(copy.deepcopy(dict(self), memo), self.revision, copy.deepcopy(self.base, memo))
