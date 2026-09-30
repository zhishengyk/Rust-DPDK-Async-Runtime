#!/usr/bin/env python3
"""Read IMDSv2 once, before detaching the secondary ENI."""
import pathlib
import shlex
import urllib.request

base = 'http://169.254.169.254/latest/'
req = urllib.request.Request(base + 'api/token', method='PUT', headers={'X-aws-ec2-metadata-token-ttl-seconds': '60'})
token = urllib.request.urlopen(req, timeout=3).read().decode()
def get(path):
    req = urllib.request.Request(base + 'meta-data/' + path, headers={'X-aws-ec2-metadata-token': token})
    return urllib.request.urlopen(req, timeout=3).read().decode().strip()

interfaces = {p.joinpath('address').read_text().strip(): p for p in pathlib.Path('/sys/class/net').iterdir()}
config = {}
for mac in get('network/interfaces/macs/').splitlines():
    mac = mac.rstrip('/')
    path = f'network/interfaces/macs/{mac}/'
    device = int(get(path + 'device-number'))
    iface = interfaces[mac]
    if device == 0:
        config['KERNEL_IF'] = iface.name
    else:
        config.update(BDF=iface.joinpath('device').resolve().name, SRC_IP=get(path + 'local-ipv4s').splitlines()[0], DPDK_IF=iface.name)
config.update(CORE='2', PEER_IP='10.202.8.15', PEER_MAC='06:ff:fd:b6:f0:cd')
assert 'BDF' in config and 'KERNEL_IF' in config, 'Two ENIs required'
for key, value in config.items():
    print(f'export {key}={shlex.quote(value)}')
