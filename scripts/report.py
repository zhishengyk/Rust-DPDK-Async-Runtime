#!/usr/bin/env python3
"""Summarize A/B/C measurements or stream raw timestamp events as CSV."""
import argparse
import collections
import csv
import gzip
import itertools
import json
import math
import re
import struct
import sys
from decimal import Decimal
from pathlib import Path


def summarize(mode, directory):
    root = Path(directory)
    quantiles = ('p50', 'p90', 'p99', 'p999', 'p9999', 'max')

    def read(name):
        return json.loads((root / f'{name}.json').read_text())

    def table(headers, rows):
        return ['| ' + ' | '.join(map(str, row)) + ' |'
                for row in [headers, ['---'] * len(headers), *rows]]

    names = ('a-600', 'b-600') if mode == 'ab' else ('a', 'c')
    runs = [(name, read(name)) for name in names]
    lines = [f'# {"A/B" if mode == "ab" else "A/C"} 测量汇总', '',
             ('64 session、64B payload、收到回复后等待 500µs；A/B 各 600 秒。'
              if mode == 'ab' else '单会话、64B payload、同一对端、客户端均在核 2，各 60 秒；A delay 950µs，C 间隔 1ms。'),
             '', '## 收发与完整性', '']
    rows = []
    for name, d in runs:
        c = d.get('counters', d)
        pool = d.get('mempool')
        rows.append([name, f"{d['elapsed_sec']:.3f}", c['tx'], c['rx'],
                     f"{c['tx'] / d['elapsed_sec']:.2f}", c.get('timeout', c.get('loss', 0)),
                     c.get('late_reply', '—'), d.get('missing', '—'),
                     f"{pool['initial']}/{pool['final']}" if pool else '—'])
        if pool:
            lines.append(f"{name}：TX 提交失败 {c['tx_failed']}，分配失败 {c['alloc_failed']}，ARP 提交失败 {c['arp_tx_failed']}。")
            if c["timeout"]:
                lines.append(f"{name}：{c['timeout']} 次请求超过设定超时，按超时丢包计数；其中 {c['late_reply']} 个回复随后匹配到原请求。每个请求身份和迟到状态见 losses；这些记录不能单独确定延迟发生在本机、网络还是对端。")
    lines += ['', *table(['运行', '秒', 'TX', 'RX', 'TX/秒', '超时/丢包', '迟到', '未收到', 'mbuf 初/末'], rows)]
    labels = {'t1_t0': 'T1−T0', 't3_t2': 'T3−T2', 'process': '(T1−T0)+(T3−T2)',
              'timer': 'T0′−T4', 'end_to_end': 'T3−T0', 'sleep_error': 'T5−T4'}
    metrics = ('t1_t0', 't3_t2', 'process', 'end_to_end', 'timer', 'sleep_error') if mode == 'ab' else ('end_to_end',)
    lines += ['', '## 延迟（ns）', '']
    rows = [[f'{name}/{labels[metric]}', *[d['latency'][metric][q] for q in quantiles],
             d['latency'][metric]['count']] for name, d in runs for metric in metrics]
    lines += table(['指标', 'p50', 'p90', 'p99', 'p99.9', 'p99.99', 'max', '样本数'], rows)
    if mode == 'ab':
        a, b = (d for _, d in runs[-2:])
        delta = {q: a['latency']['process'][q] - b['latency']['process'][q] for q in ('p50', 'p99')}
        lines += ['', f"A−B 的 (T1−T0)+(T3−T2)：p50 **{delta['p50']}ns**，p99 **{delta['p99']}ns**。",
                  '先对每个请求计算两段之和，再统计分位数；不能把两段的分位数直接相加。A−B 是独立运行的分位数之差。']
    else:
        a, c = (d for _, d in runs)
        rate_a, rate_c = a['counters']['tx'] / a['elapsed_sec'], c['tx'] / c['elapsed_sec']
        gap = abs(rate_a / rate_c - 1) if rate_c else float('inf')
        lines += ['', f'实际发包速率差异：{gap:.3%}。']
        if gap <= .01:
            delta = {q: c['latency']['end_to_end'][q] - a['latency']['end_to_end'][q] for q in ('p50', 'p99')}
            lines += [f"C−A 的 T3−T0：p50 **{delta['p50']}ns**，p99 **{delta['p99']}ns**；正数表示 A 更低。"]
        else:
            lines += ['速率差异超过 1%，本轮负载未匹配，需重新校准 A 的 delay，不计算收益。']
        lines += ['结论仅适用于此单会话负载；两张 ENI、发送策略及计时边界不同，不能外推到 64 session。']
    lines += ['', '## 时间戳说明', '',
              'T0：A 进入发送接口、B 判定本次应发送；T1：tx_burst 返回；',
              'T2：rx_burst 返回，同一批报文共用；T3：回复交给对应 session。']
    if mode == 'ab':
        lines += ['T4：收到回复后等待的到期时刻；T5：等待结束、会话恢复执行的时刻；T0′：下一次请求的 T0。',
                  'T1−T0 为发送处理时间，T3−T2 为接收交付时间；T0′−T4 为到期至下次发送的延迟。',
                  'T3−T0 为端到端延迟；T5−T4 为 sleep 恢复时刻相对 deadline 的误差。',
                  '60 秒演示不列入本汇总，独立日志及 JSON 保留完整测量。']
    else:
        lines += ['T3−T0 为一次请求至回复交付的往返时间，不含两次请求间的等待。',
                  'C 的 T3−T0 来自 ping -U 的用户态往返时间；C 没有 A 的内部 T1、T2 打点。']
    lines += ['超时请求不进入成功请求的延迟分布；请求身份和迟到状态见对应 JSON 的 losses。']
    if any(d.get('missing', d.get('loss', 0)) for _, d in runs):
        lines += ['存在未收到的回复，须解释每个丢包，当前结果不能直接认定通过完整性门槛。']
    if any('mempool' in d and (not d['mempool']['leak_free'] or not d['accounted']) for _, d in runs):
        lines += ['mbuf 或收发对账失败，未通过完整性门槛。']
    output = root / 'SUMMARY.md'
    output.write_text('\n'.join(lines) + '\n', encoding='utf-8')
    print(output)


