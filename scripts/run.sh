#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  echo "Usage: $0 a|b [client arguments] | c [seconds] | ab | ac"
  echo "Examples: $0 a --delay-us 500 --duration-sec 60; $0 ab; $0 ac"
}
mode=${1:-}
case "$mode" in
  -h|--help) usage; exit 0 ;;
  a|b|c|ab|ac) shift ;;
  *) usage >&2; exit 1 ;;
esac
if [[ $mode == ab || $mode == ac ]] && (( $# != 0 )); then
  usage >&2
  exit 1
fi
if [[ $mode == c ]] && { (( $# > 1 )) || [[ ! ${1:-60} =~ ^[1-9][0-9]*$ ]]; }; then
  usage >&2
  exit 1
fi
source env.sh
out="results/$mode-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p results
mkdir "$out" # Refuse to overwrite an existing run.
if [[ $mode != c ]]; then
  source "$HOME/.cargo/env"
  export PKG_CONFIG_PATH=/opt/dpdk/lib64/pkgconfig
  cargo build --release --locked -p async-ping -p raw-ping > "$out/build.log" 2>&1
fi

run_client() {
  local client=$1 json=$2 log=$3 bin
  shift 3
  local output=$json previous= arg
  for arg in "$@"; do
    if [[ $previous == --output ]]; then output=$arg; fi
    if [[ $arg == --output=* ]]; then output=${arg#--output=}; fi
    previous=$arg
  done
  mkdir -p "$(dirname "$output")"
  case "$client" in a) bin=async-ping ;; b) bin=raw-ping ;; esac
  sudo "target/release/$bin" --bdf "$BDF" --src-ip "$SRC_IP" --core "$CORE" \
    --peer-ip "$PEER_IP" --peer-mac "$PEER_MAC" --sessions 64 --payload 64 \
    --delay-us 500 --duration-sec 600 --output "$json" "$@" > "$log" 2>&1
  cat "$log"
}
run_kernel() {
  local duration=$1 prefix=$2
  sudo taskset -c "$CORE" ping -n -U -I "$KERNEL_IF" -i 0.001 -s 64 \
    -w "$duration" -W 1 "$PEER_IP" > "$prefix.log"
  python3 scripts/report.py c "$prefix.log" "$prefix.json" "$duration" > "$prefix-summary.log"
  gzip "$prefix.log"
  cat "$prefix-summary.log"
}

case "$mode" in
  a|b)
    run_client "$mode" "$out/$mode.json" "$out/$mode.log" "$@"
    ;;
  c)
    run_kernel "${1:-60}" "$out/c"
    ;;
  ab)
    for spec in 'a 60' 'a 600' 'b 600'; do
      read -r client duration <<< "$spec"
      echo "Running $client: $duration seconds"
      run_client "$client" "$out/$client-$duration.json" "$out/$client-$duration.log" \
        --duration-sec "$duration"
    done
    python3 scripts/report.py ab "$out"
    ;;
  ac)
    echo 'Running A: 60 seconds'
    run_client a "$out/a.json" "$out/a.log" --sessions 1 --delay-us 950 --duration-sec 60
    echo 'Running C: 60 seconds'
    run_kernel 60 "$out/c"
    python3 scripts/report.py ac "$out"
    ;;
esac
echo "Results: $out"
