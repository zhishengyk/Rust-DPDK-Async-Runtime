#!/usr/bin/env python3
import collections
import json
import math
import re
import sys
from decimal import Decimal

source, destination, duration = sys.argv[1:]
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
