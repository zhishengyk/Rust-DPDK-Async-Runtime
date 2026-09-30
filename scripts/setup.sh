#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
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
scripts/prepare-host.sh "${1:-}"
