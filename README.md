# Rust DPDK Async Runtime

单核 Rust runtime（executor、Waker、timer、poll-mode reactor）及 ICMP 客户端。
A 是 async-ping，B 是相同行为的裸 busy-poll，C 是系统 ping。

## 环境与安装

Amazon Linux 2023、AWS c8a.xlarge、两张 ENA、passwordless sudo。
Rust **1.90.0**，DPDK **23.11.5**。首次部署运行 `./scripts/setup.sh --isolate-on-reboot`，安装依赖、编译、配置大页、DPDK 网卡及启动隔离，然后 `sudo reboot`。
device-number 0 留给 SSH/内核；另一张绑定 vfio-pci。运行配置由脚本生成到 `env.sh`。
重启后先运行 `./scripts/prepare-host.sh`，再执行下面的测试。

一键测试并生成汇总：`./scripts/run.sh ab`（A/B）或 `./scripts/run.sh ac`（A/C）。
结果保存在新的 `results/ab-时间戳/` 或 `results/ac-时间戳/` 目录，结束后打印 `SUMMARY.md` 路径。

## 直接运行

以下是本机的原始命令，在仓库根目录分别执行，等上一个完成后再运行下一个。
首次准备：`mkdir -p results/ab results/ac`。A/B 默认使用核 2、payload 64B、对端 `10.202.8.15`。
结束后终端显示统计，`--output` 指定 JSON 文件；相同文件名会覆盖旧结果。

### A/B：正式测试

A 和 B 都是 64 session、delay 500µs、持续 600 秒：

```bash
sudo target/release/async-ping --bdf 0000:28:00.0 --src-ip 10.202.15.210 \
  --sessions 64 --delay-us 500 --duration-sec 600 --output results/ab/a-600.json
```

```bash
sudo target/release/raw-ping --bdf 0000:28:00.0 --src-ip 10.202.15.210 \
  --sessions 64 --delay-us 500 --duration-sec 600 --output results/ab/b-600.json
```

60 秒现场演示：把 A 命令中的 `--duration-sec 600` 改为 `60`，输出改为 `results/ab/a-60.json`。
需要保留终端日志时，在命令后追加 `2>&1 | tee results/ab/a-600.log`，B/演示使用各自文件名。

### A/C：单会话端到端对照

A：一个 session，收到回复后等待 950µs，运行 60 秒。

```bash
sudo target/release/async-ping --bdf 0000:28:00.0 --src-ip 10.202.15.210 \
  --sessions 1 --delay-us 950 --duration-sec 60 --output results/ac/a.json
```

C：系统 ping，同一客户端核、对端和包长，每 1ms 发包，运行 60 秒；同时显示并保存日志。

```bash
sudo taskset -c 2 ping -n -U -I enp39s0 -i 0.001 -s 64 -w 60 -W 1 10.202.8.15 \
  | tee results/ac/c.log
```

A 的 950µs 等待加约 50µs RTT，对应约 1000pps，与 C 匹配；不能直接把两边的间隔参数设成相同数值。
C 的 `-U` 计到用户态接收；原生命令显示逐包 RTT 和 min/avg/max，完整分位数由下面的日志统计生成。
两张 ENI、发送策略和计时边界仍有差异，ping 文本在当前延迟范围以 1µs 为刻度。
本次 C 的 IRQ 位于核 0–1，保留 ENA 的 Adaptive RX=on、rx-usecs=20；比较结论限定于此配置。
结论只适用于单会话负载，不能外推到正式 64 session；实际速率相差超过 1% 时不计算收益。

### 生成交付汇总

测量命令各自独立运行，下面仅汇总已有文件，不会重新发包：

```bash
python3 scripts/report.py ab results/ab
python3 scripts/report-c.py results/ac/c.log results/ac/c.json 60
python3 scripts/report.py ac results/ac
```

输出 `results/ab/SUMMARY.md` 和 `results/ac/SUMMARY.md`；JSON 延迟单位是 ns，除以 1000 得 µs。
交付时保留对应 JSON 和日志。A/B 汇总只列 600 秒结果，不列 60 秒演示和 T3−T0。

A/B 默认另存同名 `.ticks` 全量打点日志；`.log` 是终端输出，不能替代原始打点。
记录请求测量的 T0–T5，不记录空轮询、ARP 或 runtime 内部的时钟查询。按事件保存原始 TSC，不采样、不按纳秒取整；统计线程在核 3 缓冲写盘，队列满时等待，不能静默丢记录。完整记录仍有复制和 I/O 开销，`--no-trace` 仅用于验证这部分开销。
64 session 的每个 600 秒日志约 11GB，运行前留足空间；结束后可用 `gzip -1 文件.ticks` 无损压缩。
查看前 10 条：`python3 scripts/read-trace.py 文件.ticks --limit 10`；省略 `--limit` 导出全部 CSV，也支持 `.ticks.gz`。

格式：64B 文件头，前 8B 为 `DPDKTS01`，接下来 8B 为小端 TSC Hz；随后每条 64B，依次为 8 个小端 u64：事件类型、`session<<32|seq`、T0、T1、T2、T3、T4、T5。
类型 1=回复（T0–T3），2=等待结束（T4/T5），3=下次发送（T0′及前次 T4），4=超时（T0/T1），5=迟到（T0/T2），6=分配失败（T0），7=提交失败（T0/T1）；未发生的时刻填 0。记录按事件发布顺序排列，并非全局时间排序。按 session 的事件顺序关联请求，seq 回绕时结合 T0 区分。C 保留逐包 ping 原文，不伪造它未提供的内部时间戳。