def report_ping(source, destination, duration):
    hist = collections.Counter()
    transmitted = received = None
    with open(source) as log:
        for line in log:
            match = re.search(r'time=([0-9.]+) ms', line)
            if match:
                hist[int(Decimal(match[1]) * 1_000_000)] += 1
            match = re.search(r'(\d+) packets transmitted, (\d+) received', line)
            if match:
                transmitted, received = map(int, match.groups())
    count = sum(hist.values())
    # 按样本计数求最近秩分位数，p 取 0～1；返回已换算为 ns 的系统 ping RTT。
    def percentile(p):
        target = math.ceil(count * p)
        total = 0
        for value, n in sorted(hist.items()):
            total += n
            if total >= target:
                return value

    latency = dict(count=count, p50=percentile(.5), p90=percentile(.9), p99=percentile(.99),
                   p999=percentile(.999), p9999=percentile(.9999), max=max(hist))
    report = dict(client='C', units='ns', elapsed_sec=int(duration), tx=transmitted, rx=received,
                  loss=transmitted-received, packets_per_sec=count/int(duration),
                  latency=dict(end_to_end=latency), interval_ms=1, payload=64, sessions=1,
                  receive_timestamp='userspace (-U)')
    with open(destination, 'w') as out:
        json.dump(report, out, indent=2)
    print(json.dumps(report, indent=2))


def read_trace(source, limit=None):
    open_trace = gzip.open if source.endswith('.gz') else open
    with open_trace(source, 'rb') as trace:
        header = trace.read(64)
        if len(header) != 64 or header[:8] != b'DPDKTS01':
            raise SystemExit('Invalid trace header')
        print('tsc_hz=' + str(struct.unpack_from('<Q', header, 8)[0]), file=sys.stderr)
        rows = csv.writer(sys.stdout)
        rows.writerow(['kind', 'session', 'seq', 'T0', 'T1', 'T2', 'T3', 'T4', 'T5'])
        try:
            for record in itertools.islice(iter(lambda: trace.read(64), b''), limit):
                if len(record) != 64:
                    raise SystemExit('Truncated trace record')
                kind, identity, *stamps = struct.unpack('<8Q', record)
                rows.writerow([kind, identity >> 32, identity & 0xffffffff, *stamps])
        except BrokenPipeError:
            sys.exit(0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest="mode", required=True)
    for mode in ("ab", "ac"):
        command = modes.add_parser(mode, help=f"Summarize an {mode.upper()} result directory")
        command.add_argument("directory")
    command = modes.add_parser("c", help="Convert a system ping log to JSON")
    command.add_argument("source")
    command.add_argument("destination")
    command.add_argument("duration", type=int)
    command = modes.add_parser("trace", help="Stream raw .ticks or .ticks.gz as CSV")
    command.add_argument("source")
    command.add_argument("--limit", type=int)
    args = parser.parse_args()
    if args.mode in ("ab", "ac"):
        summarize(args.mode, args.directory)
    elif args.mode == "c":
        if args.duration <= 0:
            parser.error("duration must be positive")
        report_ping(args.source, args.destination, args.duration)
    else:
        if args.limit is not None and args.limit < 0:
            parser.error("limit must be nonnegative")
        read_trace(args.source, args.limit)


if __name__ == "__main__":
    main()
