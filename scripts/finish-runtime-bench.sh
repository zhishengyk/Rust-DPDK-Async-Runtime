#!/usr/bin/env bash
# Continue the already-started run in the background and publish its report.
set -euo pipefail
cd "$(dirname "$0")/.."
source env.sh
prior_pid=$1
while kill -0 "$prior_pid" 2>/dev/null || pgrep -f '^target/runtime-(final|baseline)/(async|raw)-ping ' >/dev/null
do
    sleep 2
done
for spec in 'a async-ping' 'b raw-ping'
do
    read -r client binary <<< "$spec"
    output="results/runtime-optimization/$client-600"
    if [[ ! -s "$output.json" ]]; then
        date -u '+%FT%TZ' > "$output-start.txt"
        sudo "target/runtime-final/$binary" --bdf "$BDF" --src-ip "$SRC_IP" \
            --delay-us 500 --duration-sec 600 --output "$output.json" > "$output.log" 2>&1
    fi
    python3 scripts/report-runtime.py
done
python3 scripts/report-runtime.py
date -u '+%FT%TZ' > results/runtime-optimization/COMPLETED
