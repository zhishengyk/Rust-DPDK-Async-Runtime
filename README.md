# Rust DPDK Async Runtime

单核 poll-mode Rust runtime，以及共用收发代码的 ICMP 客户端 A (`async-ping`) / B (`raw-ping`)。
C 使用系统 `ping`。没有现成 executor、reactor 或网络栈。

## 快速运行

开发环境：Amazon Linux 2023、x86_64、两张 ENA、passwordless sudo。
固定 **Rust 1.90.0 / DPDK 23.11.5**，DPDK 静态链接，启用 ENA 和 ring mempool 驱动。

```bash
./scripts/setup.sh
./scripts/run-a.sh --delay-us 500 --duration-sec 60
./scripts/run-b.sh --delay-us 500 --duration-sec 60

# 默认都是 64 session / payload 64 / delay 500us / 持续 600s
./scripts/run-a.sh
./scripts/run-b.sh

# 顺序完成 60s 演练、A/B 各 600s、可比低负载 A/C、输出差值
./scripts/bench.sh results/local
```

`setup.sh` 编译工具链和 DPDK、从 IMDSv2 识别 device-number 非 0 的 ENI，保存
`env.sh`，分配 1024 个 2MB hugepage，再将该 ENI 绑定到 vfio-pci noiommu。
device-number 0 留给 SSH/C。脚本不会重启。
本机配置：主网卡 `enp39s0 / 10.202.11.18`，DPDK 网卡
`0000:28:00.0 / 10.202.15.210`；对端 `10.202.8.15 / 06:ff:fd:b6:f0:cd`。

核 2 跑 A/B；0–1 跑 OS/C，3 备用。EAL 固定 runtime 到核 2，脚本迁走可移动 IRQ。
需要启动级隔离时执行 `./scripts/prepare-host.sh --isolate-on-reboot`，下次手动重启才生效。
默认实测采用无需重启的 IRQ 亲和性方案，**不宣称已启用 isolcpus**。
重启后先执行 `./scripts/prepare-host.sh` 恢复大页挂载和绑定。
恢复内核驱动用 `./scripts/restore-nic.sh`。
A/B 使用同一个端口，须顺序运行。

直接运行二进制：

```bash
source env.sh
sudo target/release/async-ping --bdf "$BDF" --src-ip "$SRC_IP" \
  --delay-us 500 --duration-sec 60 --output results/local/demo.json
```

两个客户端参数相同：必填 `--delay-us`、`--duration-sec`、`--src-ip`、`--bdf`；
可选 `--sessions`（1–128，默认 64）、`--payload`（8–1472，默认 64）、
`--timeout-us`（默认 10000）、`--core`（默认 2）、`--peer-ip`、`--peer-mac`、`--output`。
脚本提供源 IP/BDF。持续时间从所有启动工作完成后开始计算；到期停止新请求，
完成最后的 reply/timeout 和 sleep，再接收 50ms 迟到包，停止端口、清点 mbuf、输出统计。
因此墙钟时间还包含 EAL 初始化、500ms TSC 校准和至多一次 timeout/sleep 的收尾。

## 架构

```text
async-ping -> rt (std only)
    |          executor / Waker / timers / Slot<T>
    v
ping-common -> wire / metrics / dpdk -> dpdk-sys -> ENA
    ^
raw-ping (手写状态表，不依赖 rt)
```

`rx_burst` 返回的一批 mbuf 先获得同一个 T2。共用的 `dispatch` 解析帧，
通过 ICMP id 找 session，核对 seq 与 payload 的 T0，再把唯一所有权的 `Reply`
移进该 session 的 `Slot`。Waker 去重后把 task id 放入定长就绪 ring；
executor 在这一轮调用 future 的 `poll`，`recv().await` 返回，task 第一行记录 T3。
runtime 提供的就是 **所有权交接、唤醒、调度、超时和持有 mbuf 期间的异步 sleep**。
当前循环先 poll 收包唤醒的任务，再检查 timer/跨线程唤醒并再次 poll，最后执行驱动维护。
这调整了设计稿 §4.2 的顺序，让已经到达的 reply 优先交给 session；T0–T3 的位置不变。

A 的每个 session 是独立 async task：

```rust,ignore
send(...).await;
let reply = slot.recv(&mut handle, deadline).await;
let t3 = now();
handle.sleep(delay).await; // reply 的 mbuf 仍由这个 task 持有
record(reply);
```

