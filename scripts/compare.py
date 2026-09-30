#!/usr/bin/env python3
"""A/B ranking. C is compared only to a separately matched low-load A run."""
import json
import sys

a, b = [json.load(open(path)) for path in sys.argv[1:3]]
for key in ['delay_us', 'duration_sec', 'sessions', 'payload', 'timeout_us', 'peer_ip', 'bdf']:
    assert a['options'][key] == b['options'][key], f'A/B differ: {key}'
for result in [a, b]:
    assert result['accounted'] and result['mempool']['leak_free']
print('A - B process latency (ns; difference of quantiles):')
for quantile in ['p50', 'p90', 'p99', 'p999', 'p9999', 'max']:
    print(f"  {quantile}: {a['latency']['process'][quantile] - b['latency']['process'][quantile]}")
