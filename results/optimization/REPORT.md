# 延迟优化对照

2026-09-30，同一开发机、同一 ENA、核 2。基线为 `d0878e5`，基线二进制在修改前复制并保留。
这里只调整项目自身 C shim 传给 DPDK 的配置参数；未修改 DPDK、ENA 驱动或 Linux 内核源码。
原始 600 秒验收数据保留在 [原报告](../REPORT.md)。本次复测的数据独立保存。

## 改动与取舍

- `rte_pktmbuf_pool_create` 的 cache_size 从 0 改为 128，减少常见分配/释放路径上的共享 mempool ring 操作。
- `tx_free_thresh` 从驱动默认值改为 `tx - 16`。本机 TX ring 为 512，ENA 默认阈值为 448，
  新阈值为 496；驱动在空闲描述符少于阈值时尝试回收。因此通常从积累约 64 个描述符改为约 16 个。
  这不是固定回收 16 个包：实际批量还取决于完成情况和每包所用描述符数。

A/B 共用以上配置。四个打点、TSC 指令、调度顺序、收发逻辑、会话数与 delay 均未改变。
较早回收会增加检查完成队列的频率，目的是减少单次发送承担的大批回收工作；不宣称提高最大吞吐量。
`rte_mempool_avail_count` 包含各 lcore 的缓存，开启缓存后仍用 4095 个 mbuf 的初末对账。

ENA 驱动的依据：DPDK 23.11.5 `drivers/net/ena/ena_ethdev.c` 的 `ena_tx_queue_setup`、
`eth_ena_xmit_pkts` 和 `ena_tx_cleanup`。只读取该源码来定位行为，没有改动它。
[设备诊断日志](ena-info.log) 已确认 `Placement policy: Low latency`，所以本次没有额外开启 LLQ。

## 20 秒逐项试验

每次 64 session、64B payload、delay 500us、timeout 10ms；A/B 顺序独占端口。单位 ns。

| 配置 | A send p50/p99 | A process p50/p99 | B send p50/p99 | B process p50/p99 |
|---|---:|---:|---:|---:|
| 原始 | 80 / 1575 | 270 / 1747 | 90 / 1599 | 211 / 1698 |
| 仅 cache=128 | 80 / 1341 | 261 / 1501 | 80 / 1205 | 190 / 1328 |
| cache=128，tx−16（采用） | 80 / 553 | 291 / 762 | 80 / 541 | 202 / 695 |
| cache=128，tx−4（未采用） | 202 / 424 | 381 / 664 | 181 / 411 | 291 / 553 |

更激进的 tx−4 虽然降低 p99，却明显增加中位数开销，所以最终采用 tx−16。
这些为不同时间窗口的分位数，不是逐包配对差；直方图误差上界约 1.6%。

原始数据分别为 `baseline-{a,b}`、`cache-{a,b}`、`threshold-{a,b}`（tx−16）、
`threshold4-{a,b}` 的 JSON 和日志。各候选的二进制摘要见
[基线 SHA256](baseline-binaries.sha256) 和 [候选 SHA256](candidate-binaries.sha256)。

## 确认测试

按原始 A → 优化 A → 优化 B → 原始 B 的顺序各运行 60 秒。参数与上面的逐项试验相同。
以下为本次新测数据，未拿历史 600 秒测试代替原版复测。单位 ns。

| 客户端 | send p50 | send p99 | receive p50 | receive p99 | process p50 | process p99 | RX |
|---|---:|---:|---:|---:|---:|---:|---:|
| [原始 A](validation-baseline-a.json) | 80 | 1599 | 181 | 405 | 282 | 1771 | 6018365 |
| [优化 A](validation-optimized-a.json) | 80 | 553 | 181 | 381 | 282 | 762 | 5869485 |
| [原始 B](validation-baseline-b.json) | 80 | 1599 | 121 | 291 | 211 | 1722 | 5890512 |
| [优化 B](validation-optimized-b.json) | 80 | 535 | 110 | 282 | 202 | 664 | 6008006 |

A 的 send p99 降低 **65.4%**，process p99 降低 **57.0%**，两项 p50 保持不变。
B 的 send p99 降低 **66.5%**，process p99 降低 **61.4%**。
20 秒试验和 60 秒复测均支持发送 p99 改善；没有消除所有极端尖峰，完整 p99.9/p99.99/max 见 JSON。
四轮均为 TX=RX、timeout=0，NIC missed/error/rx_nombuf=0，mbuf 4095→4095。

