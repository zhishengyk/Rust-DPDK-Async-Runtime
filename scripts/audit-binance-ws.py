#!/usr/bin/env python3
"""Audit saved live measurements without changing the measured binary or samples."""
import argparse
import csv
import json
from pathlib import Path


def audit(path):
    path = Path(path)
    report = json.loads(path.read_text())
    assert report['success'], report.get('error')
    assert report['error'] is None and report['trace_error'] is None
    assert report['mempool']['leak_free'] and report['tx_failed'] == 0
    assert report['statistics']['backpressure_batches'] == 0
    assert not report['timing']['dma_completion_exactly_measured']
    assert not report['timing']['kernel_tcp_socket_used']
    assert all(d['count'] == report['samples'] for d in report['latency'].values())
    csv_path = path.with_suffix('.csv')
    checked = 0
    if csv_path.exists():
        with csv_path.open(newline='') as f:
            for raw in csv.DictReader(f):
                row = {k: int(v) for k, v in raw.items()}
                assert row['rx_last_to_json'] <= row['rx_last_to_app']
                assert row['rx_last_to_json'] <= row['completion_to_json_upper_bound']
                assert row['rx_last_to_app'] <= row['completion_to_app_upper_bound']
                assert row['rx_first_to_app'] >= row['rx_last_to_app']
                assert row['rx_first_to_json'] >= row['rx_last_to_json']
                parts = sum(row[k] for k in ['rx_to_tcp_record', 'tls', 'websocket', 'json', 'dispatch'])
                assert 0 <= row['rx_last_to_app'] - parts <= 4
                assert abs(row['completion_to_app_upper_bound'] - row['rx_last_to_app']
                           - row['completion_observation_window']) <= 1
                checked += 1
        assert checked == report['samples'], (checked, report['samples'])
    names = ['rx_last_to_json', 'rx_last_to_app', 'completion_to_json_upper_bound',
             'completion_to_app_upper_bound', 'completion_observation_window']
    return {'file': str(path), 'stream': report['options']['stream'], 'samples': report['samples'],
            'csv_rows_checked': checked, 'hardware_rx_timestamp_supported':
            report['timing']['hardware_rx_timestamp_supported'],
            'latency_ns': {k: report['latency'][k] for k in names}}


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('reports', nargs='+')
    args = parser.parse_args()
    print(json.dumps([audit(p) for p in args.reports], indent=2))
