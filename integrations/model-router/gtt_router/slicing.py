"""Deterministic source ranges and bounded feedback-driven work queues."""

import hashlib
import math
import json

from .models import LOGICAL_KINDS, MAX_OUTPUT_TOKENS


def requested_output(task, prompt_chars=None):
    base = task.max_output_tokens or (32768 if task.kind in LOGICAL_KINDS else 8192)
    if task.output_mode == "fixed":
        return base
    size = len(task.prompt) + len(task.context) if prompt_chars is None else prompt_chars
    # Small jobs retain established defaults. Bigger excerpts reserve reasoning
    # headroom without assuming that all native output capacity will be used.
    scale = max(1, math.sqrt(max(1, size) / 8000))
    cap = getattr(task, "_runtime_limits", {}).get("max_output_tokens", MAX_OUTPUT_TOKENS)
    return min(cap, max(getattr(task, "_output_target", 0), math.ceil(base * scale / 8192) * 8192))


def output_budget(task, route, prompt_chars=None):
    requested = requested_output(task, prompt_chars)
    effective = route.output_ceiling(None if task.output_mode == "native" else requested)
    if effective is None:
        effective = requested  # unknown capacity is not unlimited capacity
    effective = min(effective, getattr(task, "_runtime_limits", {}).get("max_output_tokens", MAX_OUTPUT_TOKENS))
    if route.context_window:
        size = len(task.prompt) + len(task.context) if prompt_chars is None else prompt_chars
        effective = min(effective, route.context_window - math.ceil(size / 2))
    return {"requested_output_tokens": requested, "effective_output_tokens": effective,
            "output_mode": task.output_mode, "native_output_limit": route.output_limit,
            "minimum_output_tokens": getattr(task, "_output_floor", 256)}