B 按相同语义实现 `Waiting / Sleeping / Done` 状态表。
两者首发均按 `sid * delay / sessions` 错开；成功、超时、发送失败后都等待同样的 delay。
常量 IPv4 头在启动时计算 checksum；发送仅更新 seq、TSC 和五个变化半字的 ICMP checksum。
ARP 请求原地改为应答；不实现 ARP 缓存、路由、分片或 IPv4 options。

`rt` 是独立 library crate，时钟函数和 reactor 闭包由调用方提供：

- task 只在启动时 `Box::pin`，固定容量 128，就绪 ring 按 task 去重。
- 每 task 一个 timer 槽，缓存最早 deadline；只有到期才扫描。
  `Handle` 的可变借用限制同一 task 同时只能持有一个 sleep/receive timer。
- Waker 的 data 是永不复用的整数标识，不解引用、不引用计数。
  本线程走 TLS + ready ring；跨线程走全局注册表 + 原子位图。
  runtime 销毁会注销标识，旧 Waker 失效，不会访问已释放内存或唤醒新 runtime。
- 正常线程内唤醒无分配、无锁、无原子 RMW。每圈仍有两次 relaxed load 检查外部唤醒。
  两个 poll 阶段各最多 128 次，持续 self-wake 的 future 也不会饿死 reactor、timer 或维护。
- reply 槽用 `Cell<Option<T>>` 直接转移所有权；交付时取出并消费 Waker，
  不保留内部借用，也不让接收 future 在恢复时再次销毁同一个 Waker。
- runtime/DPDK/mbuf 都不能 Send/Sync；Waker 遵守标准库 Send/Sync 契约。
  `RefCell` 只做局部借用，绝不持有 guard 跨 await。

## 最小实现与设计文档的差异

发送模板是普通字节数组，每次从 DPDK mempool 分配 mbuf、复制帧并交给 PMD，
不永久提高模板 mbuf 的引用计数。这样超时重发也不用证明上一包 DMA 已完成。
这是 mempool 操作，不是每包系统堆分配；A/B 使用完全相同的发送函数。
内存池启用 128 个 mbuf 的本核缓存；ENA 的 TX 回收阈值设为描述符数减 16，
让发送完成回收分成较小批次，降低单次发送的尾延迟。
`Mbuf::Drop` 归还包，`Rc<Pool>` 保证 pool/EAL 比所有包活得更久。
TX 失败立即释放，计入 `tx_failed`，下一次仍遵守 delay；不加无限重试或恢复框架。

timer/task 槽不在一次运行内回收复用；固定启动任务已满足 demo。
直方图、就绪队列、包批次固定容量；只有异常路径的逐包丢失记录可能增长。
协议和两个 bin 禁止 unsafe。unsafe 限于 FFI/mbuf 封装、Waker vtable、TSC 指令。

TSC 使用 `LFENCE; RDTSC; LFENCE` 和编译器内存屏障，避免把几十纳秒的指标建立在
未排序的指令上。C shim 与 Rust 使用同一序列；A/B 同样承担打点开销。
ENA watchdog 的 `rte_timer_manage` 每毫秒调用一次；不另建后台上报线程。
`Runtime::run_with_maintenance` 在两轮任务 poll 后执行维护闭包；普通 `run` 不需要此闭包。

## 测量口径与报表

| 时刻 | A/B 共同语义 |
|---|---|
| T0 | 共用 `Shared::send` 入口，分配/patch/copy 之前 |
| T1 | C shim 中 `rte_eth_tx_burst` 返回后立刻打点 |
| T2 | C shim 中 `rte_eth_rx_burst` 返回后立刻打点，整批共用 |
| T3 | A 从 recv await 返回的第一行；B 把 reply 交到 session 状态机后 |

`process = (T1-T0) + (T3-T2)`；`end_to_end = T3-T0`。
`timer = 下一个 T0 - sleep deadline`，包含唤醒、记录上一样本和释放 mbuf；
`sleep_error = sleep 恢复时刻 - deadline`。
首发 stagger 不记 sleep 指标，最后一次 sleep 没有下一个 T0，因此 timer 样本略少。
另外单独报 `send` / `receive`，方便定位开销。
所有指标给出 p50/p90/p99/p99.9/p99.99/max/count，单位 ns。
直方图保存原始 cycles，分位数取桶上界，误差小于约 1.6%；max 精确保存。
报表阶段才换算 ns。TSC 频率通过 `Instant` 与 500ms 校准得到。

