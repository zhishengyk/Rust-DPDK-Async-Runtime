#!/usr/bin/env python3
"""Turn the retained JSON measurements into the repository's latency report."""
import json
import pathlib
import sys
from datetime import datetime, timezone

root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else 'results')
def read(name):
    return json.loads((root / f'{name}.json').read_text())
a, b, low, c = map(read, ['a-600', 'b-600', 'a-lowload', 'c'])
quantiles = ['p50', 'p90', 'p99', 'p999', 'p9999', 'max']
date = datetime.now(timezone.utc).strftime('%Y-%m-%d')
lines = ['# 实机延迟报告', '', f'报告生成日期：{date} UTC。所有延迟单位为 **ns**。', '',
         '环境：AWS c8a.xlarge，AMD EPYC 9R45，4 物理核；Amazon Linux 2023；',
         'Rust 1.90.0 / DPDK 23.11.5 / ENA / vfio-pci noiommu / 2MB hugepages。',
         'runtime 固定核 2，已迁移可移动 IRQ；本次未重启启用 isolcpus。',
         '完整环境和二进制 SHA256 见 [environment.txt](environment.txt)。', '',
         '## 10 分钟主测试', '',
         'A/B 顺序运行，各 `--delay-us 500 --duration-sec 600 --sessions 64 --payload 64`。',
         '超时 10ms；两者共用帧模板、checksum、解析、T0/T1/T2 和直方图实现。', '',
         '| 客户端 | 实测秒数 | TX | RX | timeout | late | missing | mbuf 初/末 |',
         '|---|---:|---:|---:|---:|---:|---:|---|']
for name, r in [('A', a), ('B', b)]:
    counts, pool = r['counters'], r['mempool']
    lines.append(f"| {name} | {r['elapsed_sec']:.6f} | {counts['tx']} | {counts['rx']} | {counts['timeout']} | {counts['late_reply']} | {r['missing']} | {pool['initial']}/{pool['final']} |")
lines += ['', '主排名：A−B **进程内耗时分位数之差**（非逐样本配对差分）：', '',
          '| p50 | p90 | p99 | p99.9 | p99.99 | max |', '|---:|---:|---:|---:|---:|---:|',
          '| ' + ' | '.join(str(a['latency']['process'][q] - b['latency']['process'][q]) for q in quantiles) + ' |', '',
          '所有指标如下；process 为发送和接收两段之和，sleep 不在该指标或端到端 RTT 中。', '',
          '| 客户端/指标 | p50 | p90 | p99 | p99.9 | p99.99 | max | 样本数 |',
          '|---|---:|---:|---:|---:|---:|---:|---:|']
for name, r in [('A', a), ('B', b)]:
    for metric in ['process', 'end_to_end', 'timer', 'sleep_error', 'send', 'receive']:
        h = r['latency'][metric]
        lines.append(f'| {name}/{metric} | ' + ' | '.join(str(h[q]) for q in quantiles) + f" | {h['count']} |")
lines += ['', '直方图分位数取桶上界，误差约 ≤1.6%；max 是精确的原始 cycle 最大值换算。',
          '差值来自两次顺序运行，包含批次大小、系统调度、虚拟化和驱动回收时机的变化，',
          '不能把所有尾部差值都归因于 Waker。receive 指标体现主要 async 交接/调度成本。', '',
          '## A/C 同负载端到端参照', '',
          'A：64 session、delay 64000us、60s；C：系统 ping -U、单流、1ms 间隔、60s。',
          '两者 payload 64B、同一对端；使用各自指定 ENI。', '',
          '| 客户端 | 接收 pps | p50 | p90 | p99 | p99.9 | p99.99 | max | 样本数 |',
          '|---|---:|---:|---:|---:|---:|---:|---:|---:|']
for name, r, rate in [('A/低负载', low, low['counters']['rx']/low['elapsed_sec']), ('C', c, c['packets_per_sec'])]:
    h = r['latency']['end_to_end']
    lines.append(f'| {name} | {rate:.2f} | ' + ' | '.join(str(h[q]) for q in quantiles) + f" | {h['count']} |")
lines += ['', 'A−C 端到端差：' + '，'.join(f"{q} = {low['latency']['end_to_end'][q] - c['latency']['end_to_end'][q]}ns" for q in ['p50', 'p99']) + '。',
          '正数表示这一条件下 A 更慢，负数表示 A 更快。该结果不参与 A/B 主排名。',
          'iputils 把亚毫秒 `-i` 截为零，故没有采用设计稿的 0.0005；C 的文本 RTT 有微秒级舍入。',
          '`-U` 在用户态 recvmsg 后打点；默认内核 SO_TIMESTAMP 不包含用户态接收部分。',
          '负载仅近似匹配，仍存在两张 ENI、内核与用户态调度、相位以及发送策略的差异。', '',
          '## 可复核产物', '',
          '- [A 600s JSON](a-600.json) / [日志](a-600.log)',
          '- [B 600s JSON](b-600.json) / [日志](b-600.log)',
          '- [A 60s 演练](a-60.json) / [B 60s 演练](b-60.json)',
          '- [A 低负载](a-lowload.json) / [C 报表](c.json) / [C 原始日志](c.log.gz)',
          '- [单元测试](tests.log) / [Clippy](clippy.log) / [Miri](miri.log)', '',
          '每份 JSON 保留完整 counters、NIC stats、逐个超时的 session/seq/T0/late、mbuf 对账。',
          'timeout 在报表中算 loss；late 只表示在最终 drain 结束前已观测到，missing 不虚构原因。', '']
if low['latency']['end_to_end']['p50'] >= c['latency']['end_to_end']['p50']:
    lines += ['本次低负载对照未观察到 kernel bypass 的端到端 p50 收益；主排名仍是上面的 A−B 进程内差值。', '']
if (root / 'c-kernel-timestamp.json').exists():
    lines += ['另保留 [默认内核时间戳的 C](c-kernel-timestamp.json) / [原始日志](c-kernel-timestamp.log.gz)，',
              '用于复核计时口径差异；该轮不参与最终 A/C 差值。', '']
if (root / 'a-timeout.json').exists():
    lines += ['## 边界与超时验证', '',
              '分别对 A/B 跑 1 秒：单 session、1µs timeout、63B 奇数 payload；',
              '单 session、delay=0、8B 最小 payload；128 session、1472B 最大 payload。', '',
              '| 测试 | TX | RX | timeout | late | missing | mbuf 初/末 |',
              '|---|---:|---:|---:|---:|---:|---|']
    for name in ['a-timeout', 'b-timeout', 'a-minimum', 'b-minimum', 'a-maximum', 'b-maximum']:
        r = read(name)
        counts, pool = r['counters'], r['mempool']
        lines.append(f"| [{name}]({name}.json) | {counts['tx']} | {counts['rx']} | {counts['timeout']} | {counts['late_reply']} | {r['missing']} | {pool['initial']}/{pool['final']} |")
    lines += ['', '强制超时用来验证 late 对账，不混入正常主测试的统计。', '']
(root / 'REPORT.md').write_text('\n'.join(lines))
