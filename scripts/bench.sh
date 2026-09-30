#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
out=${1:-results/local}
mkdir -p "$out"
scripts/run-a.sh --delay-us 500 --duration-sec 60 --output "$out/a-60.json" > "$out/a-60.log" 2>&1
scripts/run-a.sh --delay-us 500 --duration-sec 600 --output "$out/a-600.json" > "$out/a-600.log" 2>&1
scripts/run-b.sh --delay-us 500 --duration-sec 600 --output "$out/b-600.json" > "$out/b-600.log" 2>&1
python3 scripts/compare.py "$out/a-600.json" "$out/b-600.json" | tee "$out/ab.txt"
# ~1000 pps in each case, same payload/peer; kernel ping has 1ms resolution.
scripts/run-a.sh --delay-us 64000 --duration-sec 60 --output "$out/a-lowload.json" > "$out/a-lowload.log" 2>&1
scripts/run-c.sh 60 "$out/c" > "$out/c-summary.log" 2>&1
python3 scripts/report.py "$out"
