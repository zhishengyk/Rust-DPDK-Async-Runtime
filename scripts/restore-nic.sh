#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source env.sh
echo "$BDF" | sudo tee "/sys/bus/pci/devices/$BDF/driver/unbind" >/dev/null
echo ena | sudo tee "/sys/bus/pci/devices/$BDF/driver_override" >/dev/null
echo "$BDF" | sudo tee /sys/bus/pci/drivers_probe >/dev/null
sudo networkctl reconfigure "$DPDK_IF"
