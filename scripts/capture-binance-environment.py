#!/usr/bin/env python3
"""Read-only capture of CPU affinity and interrupt/isolation state during a live run."""
import datetime
import glob
import json
import os
import platform
import subprocess
import sys
from pathlib import Path


def read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


result = {'captured_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
          'kernel': platform.release(), 'cmdline': read('/proc/cmdline'),
          'isolated_cpus': read('/sys/devices/system/cpu/isolated'),
          'nohz_full': read('/sys/devices/system/cpu/nohz_full'),
          'default_irq_affinity': read('/proc/irq/default_smp_affinity'),
          'irqbalance_config': read('/etc/sysconfig/irqbalance'),
          'processes': [], 'interrupts': read('/proc/interrupts'), 'irqs': []}
for name, command in {
    'irqbalance_active': ['systemctl', 'is-active', 'irqbalance'],
    'irqbalance_enabled': ['systemctl', 'is-enabled', 'irqbalance'],
    'irqbalance_unit': ['systemctl', 'show', 'irqbalance', '-p', 'DropInPaths', '-p', 'ExecStart'],
    'saved_kernel_arguments': ['sudo', '-n', 'grubby', '--info=ALL'],
}.items():
    try:
        captured = subprocess.run(command, capture_output=True, text=True, timeout=10)
        result[name] = {'exit_code': captured.returncode, 'stdout': captured.stdout.strip(),
                        'stderr': captured.stderr.strip()}
    except (OSError, subprocess.TimeoutExpired) as error:
        result[name] = {'error': str(error)}
for proc in glob.glob('/proc/[0-9]*'):
    if read(proc + '/comm') != 'binance-ws':
        continue
    threads = []
    for task in glob.glob(proc + '/task/*'):
        tid = int(task.rsplit('/', 1)[1])
        try:
            threads.append({'tid': tid, 'name': read(task + '/comm'),
                            'allowed_cpus': sorted(os.sched_getaffinity(tid))})
        except ProcessLookupError:
            pass
    result['processes'].append({'pid': int(proc.rsplit('/', 1)[1]), 'threads': threads})
for irq in glob.glob('/proc/irq/[0-9]*'):
    result['irqs'].append({'irq': int(irq.rsplit('/', 1)[1]),
                          'requested': read(irq + '/smp_affinity_list'),
                          'effective': read(irq + '/effective_affinity_list')})
Path(sys.argv[1]).write_text(json.dumps(result, indent=2))
print(json.dumps({k: result[k] for k in ['captured_utc', 'isolated_cpus', 'nohz_full', 'processes']}, indent=2))