def digest(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def boundary(text, start, size, end=None, min_size=1):
    end = len(text) if end is None else end
    stop = min(end, start + size)
    if stop < end:
        # Preserve paragraphs/lines when feasible. Ranges use Python character
        # offsets, never UTF-8 byte offsets; no character is removed or duplicated.
        floor = start + max(min_size, (stop - start) // 2)
        for separator in ("\n\n", "\n"):
            found = text.rfind(separator, floor, stop)
            if found >= floor:
                return found + len(separator)
    return stop


class SliceQueue:
    def __init__(self, request, templates, state=None):
        self.request = request
        self.text = request.context
        self.state = state if state is not None else {
            "source_hash": digest(self.text), "cursor": 0,
            "target_chars": request.slice_target_chars, "success_streak": 0,
            "records": [], "templates": templates,
            "template_count": len(templates),
            "partition_context": len(self.text) > request.slice_target_chars and
                not any("context_range" in item or item.get("evidence_workers") or item.get("information_request_ids") for item in templates),
            "template_cursor": 0,
            "template_offset": None,
        }
        if self.state["source_hash"] != digest(self.text):
            raise ValueError("Slice source revision changed")

    def _record(self, start, end, template, parent=None):
        slot = len(self.state["records"]) + 1
        template_hash = digest(json.dumps(self.state["templates"][template], ensure_ascii=False, sort_keys=True))
        record = {"slot": slot, "id": digest(f"{self.state['source_hash']}:{start}:{end}:{template_hash}:{parent}:{slot}")[:20],
                  "start": start, "end": end, "content_hash": digest(self.text[start:end]),
                  "template": template, "template_hash": template_hash, "parent": parent, "state": "pending"}
        item = self.state["templates"][template]
        if item.get("information_request_ids"):
            first, last = item.get("context_range", [0, 0 if item.get("evidence_workers") or item.get("information_request_ids") else len(self.text)])
            record.update({"support_id": digest(f"{self.state['source_hash']}:{template_hash}:{template}")[:20],
                           "support_start": first, "support_end": last})
        self.state["records"].append(record)
        return record

    def take(self, capacity_chars=None):
        for record in self.state["records"]:
            if record["state"] == "pending":
                return record
        if len(self.state["records"]) >= self.request.max_slices:
            return None
        templates = self.state["templates"]
        if self.state["partition_context"]:
            start = self.state["cursor"]
            if start >= len(self.text):
                return None
            size = self.state["target_chars"]
            if capacity_chars is not None:
                size = max(self.request.slice_min_chars, min(size, capacity_chars))
            stop = boundary(self.text, start, size, min_size=self.request.slice_min_chars)
            template = (len(self.state["records"])) % self.state["template_count"]
            self.state["cursor"] = stop
        else:
            template = self.state["template_cursor"]
            if template >= self.state["template_count"]:
                return None
            start, stop = templates[template].get("context_range", [0, 0 if templates[template].get("evidence_workers") or templates[template].get("information_request_ids") else len(self.text)])
            if not 0 <= start <= stop <= len(self.text):
                raise ValueError("Task context_range is outside supplied material")
            start = self.state.get("template_offset") if self.state.get("template_offset") is not None else start
            size = min(self.state["target_chars"], self.request.slice_max_chars)
            if capacity_chars is not None:
                size = max(self.request.slice_min_chars, min(size, capacity_chars))
            part_end = boundary(self.text, start, size, stop, self.request.slice_min_chars)
            if part_end < stop:
                self.state["template_offset"] = part_end
            else:
                self.state["template_offset"] = None
                self.state["template_cursor"] += 1
            stop = part_end
        return self._record(start, stop, template)

    def has_work(self):
        if any(r["state"] == "pending" for r in self.state["records"]):
            return True
        if len(self.state["records"]) >= self.request.max_slices:
            return False
        return self.state["cursor"] < len(self.text) if self.state["partition_context"] else self.state["template_cursor"] < self.state["template_count"]

    def finish(self, record, result):
        if result and result.get("ok"):
            record["state"] = "succeeded"
            record["worker"] = result["worker"]
            expected_ms = 180000 * max(1, (record["end"] - record["start"]) / 8000) ** .5
            if result.get("duration_ms", 0) > expected_ms:
                self.state["target_chars"] = max(self.request.slice_min_chars, self.state["target_chars"] // 2)
                self.state["success_streak"] = 0
                return
            self.state["success_streak"] += 1
            if self.state["success_streak"] >= 3:
                self.state["target_chars"] = min(self.request.slice_max_chars, math.ceil(self.state["target_chars"] * 1.25))
                self.state["success_streak"] = 0
            return
        record["state"] = "failed"
        code = (result or {}).get("error", {}).get("code", "missing_result")
        record["error"] = code
        if code == "timeout" or result.get("unknown_paid_outcome") is True:
            record["state"] = "unknown"  # paid native outcome may be unavailable
            return
        details = (result or {}).get("error", {}).get("details", {})
        incomplete_json = code == "incomplete_output" and details.get("contract_reason") == "incomplete_json" and details.get("report_chars", 0) > 0
        if code not in {"output_truncated", "context_exceeded"} and not incomplete_json:
            return
        if code == "output_truncated" and (result or {}).get("error", {}).get("details", {}).get("visible_output_chars") == 0:
            return  # a reasoning-only exhaustion is not evidence that source boundaries need splitting
        self.state["success_streak"] = 0
        self.state["target_chars"] = max(self.request.slice_min_chars, self.state["target_chars"] // 2)
        size = record["end"] - record["start"]
        if size < self.request.slice_min_chars * 2 or len(self.state["records"]) + 2 > self.request.max_slices:
            return
        middle = boundary(self.text, record["start"], size // 2, record["end"], self.request.slice_min_chars)
        record["state"] = "split"
        children = [self._record(record["start"], middle, record["template"], record["id"]),
                    self._record(middle, record["end"], record["template"], record["id"])]
        if "support_depth" in record:
            for child in children:
                child["support_depth"] = record["support_depth"]

    def support(self, record, result):
        """Only already-authorized local ranges/reports may be attached early.

        Unscoped questions/web collection remain a leader decision. Two support
        hops plus max_slices prevent cyclic requests from growing indefinitely.
        """
        if not result or not result.get("ok") or record.get("support_depth", 0) >= 2:
            return
        for request in result.get("information_requests", []):
            if "context_range" not in request and not request.get("report_workers"):
                continue
            if len(self.state["records"]) >= self.request.max_slices:
                break
            start, end = request.get("context_range", [0, 0])
            if end - start > self.request.slice_max_chars or [start, end] == [record["start"], record["end"]]:
                continue  # needs a semantic plan, not a replay of the same input
            template = dict(self.state["templates"][record["template"]])
            template.update({"prompt": "Resolve this missing evidence: " + request["question"] + " Reason: " + request["reason"],
                             "context_range": [start, end],
                             "information_request_ids": [request.get("request_id", f"{result['worker']}:{result.get('information_requests', []).index(request)}")],
                             "evidence_workers": list(dict.fromkeys([result["worker"], *request.get("report_workers", [])]))[:4]})
            index = len(self.state["templates"])
            self.state["templates"].append(template)
            added = self._record(start, end, index, record["id"])
            added["support_depth"] = record.get("support_depth", 0) + 1

    def incomplete(self):
        remaining = self.state["cursor"] < len(self.text) if self.state["partition_context"] else self.state["template_cursor"] < self.state["template_count"]
        return remaining or any(r["state"] not in {"succeeded", "split"} for r in self.state["records"])

    def view(self):
        return {"source_hash": self.state["source_hash"], "target_chars": self.state["target_chars"],
                "assigned_chars": self.state["cursor"], "source_chars": len(self.text),
                "incomplete": self.incomplete(),
                "counts": {s: sum(r["state"] == s for r in self.state["records"])
                           for s in ("pending", "running", "succeeded", "split", "failed", "unknown")}}
