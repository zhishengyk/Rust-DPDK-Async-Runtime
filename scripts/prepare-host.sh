#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if ! test -f env.sh; then
  python3 scripts/discover.py > env.sh
fi
source env.sh
sudo sysctl -w vm.nr_hugepages=1024
echo 'vm.nr_hugepages=1024' | sudo tee /etc/sysctl.d/80-dpdk-ping.conf >/dev/null
sudo mkdir -p /dev/hugepages
if ! mountpoint -q /dev/hugepages; then sudo mount -t hugetlbfs nodev /dev/hugepages; fi
sudo modprobe vfio-pci
echo 1 | sudo tee /sys/module/vfio/parameters/enable_unsafe_noiommu_mode >/dev/null
if test -d "/sys/bus/pci/devices/$BDF/net/$DPDK_IF"; then
  # Do not detach the interface carrying the default route/SSH.
  primary=$(ip -4 route show default | awk 'NR == 1 {print $5}')
  test "$DPDK_IF" != "$primary"
  sudo ip link set "$DPDK_IF" down
  echo vfio-pci | sudo tee "/sys/bus/pci/devices/$BDF/driver_override" >/dev/null
  echo "$BDF" | sudo tee "/sys/bus/pci/devices/$BDF/driver/unbind" >/dev/null
  echo "$BDF" | sudo tee /sys/bus/pci/drivers_probe >/dev/null
fi
# Keep movable IRQs and unbound workqueues on housekeeping cores.
sudo systemctl disable --now irqbalance
echo 3 | sudo tee /proc/irq/default_smp_affinity >/dev/null
echo 3 | sudo tee /sys/devices/virtual/workqueue/cpumask >/dev/null
sudo bash -c 'for irq in /proc/irq/*/smp_affinity_list; do echo 0-1 > "$irq" 2>/dev/null || :; done'
if test "${1:-}" = --isolate-on-reboot; then
  sudo mkdir -p /etc/systemd/system.conf.d
  printf '[Manager]\nCPUAffinity=0 1\n' | sudo tee /etc/systemd/system.conf.d/80-dpdk-ping.conf >/dev/null
  sudo grubby --update-kernel=ALL --args='isolcpus=domain,managed_irq,2-3 nohz_full=2-3 rcu_nocbs=2-3 irqaffinity=0-1'
  echo 'Boot isolation configured; it takes effect after your next reboot.'
fi
echo "DPDK: $BDF $SRC_IP; kernel: $KERNEL_IF; runtime core: $CORE"
