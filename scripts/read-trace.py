#!/usr/bin/env python3
"""Stream every raw timestamp event as CSV, without unit conversion."""
import argparse
import csv
import gzip
import itertools
import struct
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('file')
parser.add_argument('--limit', type=int)
args = parser.parse_args()
open_trace = gzip.open if args.file.endswith('.gz') else open
with open_trace(args.file, 'rb') as trace:
    header = trace.read(64)
    if len(header) != 64 or header[:8] != b'DPDKTS01':
        raise SystemExit('Invalid trace header')
    print('tsc_hz=' + str(struct.unpack_from('<Q', header, 8)[0]), file=sys.stderr)
    rows = csv.writer(sys.stdout)
    rows.writerow(['kind', 'session', 'seq', 'T0', 'T1', 'T2', 'T3', 'T4', 'T5'])
    try:
        for record in itertools.islice(iter(lambda: trace.read(64), b''), args.limit):
            if len(record) != 64:
                raise SystemExit('Truncated trace record')
            kind, identity, *stamps = struct.unpack('<8Q', record)
            rows.writerow([kind, identity >> 32, identity & 0xffffffff, *stamps])
    except BrokenPipeError:
        sys.exit(0)
