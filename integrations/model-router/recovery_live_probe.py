"""Simulated demotion -> real tiny generation recovery, in an isolated state DB."""

import json
from pathlib import Path

from gtt_router.catalog import Catalog
from gtt_router.daemon import load_config
from gtt_router.models import now_ms
from gtt_router.service import RouterService


root = Path.home() / ".globaltokentracker" / "router"
config = load_config(root / "config.json")
config["state_dir"] = str(root / "diagnostics" / "recovery-live")
config["recovery"] = {"enabled": False, "probe_timeout_s": 45}
config.pop("_config_path", None)
catalog = Catalog(config)
catalog.refresh()
route = next(r for r in catalog.routes if r.adapter == "zcode_rpc" and r.model == "inclusionai/ling-3.1-flash")
catalog.routes = [route]
service = RouterService(config, catalog)
try:
    key = "route:" + route.id
    service.store.failure(key, "timeout", 0)
    with service.store.lock:
        service.store.conn.execute("UPDATE health SET next_probe_at=? WHERE key=?", (now_ms() - 1, key))
        service.store.conn.commit()
    before = service.store.availability(key)
    result = service.recovery.tick()
    after = service.store.availability(key)
    report = {"simulated_failure": True, "production_scores_changed": False, "model": route.model,
              "before": before, "probe": result, "after": after,
              "ok": result.get("success") is True and after["weight"] == 1}
    (root / "recovery-validation.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report))
finally:
    service.shutdown()
