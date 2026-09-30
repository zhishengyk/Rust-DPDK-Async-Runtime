# Runtime 延迟优化

基线提交：`512bd76`。本次修改仅涉及 rt 和 async-ping，B 的二进制与基线逐字节相同。
发送、解析、DPDK 配置、T0/T1/T2/T3 和有序 TSC 指令均未改动。所有延迟单位为 ns。

## 修改内容

原顺序：收包/分发/wake → 驱动维护 → timer/外部唤醒检查 → poll 就绪任务。
新顺序：收包/分发/wake → poll 就绪任务 → timer/外部唤醒检查 → 再次 poll → 驱动维护。
每个 poll 阶段最多 128 次，timer 唤醒在同圈处理；持续 self-wake 不会无限阻塞 I/O、timer 或维护。
reply 槽由 RefCell<Option<T>> 改为 Cell<Option<T>>，只转移所有权、不暴露内部引用；
交付时取出并消费 Waker，接收恢复时不再销毁这个已使用的 Waker。重复 reply 仍保留第一份值并退回新值。

这是调度顺序和所有权交接的优化。收包打点没有移动；timer、超时、重复包处理和驱动 watchdog 都保留。
设计稿 §4.2 原先要求先检查 timer；任务书要求 timer 功能，但未固定每轮的执行顺序。
ENA watchdog 的定时器由应用驱动，见 [DPDK ENA 文档](https://doc.dpdk.org/guides-23.11/nics/ena.html#supported-features)。

## 20 秒逐项试验

64 session、64B payload、delay 500us、timeout 10ms、核 2，各次顺序运行。

| 版本 | send p50/p99 | receive p50/p99 | process p50/p99 | timer p50/p99 |
|---|---:|---:|---:|---:|
| [原始 A](baseline-a.json) | 80 / 541 | 181 / 390 | 282 / 750 | 195 / 1218 |
| [B](baseline-b.json) | 80 / 541 | 110 / 270 | 202 / 670 | 135 / 1095 |
| [A：先 poll reply](ready-a.json) | 80 / 510 | 161 / 362 | 251 / 701 | 195 / 1328 |
| [A：再把维护移到轮末](priority-a.json) | 90 / 565 | 141 / 331 | 242 / 725 | 205 / 1353 |
| [A：再简化 reply 槽（采用）](cell-a.json) | 80 / 565 | 130 / 341 | 242 / 713 | 195 / 1205 |
| [A：额外强制内联（未采用）](inline-a.json) | 80 / 571 | 141 / 331 | 242 / 731 | 195 / 1144 |

强制内联没有显示额外收益，最终未保留该属性。各次分位数来自独立时间窗口，
网络负载及批次分布会变化，不能把每个候选相减视为精确的逐项指令成本。

## 第二轮短测确认

| 客户端 | process p50 | process p99 | receive p50 | receive p99 |
|---|---:|---:|---:|---:|
| [原始 A](confirm-runtime-baseline-a.json) | 270 | 750 | 181 | 381 |
| [优化 A](confirm-runtime-final-a.json) | 230 | 664 | 141 | 341 |
| [B](confirm-runtime-final-b.json) | 202 | 682 | 121 | 282 |

## 600 秒主测试

A/B 顺序各运行 600 秒；64 session、64B payload、delay 500us、timeout 10ms。

| 客户端/指标 | p50 | p90 | p99 | p99.9 | p99.99 | max | 样本数 |
|---|---:|---:|---:|---:|---:|---:|---:|
| A/process | 242 | 461 | 713 | 861 | 2116 | 54190 | 57961883 |
| A/receive | 141 | 242 | 341 | 442 | 1439 | 47910 | 57961883 |
| A/send | 80 | 301 | 541 | 670 | 811 | 54050 | 57961883 |
| A/timer | 193 | 676 | 1156 | 2411 | 10731 | 1635804 | 57961819 |
| A/sleep_error | 144 | 633 | 1107 | 2313 | 10633 | 1635734 | 57961883 |
| A/end_to_end | 160689 | 230007 | 245761 | 252062 | 264666 | 950336 | 57961883 |
| B/process | 202 | 411 | 670 | 811 | 1673 | 296402 | 58253973 |
| B/receive | 121 | 190 | 282 | 390 | 744 | 41330 | 58253973 |
| B/send | 80 | 291 | 541 | 664 | 775 | 296242 | 58253973 |
| B/timer | 144 | 639 | 1365 | 2584 | 10830 | 8947276 | 58253909 |
| B/sleep_error | 94 | 584 | 1304 | 2510 | 10830 | 8947086 | 58253973 |
| B/end_to_end | 160689 | 226856 | 245761 | 252062 | 277269 | 9028263 | 58253973 |

主排名 A−B：**p50 40ns，p99 43ns**。

这是两组进程内耗时分位数之差，不是逐样本配对差分，也不是端到端 RTT 差。
本轮未重测 C，不据此宣称比 C 更快。

## 对账与验证

| 运行 | TX | RX | timeout | late | missing | mbuf 初/末 |
|---|---:|---:|---:|---:|---:|---|
| [check-a-timeout](check-a-timeout.json) | 100 | 0 | 100 | 100 | 0 | 4095/4095 |
| [check-b-timeout](check-b-timeout.json) | 100 | 0 | 100 | 100 | 0 | 4095/4095 |
| [check-a-minimum](check-a-minimum.json) | 22566 | 22566 | 0 | 0 | 0 | 4095/4095 |
| [check-b-minimum](check-b-minimum.json) | 22637 | 22637 | 0 | 0 | 0 | 4095/4095 |
| [check-a-maximum](check-a-maximum.json) | 25248 | 25248 | 0 | 0 | 0 | 4095/4095 |
| [check-b-maximum](check-b-maximum.json) | 25151 | 25151 | 0 | 0 | 0 | 4095/4095 |
| [a-600](a-600.json) | 57961883 | 57961883 | 0 | 0 | 0 | 4095/4095 |
| [b-600](b-600.json) | 58253973 | 58253973 | 0 | 0 | 0 | 4095/4095 |

9 个单元测试、Clippy 和格式检查通过；Miri strict provenance 的 8 个测试通过。
新增用例覆盖 reply 在时钟检查/维护之前恢复，以及持续 self-wake 时 I/O、timer、维护不被饿死。
日志：[单元测试](tests.log)、[Clippy](clippy.log)、[Miri](miri.log)。
二进制：[基线摘要](baseline-binaries.sha256)、[最终摘要](final-binaries.sha256)。

直方图分位数误差约 ≤1.6%；max 来自精确 cycles。分位数不能直接相加。
边界用例各 1 秒：1 session/1us timeout/63B；1 session/delay 0/8B；128 session/delay 5000us/1472B。