## 架构与测量

`src/rt.rs` 提供单线程 executor、Waker、timer 和 reply 槽；`src/dpdk.rs` / `native/shim.c`
负责 DPDK 封装；`src/wire.rs` 实现 ICMP checksum 和 ARP 应答。
每批 `rx_burst` 返回后解析报文，以 session id/seq/T0 匹配请求，放入 reply 槽并调用 Waker；
Waker 将对应 task 放入去重的 FIFO 就绪队列，executor 取出并 poll，使等待回复的 future 恢复。
每个 session 独立运行，持有 RX mbuf 跨 sleep，之后释放；timer 到期也通过 Waker 唤醒同一个 task。
B 用状态表完成相同流程，不经过 runtime。A/B 首次发送错开，随后均按回复后 sleep 的语义运行。

mbuf 由 Rust 唯一所有权和 Drop 管理；ICMP checksum 增量更新。
收发在核 2，后台统计在核 3，只接收数值，不执行 task 或操作 mbuf。
实现亮点：本线程唤醒只入预分配就绪队列；mbuf 安全跨 await；分桶与统计放在独立核，收发线程批量提交原始打点。

| 时刻/指标 | 定义 |
|---|---|
| T0 / T1 | A 的发送接口入口、B 决定发送时 / tx_burst 返回 |
| T2 / T3 | rx_burst 返回（整批共用）/ 回复交给 session |
| T4 / T5 / T0′ | 收到回复后等待的到期时刻 / 等待结束恢复执行 / 下一次请求的 T0 |
| T1−T0 | 发送处理时间 |
| T3−T2 | 接收交付时间 |
| (T1−T0)+(T3−T2) | 每个请求的两段之和，A−B 的排名指标 |
| T3−T0 | 往返时间；A/C 汇总使用，A/B 只在原始结果保留 |
| T0′−T4 | 等待到期至下次发送的延迟 |

各项均输出 p50/p90/p99/p99.9/p99.99/max 和样本数；C 只统计 T3−T0，来自 ping -U，没有内部 T1/T2 打点。
两段之和先逐包计算再统计分位数，不能把 T1−T0 和 T3−T2 的分位数直接相加。
使用 DPDK TSC 频率和 HDR 直方图。本机实测 TSC 时间差以 26 ticks 步进，频率 2.6GHz，对应约 10ns；写成 ns 单位不代表 1ns 测量分辨率。小于 262144ns 的 HDR 桶宽为 1ns，没有再按 10ns 舍入。

默认在 T1 后 **10ms** 超时，计入 timeout，保存请求身份；迟到回复单独对账，未收到的记为 missing。
超时不进入成功请求的延迟分布；退出额外收包 50ms 后停止网卡，检查 mbuf 回到初值。
收发对账错误或 mbuf 泄漏会报错退出；发生丢包仍须解释每个包，不能只凭程序退出成功认定通过。

## 运维与交付

重启后运行 `./scripts/prepare-host.sh` 恢复大页和网卡绑定。已有部署启用隔离：先运行 `./scripts/prepare-host.sh --isolate-on-reboot`，再重启。
系统进程默认使用核 0–1；核 2 收发、核 3 统计。启动参数为 `isolcpus=domain,managed_irq,2-3 nohz_full=2-3 rcu_nocbs=2-3 irqaffinity=0-1`。
脚本禁用 irqbalance，并将可迁移 IRQ、非绑定 workqueue 放到 0–1；启动隔离必须重启才生效，可用 `cat /sys/devices/system/cpu/isolated` 确认输出 `2-3`。
恢复内核网卡驱动用 `./scripts/restore-nic.sh`。更换网卡时修改以下位置：

| 位置 | 需要修改的配置 |
|---|---|
| `env.sh` | DPDK 网卡的 `BDF`、`SRC_IP`、`DPDK_IF`；内核网卡变化时改 `KERNEL_IF` |
| 上面的原始命令 | 同步改 `--bdf`、`--src-ip` 和 C 的 `-I`；原始命令不会读取 `env.sh` |
| `scripts/setup.sh` | 非 ENA 网卡需将 `-Denable_drivers=net/ena,mempool/ring` 中的 `net/ena` 改为对应 PMD，并重新构建 DPDK 和程序 |
| `scripts/restore-nic.sh` | 非 ENA 网卡需将 `echo ena` 改为对应内核驱动名 |

首次发现脚本面向 AWS 双 ENI；已有 `env.sh` 时不会自动重新发现。换对端还需改 `PEER_IP`、`PEER_MAC`，原始 A/B 命令显式传 `--peer-ip`、`--peer-mac`，C 改目标 IP。

禁用现成 runtime 检索：`grep -Enr 'tokio|async-std|smol|glommio|monoio|LocalPool|block_on' Cargo.lock src`，当前无匹配。
任务书 §9 提到的“§5 grep”没有给具体命令，这里检索其禁止的 runtime 依赖。

交付仓库链接或源码 zip，附 A/B、A/C 两个结果目录（运行日志和汇总报告）。答辩 30 分钟，包含上述 60 秒现场演示。