收益来自公共发送路径，不能声称 runtime 更快了：A−B 的 receive p50 分位数差为原版 60ns、
优化版 71ns；process p50 差为 71ns、80ns。调度代码没有改动，这些顺序测试差值也不是逐包配对开销。

| 客户端 | end_to_end p50 | end_to_end p99 | 实测 RX pps |
|---|---:|---:|---:|
| 原始 A | 157539 | 242610 | 100305.06 |
| 优化 A | 160690 | 168567 | 97823.87 |
| 原始 B | 159114 | 239459 | 98174.44 |
| 优化 B | 157539 | 245761 | 100132.74 |

高负载 RTT 没有一致变化。尤其 A 的这一轮 RTT p99 降低，不能单独归因于配置：
20 秒试验的原始 A/优化 A RTT p99 分别是 166991/168566ns，方向相反。
闭环请求会随 RTT 改变实际 pps，本次也没有完成固定到达速率的最大吞吐量测试。

## 低负载与边界回归

低负载 A 使用 64 session、delay 64000us、每轮 30 秒，其余参数相同。
以下按实际运行顺序排列；每轮收到 29975 个 reply，约 997.1pps。单位 ns。

| 轮次 | end_to_end p50 | end_to_end p99 | end_to_end max |
|---|---:|---:|---:|
| [原始 1](lowload-baseline-1.json) | 51987 | 59864 | 124090 |
| [优化 1](lowload-optimized-1.json) | 51987 | 59864 | 111250 |
| [优化 2](lowload-optimized-2.json) | 51987 | 60652 | 123220 |
| [原始 2](lowload-baseline-2.json) | 51987 | 60652 | 110660 |

在当前测量精度和测试窗口内，**未观察到低负载 RTT p50/p99 的改善**。
发送尾延迟的收益不能直接推广为网络往返延迟收益；本轮也没有重新测试 C。

边界用例各运行 1 秒。强制超时为 1 session、delay 10000us、timeout 1us、payload 63B；
最小包为 1 session、delay 0、payload 8B；最大包为 128 session、delay 5000us、payload 1472B。

| 用例 | TX | RX | timeout | late | missing | mbuf 初/末 |
|---|---:|---:|---:|---:|---:|---|
| [A 强制超时](check-a-timeout.json) | 100 | 0 | 100 | 100 | 0 | 4095/4095 |
| [B 强制超时](check-b-timeout.json) | 100 | 0 | 100 | 100 | 0 | 4095/4095 |
| [A 最小包](check-a-minimum.json) | 22692 | 22692 | 0 | 0 | 0 | 4095/4095 |
| [B 最小包](check-b-minimum.json) | 22706 | 22706 | 0 | 0 | 0 | 4095/4095 |
| [A 最大包](check-a-maximum.json) | 25269 | 25269 | 0 | 0 | 0 | 4095/4095 |
| [B 最大包](check-b-maximum.json) | 25288 | 25288 | 0 | 0 | 0 | 4095/4095 |

全部 22 次实机运行通过包数、成功样本数和 mbuf 对账，NIC missed/error/rx_nombuf 均为 0。
除两轮人为设置的 1us 超时测试外，没有 timeout；强制超时的 200 个包均在 drain 结束前观测为 late。
`cargo test --workspace` 的 7 个单元测试通过，见 [测试日志](tests.log)。

## 复现

基线提交见 [baseline-commit.txt](baseline-commit.txt)，最终 release 二进制见
[final-binaries.sha256](final-binaries.sha256)。本机保留 `target/baseline/` 与 `target/optimized/`，
分别装有修改前、最终配置的两个客户端；`target/` 不提交到 Git。

```bash
source env.sh
# variant=baseline 或 optimized；客户端为 async-ping 或 raw-ping
variant=optimized
sudo target/$variant/async-ping --bdf "$BDF" --src-ip "$SRC_IP" \
  --delay-us 500 --duration-sec 60 --output results/local/recheck.json
# 低负载使用 --delay-us 64000 --duration-sec 30，其余默认参数相同
```

最终改动只有两个配置参数和解释性注释，不新增依赖、异常处理或逐包检查。
本次每组确认测试为 60 秒；原报告的 600 秒结果属于优化前版本。
