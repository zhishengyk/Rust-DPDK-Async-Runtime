# TSC 接口、HDR 分桶与后台统计对照

测试日期：2026-10-01（Asia/Shanghai）。所有延迟单位为 **ns**；原始运行起止时间在 JSON 清单中明确使用 UTC。

## 结论

已改用官方 TSC intrinsic、DPDK 提供的 TSC 频率，以及第三方 hdrhistogram。统计线程通过有界 SPSC 队列接收延迟数值，绑定独立核 3。
同一计时/精度下，A 的 process p99：同步统计两轮均为 710ns，后台统计为 710/720ns，未观察到稳定的 process 收益。后台版 A 的 timer p99 为 1130/1210ns，同步版为 1450/1460ns；这是不同指标。
旧版 600 秒报告的 A/process p99 为 713ns，量级一致。旧报告是自写约 1.6% 分桶与自行校准频率，本次为 HDR 与 DPDK 频率，不能把几 ns 差值全部解释为优化收益。
一次后台 A 测试出现 39 次超时且随后全部收到迟到 reply；该异常保留并在下文单列，原因尚未定位。

## 计时与实现

| 指标 | 定义 | 本次是否移动打点 |
|---|---|---|
| process（主指标） | 每个请求先计算 `(T1−T0)+(T3−T2)`，然后统计分位数 | 否 |
| send / receive | `T1−T0` / `T3−T2` | 否 |
| end_to_end | `T3−T0`，包含对端及网络往返 | 否 |
| timer | sleep deadline → 下一个 T0，不计入主指标 | 否 |
| sleep_error | sleep deadline → 恢复执行时刻 | 否 |

Rust 在 `src/metrics.rs::now` 使用 `_mm_lfence(); _rdtsc(); _mm_lfence()`，并保留编译器屏障。C shim 用对应 intrinsic 和编译器屏障。实际汇编仍是 LFENCE/RDTSC/LFENCE，见 [反汇编](tsc-assembly.txt)。
EAL 初始化后由 `rte_get_tsc_hz()` 提供频率；各份本次 JSON 均记录 **2600000000Hz**，不再自行 sleep 500ms 校准。TSC ticks 仍在原打点处记录，在统计线程换算为 ns。
HDR 固定 5 位有效数字：0～262143ns 每个整数独立一桶，更大值允许合并；max 单独精确记录。启动时预分配全 u64 范围，禁止自动扩容。这个精度是分桶精度，不代表整套测量的绝对误差只有 1ns。
收发线程在原记录位置追加数值到固定 256 条本地缓冲，批量复制到容量 16384 条的 rtrb SPSC 队列；不逐批分配或清零，不跨线程传递 mbuf。统计线程忙轮询核 3，执行换算、入桶和最终汇总。
队列满时背压并累计 backpressure_batches，不静默丢样本；退出提交尾批、排空并 join。终端日志/JSON 原本就在测量结束后输出，继续保持该位置。
统计原本发生在 reply 的 T3 之后，因而移到后台的直接目标是减少后续循环/timer 的工作量；不会从 process 里直接扣掉原本就未计入的统计成本。后台线程仍可能通过缓存、内存和系统调度影响整体运行。

## 同精度对照

基线提交 `f674490a49018459e09d6c5cc934a6213005b27b`。inline 和 background 都使用新的 TSC/DPDK 频率和 HDR 精度，只比较统计同步执行与后台执行。
AWS c8a.xlarge / AMD EPYC 9R45 / Amazon Linux 2023 / Rust 1.90.0 / DPDK 23.11.5 / ENA。收发核 2、后台统计核 3、控制进程核 0–1；未启用启动级 isolcpus。
每次 30 秒，64 sessions、64B payload、delay 500us、timeout 10ms。第一轮 inline A/B → background A/B，第二轮反序；每次独占同一个 DPDK 端口。测试期间不编译。

| 测试 | process p50/p99 | send p50/p99 | receive p50/p99 | timer p50/p99 | RX | timeout | 背压批数 |
|---|---:|---:|---:|---:|---:|---:|---:|
| [inline-a-1](inline-a-1.json) | 230 / 710 | 80 / 540 | 140 / 360 | 210 / 1450 | 2660593 | 0 | 0 |
| [inline-b-1](inline-b-1.json) | 200 / 670 | 80 / 520 | 120 / 320 | 160 / 1530 | 2974110 | 0 | 0 |
| [background-a-1](background-a-1.json) | 230 / 710 | 80 / 550 | 140 / 340 | 190 / 1130 | 2871329 | 0 | 0 |
| [background-b-1](background-b-1.json) | 200 / 660 | 80 / 520 | 120 / 280 | 130 / 1340 | 2880418 | 0 | 0 |
| [inline-a-2](inline-a-2.json) | 240 / 710 | 80 / 540 | 140 / 360 | 210 / 1460 | 2794868 | 0 | 0 |
| [inline-b-2](inline-b-2.json) | 200 / 680 | 80 / 530 | 120 / 330 | 160 / 1430 | 2908173 | 0 | 0 |
| [background-a-2](background-a-2.json) | 240 / 720 | 80 / 550 | 140 / 350 | 200 / 1210 | 2936018 | 39 | 0 |
| [background-b-2](background-b-2.json) | 200 / 710 | 80 / 570 | 120 / 280 | 130 / 1300 | 2908388 | 0 | 0 |

