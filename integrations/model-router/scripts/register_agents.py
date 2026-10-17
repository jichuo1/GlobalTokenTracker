"""Register installed native agents; dry-run by default. No credential values are copied."""
import argparse
import json
from pathlib import Path
import shutil
import sys
from datetime import datetime,timezone

ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT))
from gtt_router.daemon import load_config,secure_write
from gtt_router.registry import discover_candidates,merge_registration


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--config",default=str(Path.home()/".globaltokentracker/router/config.json"))
    parser.add_argument("--apply",action="store_true")
    parser.add_argument("--allow-paid-dsh",action="store_true",help="Explicit opt-in for paid DSH profile discovery; existing disabled sources stay disabled")
    args=parser.parse_args()
    path=Path(args.config).expanduser().resolve()
    before=path.read_bytes();config=load_config(path)
    sources,inventory=discover_candidates(config,allow_paid_dsh=args.allow_paid_dsh)
    result=merge_registration(config,sources,inventory)
    result.pop("_config_path",None)
    if args.apply:
        if path.read_bytes()!=before:raise ValueError("Configuration changed during discovery; retry without overwriting")
        backup=Path(config["state_dir"])/"config-backups"/datetime.now(timezone.utc).strftime("registration-%Y%m%d-%H%M%S.json")
        backup.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(path,backup)
        secure_write(path,result)
        load_config(path)
    print(json.dumps({"applied":args.apply,"source_count":len(result["sources"]),"source_registry":inventory,
                      "credentials_copied":False,"global_ide_settings_changed":False},ensure_ascii=False))


if __name__=="__main__":
    main()
