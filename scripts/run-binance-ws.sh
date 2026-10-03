#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source env.sh
mkdir -p results/binance-ws
exec sudo target/release/binance-ws --bdf "$BDF" --src-ip "$SRC_IP" --core "$CORE" "$@"
