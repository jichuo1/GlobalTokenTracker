"""Bounded, deterministic JSON envelopes; never repair incomplete evidence."""

import json
import math
import re
from .models import DEFAULT_REPORT_CHARS, MAX_REPORT_CHARS as HARD_REPORT_CHARS
from .profiles import ABS_LIMITS
HARD_REPORT_CHARS = ABS_LIMITS["max_report_chars"]

MAX_REPORT_CHARS = DEFAULT_REPORT_CHARS
MAX_ENVELOPE_CHARS = 1000


class ReportContractError(ValueError):
    def __init__(self, reason, offset=None):
        super().__init__(reason)
        self.reason = reason
        self.offset = offset


def _pairs(items):
    result = {}
    for key, value in items:
        if key in result:
            raise ReportContractError("duplicate_key")
        result[key] = value
    return result


def _constant(_):
    raise ReportContractError("nonfinite_number")


def _float(value):
    result = float(value)
    if not math.isfinite(result):
        raise ReportContractError("nonfinite_number")
    return result


def _outside(prefix, suffix):
    outside = prefix + suffix
    if len(outside) > MAX_ENVELOPE_CHARS or any(mark in outside for mark in ("{", "[", "```")):
        raise ReportContractError("ambiguous_envelope")


def parse_report(text, max_chars=MAX_REPORT_CHARS):
    if not isinstance(max_chars, int) or isinstance(max_chars, bool) or not 1000 <= max_chars <= HARD_REPORT_CHARS:
        raise ReportContractError("report_too_large")
    if not isinstance(text, str):
        raise ReportContractError("non_text_report")
    if len(text) > max_chars:
        raise ReportContractError("report_too_large")
    text = text.strip().lstrip("\ufeff").strip()
    shape = "json"
    fenced = list(re.finditer(r"^```(?:json)?[ \t]*\r?\n(.*?)^```[ \t]*(?:\r?\n|$)", text,
                             re.MULTILINE | re.DOTALL | re.IGNORECASE))
    if fenced:
        if len(fenced) != 1:
            raise ReportContractError("multiple_documents")
        match = fenced[0]
        _outside(text[:match.start()], text[match.end():])
        text = match.group(1).strip()
        shape = "json_fence"
    elif not text.startswith("{"):
        start = text.find("{")
        if start < 0:
            raise ReportContractError("missing_json_object")
        _outside(text[:start], "")
        text = text[start:]
        shape = "prose_envelope"
    decoder = json.JSONDecoder(object_pairs_hook=_pairs, parse_constant=_constant, parse_float=_float)
    try:
        body, end = decoder.raw_decode(text)
    except ReportContractError:
        raise
    except json.JSONDecodeError as error:
        incomplete = error.pos >= len(text.rstrip()) or error.msg.startswith("Unterminated string")
        raise ReportContractError("incomplete_json" if incomplete else "invalid_json", error.pos) from None
    except RecursionError:
        raise ReportContractError("excessive_nesting") from None
    except ValueError:
        raise ReportContractError("invalid_json") from None
    if not isinstance(body, dict):
        raise ReportContractError("unexpected_root")
    suffix = text[end:].strip()
    if suffix:
        _outside("", suffix)
        if fenced:
            raise ReportContractError("multiple_documents")
        try:
            decoder.raw_decode(suffix)
        except json.JSONDecodeError:
            pass
        else:
            raise ReportContractError("multiple_documents")
        shape = "prose_envelope"
    try:
        json.dumps(body, ensure_ascii=False).encode("utf-8")
    except UnicodeEncodeError:
        raise ReportContractError("invalid_unicode") from None
    return body, shape


def feedback_slot(value, maximum):
    """Only exact ASCII integer/slot spellings; role names are ambiguous."""
    if isinstance(value, str):
        match = re.fullmatch(r"(?:slot[ \t]+)?(0|[1-9][0-9]{0,2})", value.strip())
        if not match:
            raise ValueError("feedback worker must identify an integer slot")
        value = int(match.group(1))
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not 0 <= value <= maximum or int(value) != value:
        raise ValueError("feedback worker must identify an integer slot")
    return int(value)
