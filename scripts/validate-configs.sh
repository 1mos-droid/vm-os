#!/bin/sh
# Source checks need only Python 3 and a POSIX shell, never downloads or root.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
python3 - "$repo" "$@" <<'PY'
import json
import os
from pathlib import Path
import re
import subprocess
import sys

root = Path(sys.argv[1])
ext = root / 'kernel-buildroot'

def require(condition, message):
    if not condition:
        sys.exit('FAIL: ' + message)

def config(path, prefix):
    result = {}
    for line in path.read_text().splitlines():
        match = re.fullmatch(r'(' + prefix + r'[A-Za-z0-9_]+)=(y|m|[0-9]+|0x[0-9a-fA-F]+|"[^"\n]*")', line)
        disabled = re.fullmatch(r'# (' + prefix + r'[A-Za-z0-9_]+) is not set', line)
        if match or disabled:
            key, value = match.groups() if match else (disabled[1], 'n')
            require(key not in result, f'{path}: duplicate {key}')
            result[key] = value
        else:
            require(not line.strip() or line.startswith('#'), f'{path}: invalid config line {line!r}')
    return result

br = config(ext / 'configs/hyperos_x86_64_defconfig', 'BR2_')
kernel = config(ext / 'kernel/hyperos.config', 'CONFIG_')
config(ext / 'configs/busybox.fragment', 'CONFIG_')
for symbol in ('BR2_x86_64', 'BR2_TOOLCHAIN_BUILDROOT_MUSL', 'BR2_INIT_NONE',
               'BR2_SYSTEM_BIN_SH_DASH', 'BR2_PACKAGE_BUSYBOX', 'BR2_PACKAGE_RUNIT',
               'BR2_TARGET_GRUB2', 'BR2_TARGET_ROOTFS_EXT2_4'):
    require(br.get(symbol) == 'y', f'{symbol} must be enabled')
for symbol in ('KVM', 'KVM_INTEL', 'KVM_AMD', 'ZRAM', 'ZRAM_BACKEND_ZSTD', 'KSM',
               'VIRTIO_PCI', 'VIRTIO_BLK', 'VIRTIO_NET', 'VIRTIO_BALLOON',
               'VIRTIO_CONSOLE', 'VSOCKETS', 'VIRTIO_VSOCKETS', 'BTRFS_FS', 'DRM_VIRTIO_GPU'):
    require(kernel.get('CONFIG_' + symbol) == 'y', f'{symbol} must be built in')
for symbol in ('BLK_DEV_FD', 'PARPORT', 'SOUND', 'MODULES', 'NFS_FS', 'CIFS', 'XFS_FS'):
    require(kernel.get('CONFIG_' + symbol) == 'n', f'{symbol} must be disabled')
for value in br.values():
    for relative in re.findall(r'\$\(BR2_EXTERNAL_HYPEROS_PATH\)/([^" ]+)', value):
        require((ext / relative).exists(), f'missing config input {relative}')
for path in ext.rglob('*'):
    if path.is_file() and (path.suffix == '.sh' or path.name in ('run', '1', '2', '3')):
        require(os.access(path, os.X_OK), f'{path} must be executable')
        subprocess.run(['sh', '-n', str(path)], check=True)
subprocess.run(['sh', '-n', str(root / 'scripts/validate-configs.sh')], check=True)
arch = (root / 'ARCHITECTURE.md').read_text()
for method in ('create_vm', 'start_vm', 'stop_vm', 'pause_vm', 'snapshot_vm',
               'set_balloon_ram', 'set_network_mode', 'list_vms'):
    require(f'`{method}`' in arch, f'missing RPC method {method}')
for frame in re.findall(r'```json\n(.*?)\n```', arch, re.S):
    for line in frame.splitlines():
        json.loads(line)
require('/run/hyperos/vmm.sock' in arch, 'socket contract missing')
for directory in ('vmm-daemon', 'vm-control-ui', 'guest-images', '.github/workflows'):
    require((root / directory).is_dir(), f'missing directory {directory}')

args = sys.argv[2:]
if args:
    require(len(args) == 2 and args[0] in ('--resolved-buildroot', '--resolved-kernel'),
            'usage: validate-configs.sh [--resolved-buildroot|--resolved-kernel .config]')
    expected, prefix = (br, 'BR2_') if args[0] == '--resolved-buildroot' else (kernel, 'CONFIG_')
    actual = config(Path(args[1]), prefix)
    for key, value in expected.items():
        require(actual.get(key, 'n') == value,
                f'{key}: requested {value}, resolved {actual.get(key, "n")}')
print('PASS: source configuration and shell checks' + ('; resolved config matches' if args else ''))
PY
