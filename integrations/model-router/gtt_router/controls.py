"""Shared local interface switch. This is user control, not process authentication."""

import json
import os
from pathlib import Path

FILE_NAME = "external-interface.json"
BLOCK_CODES = {"external_interface_disabled", "external_interface_settings_invalid", "luna_supervisor_required",
               "scheduling_disabled", "scheduling_settings_invalid", "source_dispatch_disabled", "route_not_selected", "directed_pool_expired", "directed_pool_empty", "workflow_cancelled"}


class DispatchBlocked(ValueError):
    def __init__(self, code, message=None):
        self.code = code
        super().__init__(code + (": " + message if message else ""))


def control_paths(config):
    directory = os.environ.get("GTT_DATA_DIR")
    if directory is not None:
        global_root = Path(directory)
    else:
        current = Path.home() / ".globaltokentracker"
        legacy = Path.home() / ".codeledger"
        global_root = legacy if not (current / "ledger.db").exists() and (legacy / "ledger.db").is_file() else current
    ledger = Path(config["ledger_path"]) if "ledger_path" in config else Path(config["state_dir"]) / "ledger.db"
    return tuple(dict.fromkeys((global_root.resolve() / FILE_NAME, ledger.expanduser().resolve().parent / FILE_NAME)))


def _unique(items):
    result = {}
    for key, value in items:
        if key in result:
            raise DispatchBlocked("external_interface_settings_invalid")
        result[key] = value
    return result


def require_interface(paths):
    for path in paths:
        try:
            with Path(path).open("rb") as stream:
                data = stream.read(4097)
        except FileNotFoundError:
            continue  # Existing installations expose the interface until explicitly disabled.
        except OSError:
            raise DispatchBlocked("external_interface_settings_invalid") from None
        try:
            value = json.loads(data.decode("utf-8-sig"), object_pairs_hook=_unique)
            valid = (len(data) <= 4096 and isinstance(value, dict) and
                     set(value) == {"schema_version", "enabled"} and
                     type(value["schema_version"]) is int and value["schema_version"] == 1 and
                     type(value["enabled"]) is bool)
        except (ValueError, UnicodeError):
            valid = False
        if not valid:
            raise DispatchBlocked("external_interface_settings_invalid")
        if not value["enabled"]:
            raise DispatchBlocked("external_interface_disabled")