| 统计方式/轮次 | A−B process p50 | A−B process p99 |
|---|---:|---:|
| inline/1 | 30 | 40 |
| inline/2 | 40 | 30 |
| background/1 | 30 | 50 |
| background/2 | 40 | 10 |

上述 A−B 是两次独立运行的分位数之差，不是配对样本的耗时；本次短测不能证明稳定的 A−B 改善，也不替代原来的 600 秒主测试。本次没有重测 C。

## 异常保留

`background-a-2`：TX 2936057，正常 RX 2936018，timeout 39，late_reply 39，missing 0。NIC 的 missed/rx_errors/tx_errors/rx_nombuf 均为 0，mbuf 4095/4095，统计队列背压为 0。
该次 process p99 仍为 720ns，但 process max 为 1059640ns；sleep_error max 为 11807040ns（约 11.8ms）。**max 与 p99 是不同统计量，timer 与 process 也是不同测量区间。**
39 个超时请求的 T0 集中在不到 0.4ms 的区间，最终全部观察到迟到回复；现有数据不足以把原因归为网络、虚拟化、OS 调度或本次代码中的某一项。超时不进入成功请求的 process/RTT 直方图，另外完整列在 JSON 的 losses 中。
最初对照脚本额外断言 timeout=0，因这次异常停止；保留原 JSON 后，将断言改为验证 `TX=RX+timeout`、直方图计数和资源对账，再继续尚未运行的 inline B/A。没有重跑覆盖异常窗口；该条记录的准确启动时间未保存，runs.json 明确标为 null 并保留时间下界。

## 最终版本实机验证

A/B 各补测 60 秒标准参数；另外各跑 1 秒：timeout=1us/单 session/奇数 payload=63、delay=0/payload=8，以及 128 sessions/payload=1472。

| 测试 | TX | RX | timeout | late | missing | process p99 | timer p99 | mbuf 初/末 | 背压批数 |
|---|---:|---:|---:|---:|---:|---:|---:|---|---:|
| [check-a-60s](check-a-60s.json) | 5796799 | 5796799 | 0 | 0 | 0 | 700 | 1110 | 4095/4095 | 0 |
| [check-b-60s](check-b-60s.json) | 5934776 | 5934776 | 0 | 0 | 0 | 680 | 1280 | 4095/4095 | 0 |
| [check-a-timeout](check-a-timeout.json) | 100 | 0 | 100 | 100 | 0 | 0 | 190 | 4095/4095 | 0 |
| [check-b-timeout](check-b-timeout.json) | 100 | 0 | 100 | 100 | 0 | 0 | 150 | 4095/4095 | 0 |
| [check-a-min](check-a-min.json) | 22688 | 22688 | 0 | 0 | 0 | 710 | 90 | 4095/4095 | 0 |
| [check-b-min](check-b-min.json) | 23484 | 23484 | 0 | 0 | 0 | 720 | 90 | 4095/4095 | 0 |
| [check-a-max](check-a-max.json) | 25295 | 25295 | 0 | 0 | 0 | 720 | 990 | 4095/4095 | 0 |
| [check-b-max](check-b-max.json) | 25281 | 25281 | 0 | 0 | 0 | 710 | 940 | 4095/4095 | 0 |

timeout 测试中成功样本数为 0 时，输出的 process p99=0 表示空直方图，不能解释为零延迟。
全部已保存的最终版本实测均满足成功样本四组直方图 count=RX，sleep_error count=TX，timer count=TX−sessions（最后一次 sleep 没有下一个 T0），无重复包、分配/TX 失败或 NIC 错误，mbuf 完整归还。
`cargo test --workspace --locked`：11 个单元测试通过；`cargo clippy --workspace --all-targets -- -D warnings`、release 构建通过。统计测试覆盖逐请求相加而非相加分位数、1ns 桶/大值 max、尾批排空和样本计数。
线程实际亲和性见 [thread-affinity.json](thread-affinity.json)。后台版额外消耗一个忙轮询 CPU 核，不能宣传为总资源不变或统计零开销。

## 时间读数步进复核

