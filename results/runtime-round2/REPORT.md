# 第二轮 runtime 优化探索

基线为 `012752326c271a3d624e03dc4962210f427cae3a`。本轮未找到足够稳定的 A−B 改善，
**所有候选代码已撤回；撤回时已核对 release 的 A/B 二进制与基线逐字节相同**。
正式的 600 秒主排名仍为 [上一轮的 p50 40ns、p99 43ns](../runtime-optimization/REPORT.md)。
本轮短测不能证明已经达到性能极限，也不替代主排名。

## 试验内容

1. 就绪队列的字段改为 `Cell`，省去每次入队/出队的 `RefCell` 借用检查；保留 FIFO 和唤醒去重。
2. 在 1 的基础上，把跨线程唤醒的注册表/锁处理拆到独立的 cold 函数；保留跨线程和失效 Waker 支持。
3. 在 2 的基础上，让 `Sleep` 直接持有定时器槽的安全引用；减少取消 timer 的指针查找，并避免完成后重复 disarm。
4. 单独测试第 3 项的定时器修改，队列与 Waker 恢复基线。
5. 单独测试具体 future 类型的 runtime，A 用 `spawn_typed`，原 `spawn` 仍支持异构任务。
   反汇编显示 session future 被合并进 `poll_ready`，避免了它的虚函数表调用；实测没有相应的稳定延迟收益。

所有试验均保留 T0/T1/T2/T3、TSC fence、计时公式、收包批量、会话行为和 DPDK 配置。
B 的二进制未改变。未添加 unsafe，也未去掉超时、watchdog 或所有权保护。

## 首轮试验：每次 20 秒

64 session、payload 64B、delay 500us、timeout 10ms、核 2；客户端顺序独占同一 DPDK 网卡。
表内为 p50 / p99，单位 ns。

| 版本 | receive | process | send |
|---|---:|---:|---:|
| [基线 A](baseline-a.json) | 130 / 331 | 230 / 695 | 80 / 535 |
| [基线 B](baseline-b.json) | 110 / 270 | 202 / 682 | 80 / 553 |
| [1：Cell 队列](cell-a.json) | 130 / 341 | 230 / 750 | 80 / 590 |
| [2：再拆分跨线程唤醒](cold-a.json) | 130 / 341 | 242 / 781 | 80 / 615 |
| [3：再简化 timer 引用](timer-a.json) | 130 / 331 | 230 / 701 | 80 / 535 |
| [4：仅简化 timer 引用](timer-only-a.json) | 130 / 331 | 221 / 695 | 80 / 535 |
| [5：仅保留具体 future 类型](typed-a.json) | 130 / 331 | 242 / 713 | 80 / 565 |

前两项没有显示收益；组合版也没有降低接收段中位数。
timer-only 的 process p50 曾低 9ns，不能仅凭这一次短测认定有效，因此继续复测。

## 复测：每次 30 秒，正序再反序

以下按实际执行顺序列出，各轮参数相同，表内为 p50 / p99，单位 ns。

| 版本 | receive | process | send | timer |
|---|---:|---:|---:|---:|
| [基线 1](confirm-baseline-1.json) | 130 / 331 | 221 / 713 | 80 / 565 | 205 / 1255 |
| [timer-only 1](confirm-timer-only-1.json) | 130 / 322 | 221 / 695 | 80 / 535 | 193 / 1255 |
| [typed 1](confirm-typed-1.json) | 141 / 331 | 242 / 725 | 90 / 565 | 193 / 1144 |
| [typed 2](confirm-typed-2.json) | 141 / 331 | 242 / 713 | 80 / 541 | 193 / 1144 |
| [timer-only 2](confirm-timer-only-2.json) | 130 / 331 | 221 / 670 | 80 / 510 | 195 / 1316 |
| [基线 2](confirm-baseline-2.json) | 141 / 331 | 242 / 701 | 80 / 535 | 205 / 1181 |
| [B 对照](confirm-b.json) | 110 / 282 | 202 / 670 | 80 / 541 | 144 / 1365 |

全部指标、样本数、NIC 计数和 mbuf 对账保留在 JSON 中。

同一基线二进制，两轮 process p50 为 221/242ns，receive p50 为 130/141ns，说明当前实验条件下
确实存在与候选差值相当的波动。timer-only 的接收 p99 为 322/331ns，未持续低于基线 331ns；
其较低的 process p99 还伴随发送 p99 的下降，而它没有改发送代码。
typed 版本也没有低于基线的接收延迟。
因此本轮数据不足以把个位到十几纳秒的差异归因于修改，不从中挑一轮最好成绩作为新的 A−B。

## 复核与产物

- 共完成 14 轮短测、34,324,946 次成功往返；每轮 tx=rx，timeout、TX/分配失败、NIC 错误均为 0，
  mbuf 池结束时均恢复至 4095，无泄漏。
- 原始 JSON 和对应 `.log` 均保留；分位数直方图误差约 ≤1.6%，max 来自原始 cycle 最大值。
- 未采用的候选源码副本、补丁和临时二进制已清理；原始测量数据与日志保留。
- 试验时二进制的 SHA256 记录在本目录的 `*-binary.sha256` / `*-binaries.sha256` 中，供识别当时测量版本。
- typed 候选的 10 个 workspace 单元测试通过，包括借用数据跨 await、reply、超时与 sleep；
  rt 的 7 个 Miri strict provenance 测试通过。见 [测试日志](tests.log)和 [Miri 日志](typed-miri.log)。
- 这些测试用于检查候选正确性，不代表候选性能更好；本轮结束时源码和 release 二进制均已恢复基线。

若继续深入，值得另行测量 RX 批次大小与接收尾延迟的关系：A 先分发整批 reply 再 poll，
B 按包立即处理 session，这可能贡献部分尾延迟。它是尚未验证的方向，调整时也必须检查吞吐、timer 和对照公平性。
