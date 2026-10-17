"""Bounded activity deadlines; no content, credentials or model claims are stored."""

from collections import OrderedDict
import hashlib
import json
import math
import threading
import time


class ActivityWatch:
    """Extend a soft expectation only while new, observable work is arriving.

    A live process, busy flag, repeated timestamp or empty frame is not work.
    Hard mode retains the historical absolute deadline. Activity mode remains
    bounded by its absolute horizon even if the model streams continuously.
    """

    def __init__(self, deadline, *, mode="hard", hard_deadline=None, ttft_s=None,
                 idle_s=None, stall_grace_s=15, clock=None):
        self.clock = clock or time.monotonic
        self.started = self.clock()
        self.lock = threading.RLock()
        if mode not in {"hard", "activity"}:
            raise ValueError("timeout mode must be hard or activity")
        values = [deadline, hard_deadline if hard_deadline is not None else deadline,
                  stall_grace_s, *[v for v in (ttft_s, idle_s) if v is not None]]
        if any(isinstance(v, bool) or not isinstance(v, (float, int)) or not math.isfinite(v) for v in values):
            raise ValueError("Activity deadlines must be finite numbers")
        if stall_grace_s < 0 or any(v is not None and v <= 0 for v in (ttft_s, idle_s)):
            raise ValueError("Activity windows must be positive; grace may be zero")
        self.mode = mode
        self.soft_deadline = deadline
        # The ordinary default is finite; group callers always supply their own
        # hard boundary. A larger caller horizon never changes a hard deadline.
        self.hard_deadline = deadline if mode == "hard" else (
            hard_deadline if hard_deadline is not None else self.started + 3600)
        window = max(.001, deadline - self.started)
        self.ttft_s = ttft_s if ttft_s is not None else window
        self.idle_s = idle_s if idle_s is not None else min(180, window)
        self.stall_grace_s = stall_grace_s
        self.last_activity = None
        self.response_observed = False
        self.activity_count = 0
        self.output_chars = 0
        self.reasoning_chars = 0
        self.last_kind = None
        self.seen = OrderedDict()
        self.lengths = OrderedDict()
        self.versions = {}

    def _expires_at(self):
        if self.mode == "hard":
            return self.hard_deadline
        first = min(self.soft_deadline, self.started + self.ttft_s)
        if self.last_activity is None:
            return min(self.hard_deadline, first)
        idle = self.last_activity + self.idle_s + self.stall_grace_s
        return min(self.hard_deadline, idle if self.response_observed else max(first, idle))

    def expiry_reason(self):
        with self.lock:
            current = self.clock()
            if current >= self.hard_deadline:
                return "hard_deadline"
            if current >= self._expires_at():
                return "inactivity_timeout" if self.response_observed else "first_activity_timeout"
            return None

    @staticmethod
    def _digest(value):
        return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True,
                                        default=str).encode("utf-8")).hexdigest()

    def _new(self, identity):
        if identity in self.seen:
            return False
        # Stop admitting unbounded new identities rather than evicting hashes
        # and allowing an alternating replay to look like fresh progress.
        if len(self.seen) >= 8192:
            return False
        self.seen[identity] = True
        return True

    def record(self, kind, *, key="", version=None, amount=0):
        if kind not in {"output", "reasoning", "tool_start", "tool_complete", "tool_result", "native_phase"}:
            return False
        if kind in {"output", "reasoning"} and (not isinstance(amount, int) or isinstance(amount, bool) or amount <= 0):
            return False
        if kind not in {"output", "reasoning"} and version is None:
            return False
        with self.lock:
            # Late observations cannot resurrect a cancelled/stalled request.
            if self.expiry_reason():
                return False
            if kind in {"output", "reasoning"} and isinstance(version, (int, float)) and not isinstance(version, bool) and math.isfinite(version):
                stream_key = (kind, self._digest(key))
                if stream_key not in self.versions and len(self.versions) >= 1024:
                    return False
                if version <= self.versions.get(stream_key, -math.inf):
                    return False
                self.versions[stream_key] = version
            else:
                identity = self._digest([kind, key, version, amount])
                if not self._new(identity):
                    return False
            self.last_activity = self.clock()
            if kind != "native_phase":
                self.response_observed = True
            self.last_kind = kind
            self.activity_count += 1
            if kind == "output":
                self.output_chars += amount
            if kind == "reasoning":
                self.reasoning_chars += amount
            return True

    def content(self, kind, key, text, *, delta=False, version=None):
        if not isinstance(text, str) or not text:
            return False
        if delta:
            # Protocol sequence/offset is preferred; identical unsequenced
            # frames are conservative duplicates rather than infinite leases.
            return self.record(kind, key=key, version=version if version is not None else self._digest(text), amount=len(text))
        with self.lock:
            stream_key = (kind, self._digest(key))
            if stream_key not in self.lengths and len(self.lengths) >= 1024:
                return False
            previous = self.lengths.get(stream_key, 0)
            if len(text) <= previous:
                return False
            self.lengths[stream_key] = len(text)
            return self.record(kind, key=key, version=len(text), amount=len(text) - previous)

    def snapshot(self):
        with self.lock:
            current = self.clock()
            return {"timeout_mode": self.mode, "activity_count": self.activity_count,
                    "last_kind": self.last_kind, "output_chars": self.output_chars,
                    "reasoning_chars": self.reasoning_chars,
                    "elapsed_s": round(max(0, current - self.started), 2),
                    "last_activity_elapsed_s": None if self.last_activity is None else round(max(0, self.last_activity - self.started), 2),
                    "idle_s": None if self.last_activity is None else round(max(0, current - self.last_activity), 2),
                    "soft_remaining_s": round(max(0, self.soft_deadline - current), 2),
                    "hard_remaining_s": round(max(0, self.hard_deadline - current), 2),
                    "expires_in_s": round(max(0, self._expires_at() - current), 2),
                    "timeout_reason": self.expiry_reason()}