针对“为什么 process 输出多为 0 结尾”的复核：链接项目实际 `metrics::now`、`Clock::ns` 和 `Recorder`，诊断进程固定核 2，采集时不访问网卡、不换算、不入桶、不打印。频率沿用该机器 DPDK 实测的 2600000000Hz。

| 读法/负载 | 时间差样本数 | 时间差 ticks 的最大公约数 | 非 26 倍数的时间差 |
|---|---:|---:|---:|
| raw_rdtsc | 1000000 | 1 | 319640 |
| project_metrics_now | 1000000 | 26 | 0 |
| project_now_varied_work | 1000000 | 26 | 0 |
| legacy_asm_varied_work | 1000000 | 26 | 0 |

当前有屏障的读法，无论连续空读还是穿插 0～63 次变化的整数运算，时间差都落在 26 ticks 的网格上：`26 × 10^9 / 2600000000 = 10ns`。旧版内联汇编读法也一样。因此本机这条有序计时路径实测呈 10ns 离散步进，不能把“1ns 桶”宣传为真实 1ns 测量分辨率。

这不是把 TSC 的标称 2.6GHz 直接当成可观测的 0.385ns 分辨率。裸 RDTSC 的诊断确实出现 1/25 ticks 差值；它与有屏障的读法不同，本次证据不足以断言所有 TSC 读法或所有机器都只有 10ns 物理分辨率，也不能据此认定是某个 AWS 虚拟化机制造成的。此项是本机实测现象，不是硬件规格保证。

另外，将人工构造的 701～709ns 样本送入实际 Clock 换算 → SPSC → HDR → 分位数 → JSON 链路，p99 依次原样输出 701～709ns。`Clock::ns` 仅向下取整到整数 ns，终端的 `{:10}` 表示字段宽度，JSON 直接序列化整数；没有按 10ns 舍入。未发现这条转换/统计/输出路径把个位清零的问题。

旧报告的 713ns 也不证明旧版能分辨真实的 1ns 变化。举例：原始 1846 ticks（按 2.6GHz 为 710ns），旧版自写直方图所在桶的上界是 1855 ticks，再按旧 A 频率 2599982104Hz 换算，报告就会显示 713ns；这是桶上界近似造成的非整十尾数。新版在该范围内不再引入这项桶上界误差。不能把 713→710ns 直接解释成代码省了 3ns。

原始数据见 [tsc-probe-core2.json](tsc-probe-core2.json)，诊断源码见 [tsc-probe.rs](tsc-probe.rs)。本次不改变生产代码、计时屏障或既有 A/B 性能结果。复现命令（在可构建的工作区、release 库已构建后）：

```bash
source /home/ec2-user/.cargo/env
rustc --edition=2021 -O -C panic=abort results/metrics-offload/tsc-probe.rs \
  -L dependency=target/release/deps \
  --extern metrics=target/release/libmetrics.rlib \
  --extern serde_json="$(ls target/release/deps/libserde_json-*.rlib | head -n 1)" \
  -o target/metrics-bench/tsc-probe
taskset -c 2 target/metrics-bench/tsc-probe
```

## 未采用的初版

曾用标准库 sync_channel 传整批事件，统计线程阻塞接收。A timer p99 为 2200/2570ns，对应同步版 1450/1420ns；未保留该实现，改为目前的 SPSC 环形队列。原始数据保存在 [channel/](channel/)，源码快照是 [channel.patch.gz](channel.patch.gz)，不参与最终编译。

## 复核与重现

- 原始命令、起止 UTC 时间：[runs.json](runs.json)、[validation-runs.json](validation-runs.json)。
- 构建与测试：[build-background.log](build-background.log)、[tests.log](tests.log)、[clippy.log](clippy.log)。
- 固定二进制哈希：[inline-binaries.sha256](inline-binaries.sha256)、[background-binaries.sha256](background-binaries.sha256)。
- 同步基线源码补丁：[inline.patch.gz](inline.patch.gz)，解压并应用于上述基线提交后构建；最终版源码位于 `experiment/metrics-offload` 分支。
- 对照脚本：[scripts/bench-metrics.py](../../scripts/bench-metrics.py)，需要先把两种构建的二进制分别放入 `target/metrics-bench/inline/` 与 `target/metrics-bench/background/`。脚本会跳过 runs.json 中已保存的窗口；独立复测示例：`source env.sh; python3 scripts/bench-metrics.py 30 results/local/metrics-repeat`，保留本次记录。

接口参考：[Rust _rdtsc](https://doc.rust-lang.org/core/arch/x86_64/fn._rdtsc.html)、[DPDK TSC 接口](https://doc.dpdk.org/api/rte__cycles_8h.html)、[hdrhistogram](https://docs.rs/hdrhistogram/7.6.0/hdrhistogram/struct.Histogram.html)、[rtrb](https://docs.rs/rtrb/0.4.0/rtrb/)。