默认超时是 **T1 后 10ms**。超时算 loss，不进入成功 RTT 直方图；
以 `(session, seq, T0)` 记录每个超时，避免 16 位 seq 回绕误认旧包。
超时后的匹配 reply 标记 `late`，只对账一次；`missing = timeout - late_reply`。
超过退出后的 50ms drain 才来的包无法观测，不把它声称为已解释。
无回复的样本只说明“截止到 deadline/drain 没收到”，不凭空推断网络丢失原因。
另报重复包、杂包、ARP、TX 分配/提交失败和 NIC error/missed/rx_nombuf。
对账公式是 `tx = rx + timeout`；未被 TX 接受的包不算已发送。

mbuf 基线在创建 pool 后、配置 RX 描述符前取 **4095**；最终先释放任务持包、停止并关闭
端口，让驱动归还 RX/TX 描述符，再读取可用数。两个时刻包含相同资源集合。
计数不一致或 mbuf 未归还使程序退出失败，不隐藏成成功结果。

```bash
python3 scripts/compare.py results/a-600.json results/b-600.json
```

这里只计算 **A 分位数减 B 分位数**，不是逐样本配对差分。
优化前的结果见 [原始实测报告](results/REPORT.md)，本次配置调整及对照结果见
[延迟优化报告](results/optimization/REPORT.md)；原始终端日志及完整 JSON 一并保留。
后续 runtime 调度与 reply 槽优化见 [runtime 优化报告](results/runtime-optimization/REPORT.md)。

## C 的可比性

本机 iputils 20210202 的 `-i` 以整数毫秒存储，设计稿的 `-i 0.0005` 实际会变成 0。
因此 C 使用 `ping -n -U -I enp39s0 -i 0.001 -s 64 -w 60 -W 1 10.202.8.15`，固定到核 0–1。
`-U` 使用用户态收包后的时间；默认 SO_TIMESTAMP 是内核收包时间，少算了用户态接收部分。
脚本仅解析系统 ping 输出，没有自行实现第三个 ICMP 客户端。

对 C 另外测一组 A：64 session、`--delay-us 64000 --duration-sec 60`，总速率约
1000pps，和 C 的 1ms 间隔匹配。同 payload、同对端、同子网/AZ，各自走指定 ENI。
报告列出实际 pps 核对负载，并只在这组条件下比较 A/C 的端到端指标。
500us 的 A/B 主排名不拿来与低负载 C 做直接减法。
仍有 ENI、内核调度、固定发送间隔与 reply 后等待、ping 文本精度的差异；
A−C 是这些条件下的端到端观察差，不是纯 syscall 成本。
不把“低负载”当作已证明对端永不排队，也不预设 kernel bypass 必然更快。

```bash
./scripts/run-a.sh --delay-us 64000 --duration-sec 60 --output results/local/a-lowload.json
./scripts/run-c.sh 60 results/local/c
```

## 验证、换卡与运维

```bash
source "$HOME/.cargo/env"
export PKG_CONFIG_PATH=/opt/dpdk/lib64/pkgconfig
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

# 不依赖 DPDK 的时钟注入/runtime/协议/直方图测试
cargo test -p rt -p wire -p metrics

# 可选：不涉及 FFI 的严格 provenance 内存模型检查
rustup toolchain install nightly-2025-09-18 --profile minimal --component miri
MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly-2025-09-18 miri test -p rt -p wire --locked

# 任务书“grep”要求（原文 §5 没有给命令，按禁用依赖列表自证）
grep -rEn 'tokio|async-std|smol|glommio|monoio|LocalPool|block_on' Cargo.lock crates bins
# 实测：无输出，退出码 1。
```

测试涵盖增量 checksum、奇数 payload、截断帧/fragment、ARP、直方图、timer、
reply/timeout、重复唤醒、跨线程唤醒、runtime 销毁后的 Waker。
网卡和 10 分钟验证见实测报告。

换 ENI：恢复旧卡，删除或修改 `env.sh` 的 BDF/SRC_IP/DPDK_IF，再运行 prepare-host。
本机 MAC 自动读取；对端地址在 env.sh。换非 ENA 卡还要修改 setup.sh 的 `enable_drivers`；
队列深度 512、单端口/单队列/offload=0 集中在 `crates/dpdk-sys/shim.c::w_port_start`。
大于 MTU 的 payload、不可信网络中的完整协议校验、多核、热插拔、故障重连均不在本项目范围。

参考：[DPDK ENA 文档](https://doc.dpdk.org/guides-23.11/nics/ena.html)、
[iputils interval 解析](https://github.com/iputils/iputils/blob/20210202/ping/ping.c)、
[iputils 收包计时](https://github.com/iputils/iputils/blob/20210202/ping/ping_common.c)。
