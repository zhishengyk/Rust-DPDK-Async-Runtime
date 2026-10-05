#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mode=${1:-}
case "$mode" in ab|ac) ;; *) echo "Usage: $0 ab|ac" >&2; exit 1 ;; esac
source "$HOME/.cargo/env"
export PKG_CONFIG_PATH=/opt/dpdk/lib64/pkgconfig
out="results/$mode-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$out"
cargo build --release --locked -p async-ping -p raw-ping > "$out/build.log" 2>&1
if [[ $mode == ab ]]; then
  for spec in 'a 60' 'a 600' 'b 600'; do
    read -r client duration <<< "$spec"
    echo "Running $client: $duration seconds"
    "scripts/run-$client.sh" --sessions 64 --payload 64 --delay-us 500 --duration-sec "$duration" --output "$out/$client-$duration.json" > "$out/$client-$duration.log" 2>&1
  done
else
  echo 'Running A: 60 seconds'
  scripts/run-a.sh --sessions 1 --payload 64 --core 2 --delay-us 950 --duration-sec 60 --output "$out/a.json" > "$out/a.log" 2>&1
  echo 'Running C: 60 seconds'
  scripts/run-c.sh 60 "$out/c" > "$out/c-summary.log" 2>&1
fi
python3 scripts/report.py "$mode" "$out"
