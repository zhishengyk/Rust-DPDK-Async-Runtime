#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source env.sh
duration=${1:-60}
output=${2:-results/local/c}
mkdir -p "$(dirname "$output")"
sudo taskset -c 2 ping -n -U -I "$KERNEL_IF" -i 0.001 -s 64 -w "$duration" -W 1 "$PEER_IP" > "$output.log"
python3 scripts/report-c.py "$output.log" "$output.json" "$duration"
gzip -f "$output.log"
