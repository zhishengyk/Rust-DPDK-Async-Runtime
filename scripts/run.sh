#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source env.sh
bin=$1
shift
mkdir -p results/local
if test "$#" -eq 0; then set -- --delay-us 500 --duration-sec 600; fi
exec sudo "target/release/$bin" --bdf "$BDF" --src-ip "$SRC_IP" --core "$CORE" \
  --peer-ip "$PEER_IP" --peer-mac "$PEER_MAC" --output "results/local/$bin.json" "$@"
