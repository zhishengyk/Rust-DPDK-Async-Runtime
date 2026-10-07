#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  echo "Usage: $0 [install|prepare] [--isolate-on-reboot] | restore"
  echo "install: dependencies, build and host setup; prepare: host setup after reboot; restore: kernel NIC driver"
}
action=install
case ${1:-} in
  -h|--help) usage; exit 0 ;;
  install|prepare|restore) action=$1; shift ;;
  ''|--isolate-on-reboot) ;;
  *) usage >&2; exit 1 ;;
esac
isolation=${1:-}
if (( $# > 1 )) || [[ -n "$isolation" && $isolation != --isolate-on-reboot ]] || [[ $action == restore && -n "$isolation" ]]; then
  usage >&2
  exit 1
fi

install_dependencies() {
  sudo dnf install -y git gcc gcc-c++ make meson ninja-build python3-pip numactl-devel libatomic clang clang-devel pciutils ethtool
  python3 -m pip install --user pyelftools==0.32
  if ! test -x "$HOME/.cargo/bin/rustup"; then
    curl -fsSL https://sh.rustup.rs -o /tmp/ping-rustup.sh
    sh /tmp/ping-rustup.sh -y --profile minimal --default-toolchain 1.90.0
  fi
  source "$HOME/.cargo/env"
  export PKG_CONFIG_PATH=/opt/dpdk/lib64/pkgconfig
  if ! pkg-config --exact-version=23.11.5 libdpdk; then
    mkdir -p build
    curl -fL --retry 2 https://fast.dpdk.org/rel/dpdk-23.11.5.tar.xz -o build/dpdk.tar.xz
    tar -xf build/dpdk.tar.xz -C build
    meson setup build/dpdk-stable-23.11.5/build build/dpdk-stable-23.11.5 --prefix=/opt/dpdk \
      -Dplatform=native -Ddefault_library=static -Denable_drivers=net/ena,mempool/ring -Dtests=false -Dexamples=
    ninja -C build/dpdk-stable-23.11.5/build -j3
    sudo ninja -C build/dpdk-stable-23.11.5/build install
  fi
  cargo build --release --locked
}

prepare_host() {
  if ! test -f env.sh; then
    python3 - <<'DISCOVER_PY' > env.sh
import pathlib
import shlex
import urllib.request

base = 'http://169.254.169.254/latest/'
req = urllib.request.Request(base + 'api/token', method='PUT', headers={'X-aws-ec2-metadata-token-ttl-seconds': '60'})
token = urllib.request.urlopen(req, timeout=3).read().decode()
# 使用启动时取得的 IMDSv2 token 读取指定元数据路径，返回去除首尾空白的文本。
def get(path):
    req = urllib.request.Request(base + 'meta-data/' + path, headers={'X-aws-ec2-metadata-token': token})
    return urllib.request.urlopen(req, timeout=3).read().decode().strip()

interfaces = {p.joinpath('address').read_text().strip(): p for p in pathlib.Path('/sys/class/net').iterdir()}
config = {}
for mac in get('network/interfaces/macs/').splitlines():
    mac = mac.rstrip('/')
    path = f'network/interfaces/macs/{mac}/'
    device = int(get(path + 'device-number'))
    iface = interfaces[mac]
    if device == 0:
        config['KERNEL_IF'] = iface.name
    else:
        config.update(BDF=iface.joinpath('device').resolve().name, SRC_IP=get(path + 'local-ipv4s').splitlines()[0], DPDK_IF=iface.name)
config.update(CORE='2', PEER_IP='10.202.8.15', PEER_MAC='06:ff:fd:b6:f0:cd')
assert 'BDF' in config and 'KERNEL_IF' in config, 'Two ENIs required'
for key, value in config.items():
    print(f'export {key}={shlex.quote(value)}')
DISCOVER_PY
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
}

restore_nic() {
  source env.sh
  echo "$BDF" | sudo tee "/sys/bus/pci/devices/$BDF/driver/unbind" >/dev/null
  echo ena | sudo tee "/sys/bus/pci/devices/$BDF/driver_override" >/dev/null
  echo "$BDF" | sudo tee /sys/bus/pci/drivers_probe >/dev/null
  sudo networkctl reconfigure "$DPDK_IF"
}

case "$action" in
  install) install_dependencies; prepare_host "$isolation" ;;
  prepare) prepare_host "$isolation" ;;
  restore) restore_nic ;;
esac
