#!/usr/bin/env python3
"""source env.sh 后运行；同精度的同步/后台统计版本，顺序独占网卡并反序复测。"""
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parent.parent
os.chdir(root)
out = Path(sys.argv[2]) if len(sys.argv) > 2 else Path("results/metrics-offload")
out.mkdir(parents=True, exist_ok=True)
duration = int(sys.argv[1]) if len(sys.argv) > 1 else 60
order = [("inline", "a"), ("inline", "b"), ("background", "a"), ("background", "b")]
runs = json.loads((out / "runs.json").read_text()) if (out / "runs.json").exists() else []
done = {r["name"] for r in runs}
for round_no, sequence in enumerate([order, list(reversed(order))], 1):
    for mode, client in sequence:
        name = f"{mode}-{client}-{round_no}"
        if name in done:
            continue
        binary = root / "target/metrics-bench" / mode / ("async-ping" if client == "a" else "raw-ping")
        command = ["sudo", "-n", str(binary), "--bdf", os.environ["BDF"],
                   "--src-ip", os.environ["SRC_IP"], "--core", os.environ["CORE"],
                   "--stats-core", "3", "--peer-ip", os.environ["PEER_IP"],
                   "--peer-mac", os.environ["PEER_MAC"], "--sessions", "64", "--payload", "64",
                   "--delay-us", "500", "--timeout-us", "10000", "--duration-sec", str(duration),
                   "--output", str(out / (name + ".json"))]
        run = dict(name=name, command=command, started_utc=datetime.now(timezone.utc).isoformat())
        print("starting", name, flush=True)
        with (out / (name + ".log")).open("w") as log:
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
        run["finished_utc"] = datetime.now(timezone.utc).isoformat()
        d = json.loads((out / (name + ".json")).read_text())
        c = d["counters"]
        assert d["accounted"] and d["mempool"]["leak_free"] and c["tx"] == c["rx"] + c["timeout"], name
        assert all(c[k] == 0 for k in ("duplicate", "alloc_failed", "tx_failed")), name
        assert all(d["nic"][k] == 0 for k in ("missed", "rx_errors", "tx_errors", "rx_nombuf")), name
        assert all(d["latency"][m]["count"] == c["rx"] for m in ("send", "receive", "process", "end_to_end")), name
        assert d["latency"]["sleep_error"]["count"] == c["tx"], name
        assert d["latency"]["timer"]["count"] == c["tx"] - 64, name
        runs.append(run)
        (out / "runs.json").write_text(json.dumps(runs, indent=2) + "\n")
        p, t = d["latency"]["process"], d["latency"]["timer"]
        print(f"done {name}: process={p['p50']}/{p['p99']}ns timer={t['p50']}/{t['p99']}ns "
              f"rx={c['rx']} timeout={c['timeout']} late={c['late_reply']} "
              f"backpressure={d['statistics']['backpressure_batches']}", flush=True)
print("all 8 runs complete", flush=True)
