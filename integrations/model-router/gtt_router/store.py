"""Coordinator state is separate from GTT's usage ledger."""

import json
from pathlib import Path
import sqlite3
import threading
import math
import uuid

from .models import now_ms
from .usage import project_attempt

REPEATED_CONNECTIVITY_ERRORS = {"timeout", "provider_request_failed", "adapter_error"}
SOURCE_CONTRACT_ERRORS = {"native_protocol_mismatch", "native_configuration_error", "reasoning_level_required"}
VERIFICATION_ERRORS = {"auth_failed", "quota_exhausted", "model_unavailable", *SOURCE_CONTRACT_ERRORS}


class StateStore:
    def __init__(self, path):
        path = Path(path)
        self.path = path
        path.parent.mkdir(parents=True, exist_ok=True)
        self.lock = threading.RLock()
        self.conn = sqlite3.connect(path, check_same_thread=False)
        self.conn.row_factory = sqlite3.Row
        self.conn.executescript("""
            PRAGMA journal_mode=WAL;
            PRAGMA busy_timeout=3000;
            CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY,state TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,task TEXT NOT NULL,detail TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS health(key TEXT PRIMARY KEY,failures INTEGER NOT NULL DEFAULT 0,successes INTEGER NOT NULL DEFAULT 0,latency_ms REAL,cooldown_until INTEGER NOT NULL DEFAULT 0,last_error TEXT);
            CREATE TABLE IF NOT EXISTS feedback(route_id TEXT NOT NULL,kind TEXT NOT NULL,accepted INTEGER NOT NULL,rejected INTEGER NOT NULL,PRIMARY KEY(route_id,kind));
            CREATE TABLE IF NOT EXISTS feedback_votes(job_id TEXT NOT NULL,worker INTEGER NOT NULL,route_id TEXT NOT NULL,kind TEXT NOT NULL,accepted INTEGER NOT NULL,PRIMARY KEY(job_id,worker));
            CREATE TABLE IF NOT EXISTS recovery_checks(id INTEGER PRIMARY KEY,captured_at INTEGER NOT NULL,route_id TEXT NOT NULL,source TEXT NOT NULL,model TEXT NOT NULL,success INTEGER NOT NULL,code TEXT NOT NULL,tokens INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS usage_records(seq INTEGER PRIMARY KEY AUTOINCREMENT,record_key TEXT NOT NULL UNIQUE,payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS preference_scopes(id TEXT PRIMARY KEY,updated_at INTEGER NOT NULL,expires_at INTEGER NOT NULL,preferences TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS coordination_events(seq INTEGER PRIMARY KEY AUTOINCREMENT,workflow_id TEXT NOT NULL,event_id TEXT NOT NULL,created_at INTEGER NOT NULL,expires_at INTEGER NOT NULL,payload TEXT NOT NULL,UNIQUE(workflow_id,event_id));
            CREATE INDEX IF NOT EXISTS coordination_scope ON coordination_events(workflow_id,seq);
            CREATE INDEX IF NOT EXISTS coordination_recipient ON coordination_events(workflow_id,json_extract(payload,'$.to_group'),json_extract(payload,'$.type'),seq);
            CREATE TABLE IF NOT EXISTS coordination_gaps(workflow_id TEXT PRIMARY KEY,dropped INTEGER NOT NULL,updated_at INTEGER NOT NULL);
        """)
        columns = {r[1] for r in self.conn.execute("PRAGMA table_info(health)")}
        if "tokens_ema" not in columns:
            self.conn.execute("ALTER TABLE health ADD COLUMN tokens_ema REAL")
        for name, declaration in {
            "penalty": "REAL NOT NULL DEFAULT 0", "penalty_updated_at": "INTEGER NOT NULL DEFAULT 0",
            "failure_streak": "INTEGER NOT NULL DEFAULT 0", "next_probe_at": "INTEGER NOT NULL DEFAULT 0",
            "probe_failures": "INTEGER NOT NULL DEFAULT 0", "last_probe_at": "INTEGER NOT NULL DEFAULT 0",
            "verification_required": "INTEGER NOT NULL DEFAULT 0", "input_chars_ema": "REAL",
            "last_verified_at": "INTEGER NOT NULL DEFAULT 0", "measured_tokens_ema": "REAL",
            "measured_samples": "INTEGER NOT NULL DEFAULT 0",
            "measured_input_chars_ema": "REAL",
        }.items():
            if name not in columns:
                self.conn.execute("ALTER TABLE health ADD COLUMN " + name + " " + declaration)
        if "verification_required" not in columns:
            self.conn.execute("UPDATE health SET verification_required=1 WHERE last_error IN ('auth_failed','quota_exhausted','model_unavailable')")
        # Upgrade legacy failed routes without replaying any paid request. A
        # decayed score is not evidence that a repeatedly broken route works.
        self.conn.execute("UPDATE health SET verification_required=1 WHERE last_error IN ('native_protocol_mismatch','native_configuration_error') OR (key LIKE 'route:%' AND failure_streak>=3 AND last_error IN ('timeout','provider_request_failed','adapter_error'))")
        # Legacy connectivity failures get a delayed check, never an immediate
        # burst of billable requests when a new server version starts.
        self.conn.execute("UPDATE health SET next_probe_at=? WHERE next_probe_at=0 AND last_error IN ('auth_failed','quota_exhausted','rate_limited','timeout','provider_request_failed','adapter_error','model_unavailable','native_protocol_mismatch','native_configuration_error')", (now_ms() + 1800000,))
        self.conn.execute("UPDATE jobs SET state='interrupted',updated_at=? WHERE state IN ('queued','running','cancelling','needs_codex')", (now_ms(),))
        self.conn.commit()
        # Replay is idempotent and runs only on coordinator startup, never in
        # GTT's reader. No prompts, reports, config or credentials are exported.
        with self.lock, self.conn:
            for row in self.conn.execute("SELECT id,detail FROM jobs").fetchall():
                self._project_job_usage(row["id"], json.loads(row["detail"]))

    def preference_scope(self, scope_id):
        with self.lock:
            row = self.conn.execute("SELECT * FROM preference_scopes WHERE id=? AND expires_at>?", (scope_id, now_ms())).fetchone()
        return {"scope_id": row["id"], "updated_at": row["updated_at"], "expires_at": row["expires_at"],
                "preferences": json.loads(row["preferences"])} if row else None

    def set_preferences(self, values, scope_id="", ttl_s=None):
        from .preferences import parse_preferences, validate_scope
        validate_scope(scope_id)
        if ttl_s is not None and (not isinstance(ttl_s, int) or isinstance(ttl_s, bool) or not 60 <= ttl_s <= 604800):
            raise ValueError("ttl_s must be an integer between 60 and 604800")
        with self.lock, self.conn:
            prior = self.preference_scope(scope_id) if scope_id else None
            if scope_id and prior is None:
                raise ValueError("Unknown or expired preference scope; create a new scope")
            values = parse_preferences(values, prior["preferences"] if prior else None)
            self.conn.execute("DELETE FROM preference_scopes WHERE expires_at<=?", (now_ms(),))
            if not prior and self.conn.execute("SELECT COUNT(*) FROM preference_scopes").fetchone()[0] >= 256:
                raise ValueError("Preference scope capacity reached")
            scope_id = scope_id or uuid.uuid4().hex
            expires = now_ms() + (ttl_s or 21600) * 1000 if ttl_s is not None or not prior else prior["expires_at"]
            self.conn.execute("INSERT INTO preference_scopes VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET updated_at=excluded.updated_at,expires_at=excluded.expires_at,preferences=excluded.preferences",
                              (scope_id, now_ms(), expires, json.dumps(values)))
            return self.preference_scope(scope_id)

    def clear_preferences(self, scope_id):
        from .preferences import validate_scope
        validate_scope(scope_id, required=True)
        with self.lock, self.conn:
            cursor = self.conn.execute("DELETE FROM preference_scopes WHERE id=?", (scope_id,))
            return {"scope_id": scope_id, "cleared": bool(cursor.rowcount)}

    def save_job(self, job_id, state, task, detail):
        with self.lock, self.conn:
            prior = self.conn.execute("SELECT state,detail FROM jobs WHERE id=?", (job_id,)).fetchone()
            if prior and prior["state"] == "merged":
                if state != "merged" or json.loads(prior["detail"]) != detail:
                    raise ValueError("Merged source groups are permanently read-only")
                return
            self._put_job(job_id, state, task, detail)

    def _put_job(self, job_id, state, task, detail):
        self.conn.execute("INSERT INTO jobs VALUES(?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET state=excluded.state,updated_at=excluded.updated_at,detail=excluded.detail",
                          (job_id, state, now_ms(), now_ms(), json.dumps(task.public()), json.dumps(detail, ensure_ascii=False)))
        self._project_job_usage(job_id, detail)

    def _append_event(self, workflow_id, event_id, payload, ttl_s):
        encoded = json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        row = self.conn.execute("SELECT * FROM coordination_events WHERE workflow_id=? AND event_id=?", (workflow_id, event_id)).fetchone()
        if row:
            if row["payload"] != encoded:
                raise ValueError("event_id already identifies different immutable content")
            return {"event_id": event_id, "cursor": row["seq"], "deduplicated": True}
        if self.conn.execute("SELECT COUNT(*) FROM coordination_events WHERE workflow_id=? AND expires_at>?", (workflow_id, now_ms())).fetchone()[0] >= 10000:
            raise ValueError("Workflow coordination event capacity reached")
        self.conn.execute("INSERT INTO coordination_events(workflow_id,event_id,created_at,expires_at,payload) VALUES(?,?,?,?,?)",
                          (workflow_id, event_id, now_ms(), now_ms() + ttl_s * 1000, encoded))
        return {"event_id": event_id, "cursor": self.conn.execute("SELECT last_insert_rowid()").fetchone()[0], "deduplicated": False}

    def append_event(self, workflow_id, event_id, payload, ttl_s=21600):
        with self.lock, self.conn:
            self.conn.execute("DELETE FROM coordination_events WHERE expires_at<=?", (now_ms(),))
            return self._append_event(workflow_id, event_id, payload, ttl_s)

    def events(self, workflow_id, cursor=0, limit=64):
        with self.lock:
            rows = self.conn.execute("SELECT * FROM coordination_events WHERE workflow_id=? AND seq>? AND expires_at>? ORDER BY seq LIMIT ?",
                                     (workflow_id, cursor, now_ms(), limit)).fetchall()
        return [{"cursor": row["seq"], "event_id": row["event_id"], "created_at": row["created_at"],
                 "expires_at": row["expires_at"], **json.loads(row["payload"])} for row in rows]

    def event(self, workflow_id, event_id):
        with self.lock:
            row = self.conn.execute("SELECT * FROM coordination_events WHERE workflow_id=? AND event_id=? AND expires_at>?", (workflow_id, event_id, now_ms())).fetchone()
        if not row:
            raise ValueError("Unknown or expired event in this workflow")
        return {"cursor": row["seq"], "event_id": event_id, **json.loads(row["payload"])}

    def message_events(self, workflow_id, group_id, roles, cursor=0):
        placeholders = ",".join("?" for _ in roles)
        with self.lock:
            rows = self.conn.execute("SELECT * FROM coordination_events WHERE workflow_id=? AND seq>? AND expires_at>? AND json_extract(payload,'$.type')='message' AND json_extract(payload,'$.to_group')=? AND json_extract(payload,'$.to_role') IN (" + placeholders + ") ORDER BY seq LIMIT 64",
                                     (workflow_id, cursor, now_ms(), group_id, *roles)).fetchall()
        return [{"cursor": row["seq"], "event_id": row["event_id"], **json.loads(row["payload"])} for row in rows]

    def workflow_jobs(self, workflow_id):
        with self.lock:
            rows = self.conn.execute("SELECT id FROM jobs WHERE json_extract(task,'$.workflow_id')=? ORDER BY updated_at DESC LIMIT 1025", (workflow_id,)).fetchall()
        return [self.job(row["id"]) for row in rows]

    def event_gap(self, workflow_id, increment=False):
        with self.lock, self.conn:
            if increment:
                self.conn.execute("INSERT INTO coordination_gaps VALUES(?,1,?) ON CONFLICT(workflow_id) DO UPDATE SET dropped=dropped+1,updated_at=excluded.updated_at", (workflow_id, now_ms()))
            row = self.conn.execute("SELECT dropped FROM coordination_gaps WHERE workflow_id=?", (workflow_id,)).fetchone()
            return {"events_incomplete": bool(row), "dropped_state_events": row[0] if row else 0}

    def merge_groups(self, new_job, sources, expected_revisions, event):
        """One transaction; inherited counters never create inherited attempts."""
        with self.lock, self.conn:
            self.conn.execute("BEGIN IMMEDIATE")
            for job in sources:
                prior = self.conn.execute("SELECT state,detail FROM jobs WHERE id=?", (job.id,)).fetchone()
                if not prior or prior["state"] not in {"idle", "needs_codex", "interrupted"}:
                    raise ValueError("Merge source is not quiescent")
                detail = json.loads(prior["detail"])
                if detail["group"].get("merged_into") or detail["group"]["revision"] != expected_revisions[job.id]:
                    raise ValueError("Stale merge approval or already merged source")
                if detail.get("tokens_reserved", 0) or detail["group"].get("coordinating", False) or detail["group"].get("active_calls", 0):
                    raise ValueError("Persisted in-flight/reserved work must be reconciled before merge")
            self._put_job(new_job.id, new_job.state, new_job.task, new_job.detail())
            for job in sources:
                detail = job.detail()
                detail["group"] = {**detail["group"], "merged_into": new_job.id,
                                   "revision": job.revision + 1, "phase": "merged"}
                self._put_job(job.id, "merged", job.task, detail)
            self.conn.execute("DELETE FROM coordination_events WHERE expires_at<=?", (now_ms(),))
            self._append_event(new_job.task.workflow_id, event["event_id"], event["payload"], 604800)

    def _write_usage(self, record):
        payload = json.dumps(record, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        self.conn.execute("INSERT INTO usage_records(record_key,payload) VALUES(?,?) ON CONFLICT(record_key) DO UPDATE SET payload=excluded.payload,seq=(SELECT COALESCE(MAX(seq),0)+1 FROM usage_records) WHERE payload<>excluded.payload",
                          (record["record_key"], payload))

    def _project_job_usage(self, job_id, detail):
        for index, attempt in enumerate(detail.get("attempts", [])):
            record = project_attempt(job_id, index, attempt, detail.get("results", []))
            if record:
                self._write_usage(record)

    def job(self, job_id):
        with self.lock:
            row = self.conn.execute("SELECT * FROM jobs WHERE id=?", (job_id,)).fetchone()
        if row is None:
            raise ValueError("Unknown job ID")
        return {"job_id": row["id"], "state": row["state"], "created_at": row["created_at"], "updated_at": row["updated_at"],
                "task": json.loads(row["task"]), **json.loads(row["detail"])}

    def health(self, key):
        with self.lock:
            row = self.conn.execute("SELECT * FROM health WHERE key=?", (key,)).fetchone()
        return dict(row) if row else {"key": key, "failures": 0, "successes": 0, "latency_ms": None, "cooldown_until": 0, "last_error": None, "tokens_ema": None,
                                     "penalty": 0, "penalty_updated_at": 0, "failure_streak": 0, "next_probe_at": 0, "probe_failures": 0, "last_probe_at": 0,
                                     "verification_required": 0, "input_chars_ema": None, "last_verified_at": 0,
                                     "measured_tokens_ema": None, "measured_samples": 0, "measured_input_chars_ema": None}

    def availability(self, key, half_life_s=1800):
        state = self.health(key)
        age = max(0, now_ms() - state["penalty_updated_at"]) / 1000
        penalty = state["penalty"] * math.exp(-math.log(2) * age / max(1, half_life_s))
        return {"weight": round(max(.25, 1 - penalty), 4), "recent_penalty": round(penalty, 4),
                "failure_streak": state["failure_streak"], "next_probe_at": state["next_probe_at"],
                "last_probe_at": state["last_probe_at"], "last_error": state["last_error"],
                "verification_required": bool(state["verification_required"])}

    def success(self, key, duration_ms, tokens=None, input_chars=None, measured_tokens=None):
        with self.lock:
            prior = self.health(key)
            latency = duration_ms if prior["latency_ms"] is None else .7 * prior["latency_ms"] + .3 * duration_ms
            self.conn.execute("INSERT INTO health(key,successes,latency_ms) VALUES(?,1,?) ON CONFLICT(key) DO UPDATE SET successes=successes+1,latency_ms=excluded.latency_ms,cooldown_until=0,last_error=NULL", (key, latency))
            penalty = self.availability(key)["recent_penalty"] * .35
            self.conn.execute("UPDATE health SET penalty=?,penalty_updated_at=?,failure_streak=0,next_probe_at=0,probe_failures=0,verification_required=0,last_verified_at=? WHERE key=?", (penalty, now_ms(), now_ms(), key))
            if tokens is not None:
                previous = prior.get("tokens_ema")
                value = tokens if previous is None else .7 * previous + .3 * tokens
                self.conn.execute("UPDATE health SET tokens_ema=? WHERE key=?", (value, key))
            if input_chars is not None:
                previous = prior.get("input_chars_ema")
                value = input_chars if previous is None else .7 * previous + .3 * input_chars
                self.conn.execute("UPDATE health SET input_chars_ema=? WHERE key=?", (value, key))
            if measured_tokens is not None:
                previous = prior.get("measured_tokens_ema")
                value = measured_tokens if previous is None else .7 * previous + .3 * measured_tokens
                self.conn.execute("UPDATE health SET measured_tokens_ema=?,measured_samples=measured_samples+1 WHERE key=?", (value, key))
                if input_chars is not None:
                    previous = prior.get("measured_input_chars_ema")
                    value = input_chars if previous is None else .7 * previous + .3 * input_chars
                    self.conn.execute("UPDATE health SET measured_input_chars_ema=? WHERE key=?", (value, key))
            self.conn.commit()

    def failure(self, key, code, cooldown_s, penalty_step=.16, recovery_delay_s=1800):
        with self.lock:
            prior_streak = self.health(key)["failure_streak"]
            penalty = min(.8, self.availability(key)["recent_penalty"] + penalty_step)
            self.conn.execute("INSERT INTO health(key,failures,cooldown_until,last_error) VALUES(?,1,?,?) ON CONFLICT(key) DO UPDATE SET failures=failures+1,cooldown_until=MAX(cooldown_until,excluded.cooldown_until),last_error=excluded.last_error",
                              (key, now_ms() + int(cooldown_s * 1000), code))
            self.conn.execute("UPDATE health SET penalty=?,penalty_updated_at=?,failure_streak=failure_streak+1,next_probe_at=? WHERE key=?",
                              (penalty, now_ms(), now_ms() + int(max(cooldown_s, recovery_delay_s) * 1000), key))
            if code in VERIFICATION_ERRORS or (key.startswith("route:") and code in REPEATED_CONNECTIVITY_ERRORS and prior_streak >= 2):
                self.conn.execute("UPDATE health SET verification_required=1 WHERE key=?", (key,))
            self.conn.commit()

    def recovered(self, keys):
        with self.lock:
            for key in keys:
                self.conn.execute("INSERT INTO health(key,last_verified_at,last_probe_at) VALUES(?,?,?) ON CONFLICT(key) DO UPDATE SET penalty=0,penalty_updated_at=excluded.last_verified_at,failure_streak=0,cooldown_until=0,next_probe_at=0,probe_failures=0,last_probe_at=excluded.last_probe_at,last_error=NULL,verification_required=0,last_verified_at=excluded.last_verified_at", (key, now_ms(), now_ms()))
            self.conn.commit()

    def defer_probe(self, keys, delay_s, failed=False):
        with self.lock:
            for key in keys:
                self.conn.execute("UPDATE health SET next_probe_at=?,last_probe_at=?,probe_failures=probe_failures+? WHERE key=?",
                                  (now_ms() + int(delay_s * 1000), now_ms(), int(failed), key))
            self.conn.commit()

    def record_probe(self, route, success, code, tokens, metering=None):
        with self.lock, self.conn:
            self.conn.execute("INSERT INTO recovery_checks(captured_at,route_id,source,model,success,code,tokens) VALUES(?,?,?,?,?,?,?)",
                              (now_ms(), route.id, route.source, route.model, int(success), code, tokens))
            if metering is not None:
                probe_id = self.conn.execute("SELECT last_insert_rowid()").fetchone()[0]
                record = project_attempt("recovery", probe_id, {"source": route.source, "provider": route.provider,
                    "model": route.model, "started_at": now_ms(), "state": "succeeded" if success else "failed",
                    "tokens_charged": tokens, **metering}, [])
                if record:
                    # Counter IDs can repeat after a state DB is replaced or
                    # another coordinator is connected; ledger IDs must not.
                    record["record_key"] = "probe:" + uuid.uuid4().hex
                    self._write_usage(record)

    def probe_budget(self, since_ms):
        with self.lock:
            row = self.conn.execute("SELECT COUNT(*),COALESCE(SUM(tokens),0) FROM recovery_checks WHERE captured_at>=?", (since_ms,)).fetchone()
        return {"count": row[0], "tokens": row[1]}

    def quality(self, route_id, kind):
        with self.lock:
            row = self.conn.execute("SELECT accepted,rejected FROM feedback WHERE route_id=? AND kind=?", (route_id, kind)).fetchone()
        if row is None:
            return {"samples": 0, "acceptance": None}
        total = row[0] + row[1]
        return {"samples": total, "acceptance": row[0] / total if total else None}

    def record_feedback(self, route_id, kind, accepted, job_id=None, worker=None):
        with self.lock:
            if job_id is not None:
                prior = self.conn.execute("SELECT route_id,kind,accepted FROM feedback_votes WHERE job_id=? AND worker=?", (job_id, worker)).fetchone()
                if prior and bool(prior[2]) == accepted:
                    return self.quality(route_id, kind)
                if prior:
                    self.conn.execute("UPDATE feedback SET accepted=accepted-?,rejected=rejected-? WHERE route_id=? AND kind=?", (int(bool(prior[2])), int(not prior[2]), prior[0], prior[1]))
                self.conn.execute("INSERT INTO feedback_votes VALUES(?,?,?,?,?) ON CONFLICT(job_id,worker) DO UPDATE SET accepted=excluded.accepted", (job_id, worker, route_id, kind, int(accepted)))
            self.conn.execute("INSERT INTO feedback VALUES(?,?,?,?) ON CONFLICT(route_id,kind) DO UPDATE SET accepted=accepted+excluded.accepted,rejected=rejected+excluded.rejected",
                              (route_id, kind, int(accepted), int(not accepted)))
            self.conn.commit()
        return self.quality(route_id, kind)

    def close(self):
        with self.lock:
            self.conn.close()
