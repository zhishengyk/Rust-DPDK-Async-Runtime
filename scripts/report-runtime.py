#!/usr/bin/env python3
"""Summarize retained runtime experiments, including the background 600s runs."""
import json
from pathlib import Path

root = Path(__file__).resolve().parent.parent / 'results/runtime-optimization'
quantiles = ['p50', 'p90', 'p99', 'p999', 'p9999', 'max']

def read(name):
    return json.loads((root / f'{name}.json').read_text())

lines = [
    '# Runtime 延迟优化', '',
    '基线提交：`512bd76`。本次修改仅涉及 rt 和 async-ping，B 的二进制与基线逐字节相同。',
    '发送、解析、DPDK 配置、T0/T1/T2/T3 和有序 TSC 指令均未改动。所有延迟单位为 ns。', '',
    '## 修改内容', '',
    '原顺序：收包/分发/wake → 驱动维护 → timer/外部唤醒检查 → poll 就绪任务。',
    '新顺序：收包/分发/wake → poll 就绪任务 → timer/外部唤醒检查 → 再次 poll → 驱动维护。',
    '每个 poll 阶段最多 128 次，timer 唤醒在同圈处理；持续 self-wake 不会无限阻塞 I/O、timer 或维护。',
    'reply 槽由 RefCell<Option<T>> 改为 Cell<Option<T>>，只转移所有权、不暴露内部引用；',
    '交付时取出并消费 Waker，接收恢复时不再销毁这个已使用的 Waker。重复 reply 仍保留第一份值并退回新值。', '',
    '这是调度顺序和所有权交接的优化。收包打点没有移动；timer、超时、重复包处理和驱动 watchdog 都保留。',
    '设计稿 §4.2 原先要求先检查 timer；任务书要求 timer 功能，但未固定每轮的执行顺序。',
    'ENA watchdog 的定时器由应用驱动，见 [DPDK ENA 文档](https://doc.dpdk.org/guides-23.11/nics/ena.html#supported-features)。', '',
    '## 20 秒逐项试验', '',
    '64 session、64B payload、delay 500us、timeout 10ms、核 2，各次顺序运行。', '',
    '| 版本 | send p50/p99 | receive p50/p99 | process p50/p99 | timer p50/p99 |',
    '|---|---:|---:|---:|---:|',
]
variants = [
    ('baseline-a', '原始 A'), ('baseline-b', 'B'), ('ready-a', 'A：先 poll reply'),
    ('priority-a', 'A：再把维护移到轮末'), ('cell-a', 'A：再简化 reply 槽（采用）'),
    ('inline-a', 'A：额外强制内联（未采用）'),
]
for name, label in variants:
    r = read(name)
    cells = [f"{r['latency'][m]['p50']} / {r['latency'][m]['p99']}"
             for m in ['send', 'receive', 'process', 'timer']]
    lines.append(f'| [{label}]({name}.json) | ' + ' | '.join(cells) + ' |')
lines += ['', '强制内联没有显示额外收益，最终未保留该属性。各次分位数来自独立时间窗口，',
          '网络负载及批次分布会变化，不能把每个候选相减视为精确的逐项指令成本。', '',
          '## 第二轮短测确认', '',
          '| 客户端 | process p50 | process p99 | receive p50 | receive p99 |',
          '|---|---:|---:|---:|---:|']
for name, label in [('confirm-runtime-baseline-a', '原始 A'),
                    ('confirm-runtime-final-a', '优化 A'), ('confirm-runtime-final-b', 'B')]:
    if (root / f'{name}.json').exists():
        r = read(name)['latency']
        cells = [str(r[m][q]) for m in ['process', 'receive'] for q in ['p50', 'p99']]
        lines.append(f'| [{label}]({name}.json) | ' + ' | '.join(cells) + ' |')
lines += ['', '## 600 秒主测试', '']
if all((root / f'{name}-600.json').exists() for name in ['a', 'b']):
    a, b = read('a-600'), read('b-600')
    lines += ['A/B 顺序各运行 600 秒；64 session、64B payload、delay 500us、timeout 10ms。', '',
              '| 客户端/指标 | p50 | p90 | p99 | p99.9 | p99.99 | max | 样本数 |',
              '|---|---:|---:|---:|---:|---:|---:|---:|']
    for label, r in [('A', a), ('B', b)]:
        for metric in ['process', 'receive', 'send', 'timer', 'sleep_error', 'end_to_end']:
            h = r['latency'][metric]
            lines.append(f'| {label}/{metric} | ' + ' | '.join(str(h[q]) for q in quantiles)
                         + f" | {h['count']} |")
    delta = {q: a['latency']['process'][q] - b['latency']['process'][q] for q in ['p50', 'p99']}
    lines += ['', f"主排名 A−B：**p50 {delta['p50']}ns，p99 {delta['p99']}ns**。", '',
              '这是两组进程内耗时分位数之差，不是逐样本配对差分，也不是端到端 RTT 差。',
              '本轮未重测 C，不据此宣称比 C 更快。', '']
else:
    lines += ['**后台测试尚未全部完成，最终 A−B 暂不下结论。**',
              'A/B 各需 10 分钟并顺序独占网卡；完成后后台脚本自动更新本报告。', '']
lines += ['## 对账与验证', '',
          '| 运行 | TX | RX | timeout | late | missing | mbuf 初/末 |',
          '|---|---:|---:|---:|---:|---:|---|']
for name in ['check-a-timeout', 'check-b-timeout', 'check-a-minimum', 'check-b-minimum',
             'check-a-maximum', 'check-b-maximum', 'a-600', 'b-600']:
    if not (root / f'{name}.json').exists():
        continue
    r = read(name)
    c, p = r['counters'], r['mempool']
    lines.append(f"| [{name}]({name}.json) | {c['tx']} | {c['rx']} | {c['timeout']} | "
                 f"{c['late_reply']} | {r['missing']} | {p['initial']}/{p['final']} |")
lines += ['', '9 个单元测试、Clippy 和格式检查通过；Miri strict provenance 的 8 个测试通过。',
          '新增用例覆盖 reply 在时钟检查/维护之前恢复，以及持续 self-wake 时 I/O、timer、维护不被饿死。',
          '日志：[单元测试](tests.log)、[Clippy](clippy.log)、[Miri](miri.log)。',
          '二进制：[基线摘要](baseline-binaries.sha256)、[最终摘要](final-binaries.sha256)。', '',
          '直方图分位数误差约 ≤1.6%；max 来自精确 cycles。分位数不能直接相加。',
          '边界用例各 1 秒：1 session/1us timeout/63B；1 session/delay 0/8B；128 session/delay 5000us/1472B。', '']
(root / 'REPORT.md').write_text('\n'.join(lines))
