#!/bin/sh
# Source checks need only Python 3 and a POSIX shell, never downloads or root.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
python3 - "$repo" "$@" <<'PY'
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import uuid
import zlib

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

def validate_hybrid_image(img_path: Path):
    require(img_path.is_file(), f'missing disk image {img_path}')
    size = img_path.stat().st_size
    require(size >= (2 + 100 + 512 + 512) * 1024 * 1024, f'hybrid image too small: {size} bytes')
    with img_path.open('rb') as f:
        mbr = f.read(512)
        require(mbr[510:512] == b'\x55\xaa', 'invalid MBR boot signature')
        kernel_sector = struct.unpack_from('<Q', mbr, 0x5c)[0]
        require(kernel_sector == 2048, f'expected MBR GRUB kernel_sector=2048, got {kernel_sector}')

        mbr_types = []
        for idx in range(4):
            entry = mbr[446 + idx * 16:446 + (idx + 1) * 16]
            ptype = entry[4]
            if ptype != 0:
                mbr_types.append(ptype)
        require(0xee in mbr_types, f'hybrid MBR missing GPT protective 0xee entry: {mbr_types}')
        require(0xef in mbr_types, f'hybrid MBR missing EFI 0xef entry: {mbr_types}')
        require(mbr_types.count(0x83) >= 2, f'hybrid MBR missing root/data 0x83 entries: {mbr_types}')

        # Verify GPT header at LBA 1.
        gpt_hdr = f.read(512)
        require(gpt_hdr[0:8] == b'EFI PART', 'missing GPT signature at LBA 1')
        hdr_size = struct.unpack_from('<I', gpt_hdr, 12)[0]
        hdr_crc = struct.unpack_from('<I', gpt_hdr, 16)[0]
        hdr_copy = bytearray(gpt_hdr[:hdr_size])
        struct.pack_into('<I', hdr_copy, 16, 0)
        require((zlib.crc32(hdr_copy) & 0xffffffff) == hdr_crc, 'GPT header CRC32 mismatch')

        part_lba = struct.unpack_from('<Q', gpt_hdr, 72)[0]
        num_parts = struct.unpack_from('<I', gpt_hdr, 80)[0]
        entry_size = struct.unpack_from('<I', gpt_hdr, 84)[0]
        f.seek(part_lba * 512)
        table = f.read(num_parts * entry_size)

        parts = []
        for idx in range(num_parts):
            raw = table[idx * entry_size:(idx + 1) * entry_size]
            type_guid = uuid.UUID(bytes_le=raw[0:16])
            if type_guid.int == 0:
                continue
            part_guid = uuid.UUID(bytes_le=raw[16:32])
            first_lba, last_lba = struct.unpack_from('<QQ', raw, 32)
            parts.append((type_guid, part_guid, first_lba, last_lba))

        require(len(parts) == 3, f'expected 3 GPT partitions, found {len(parts)}')
        esp_guid = uuid.UUID('c12a7328-f81f-11d2-ba4b-00a0c93ec93b')
        linux_guid = uuid.UUID('0fc63daf-8483-4772-8e79-3d69d8477de4')
        p1, p2, p3 = parts
        require(p1[0] == esp_guid, f'partition 1 is not EFI System Partition: {p1[0]}')
        require(p1[2] == 4096, f'partition 1 must start at LBA 4096 (2 MiB), got {p1[2]}')
        require((p1[3] - p1[2] + 1) * 512 == 100 * 1024 * 1024, 'partition 1 must be 100 MiB')
        require(p2[0] == linux_guid, f'partition 2 is not Linux filesystem: {p2[0]}')
        require(str(p2[1]) == '48595045-0000-4000-8000-000000000002', f'unexpected root PARTUUID: {p2[1]}')
        require((p2[3] - p2[2] + 1) * 512 == 512 * 1024 * 1024, 'partition 2 must be 512 MiB')
        require(p3[0] == linux_guid, f'partition 3 is not Linux filesystem: {p3[0]}')
        require(str(p3[1]) == '48595045-0000-4000-8000-000000000003', f'unexpected data PARTUUID: {p3[1]}')
        require((p3[3] - p3[2] + 1) * 512 == 512 * 1024 * 1024, 'partition 3 must be 512 MiB')
        require(p3[2] > p2[3], 'partition 3 must be the last partition for auto-expansion')

        # Verify patched GRUB i386-pc diskboot blocklist at offset 1 MiB.
        f.seek(1024 * 1024)
        grub_sec0 = f.read(512)
        block_start = struct.unpack_from('<Q', grub_sec0, 0x1f4)[0]
        block_len = struct.unpack_from('<H', grub_sec0, 0x1fc)[0]
        require(block_start == 2049 and block_len > 0,
                f'invalid BIOS GRUB core blocklist at 1 MiB: start={block_start}, len={block_len}')

br = config(ext / 'configs/hyperos_x86_64_defconfig', 'BR2_')
kernel = config(ext / 'kernel/hyperos.config', 'CONFIG_')
busybox = config(ext / 'configs/busybox.fragment', 'CONFIG_')

for symbol in ('BR2_x86_64', 'BR2_x86_corei7', 'BR2_CCACHE',
               'BR2_TOOLCHAIN_EXTERNAL', 'BR2_TOOLCHAIN_EXTERNAL_BOOTLIN',
               'BR2_TOOLCHAIN_EXTERNAL_BOOTLIN_X86_64_CORE_I7_MUSL_STABLE',
               'BR2_INIT_NONE', 'BR2_SYSTEM_BIN_SH_DASH',
               'BR2_LINUX_KERNEL_INSTALL_TARGET',
               'BR2_PACKAGE_BUSYBOX', 'BR2_PACKAGE_RUNIT', 'BR2_PACKAGE_VMM_DAEMON',
               'BR2_PACKAGE_CROSVM', 'BR2_PACKAGE_JQ', 'BR2_PACKAGE_SOCAT',
               'BR2_PACKAGE_BTRFS_PROGS', 'BR2_PACKAGE_IPROUTE2', 'BR2_PACKAGE_NFTABLES',
               'BR2_PACKAGE_UTIL_LINUX', 'BR2_PACKAGE_UTIL_LINUX_BINARIES',
               'BR2_PACKAGE_UTIL_LINUX_PARTX', 'BR2_TARGET_GRUB2',
               'BR2_TARGET_GRUB2_I386_PC', 'BR2_TARGET_GRUB2_X86_64_EFI',
               'BR2_TARGET_ROOTFS_EROFS', 'BR2_PACKAGE_HOST_DOSFSTOOLS',
               'BR2_PACKAGE_HOST_MTOOLS', 'BR2_PACKAGE_HOST_GENIMAGE',
               'BR2_PACKAGE_HOST_BTRFS_PROGS', 'BR2_PACKAGE_HOST_ZSTD'):
    require(br.get(symbol) == 'y', f'{symbol} must be enabled')

for symbol in ('KVM', 'KVM_INTEL', 'KVM_AMD', 'ZRAM', 'ZRAM_BACKEND_ZSTD', 'KSM',
               'VIRTIO_PCI', 'VIRTIO_BLK', 'VIRTIO_NET', 'VIRTIO_BALLOON',
               'VIRTIO_CONSOLE', 'VSOCKETS', 'VIRTIO_VSOCKETS', 'BTRFS_FS',
               'EFI', 'EFI_STUB', 'EFI_PARTITION', 'MSDOS_PARTITION',
               'BLK_DEV_INITRD', 'RD_ZSTD', 'VFAT_FS',
               'USB_SUPPORT', 'USB_XHCI_HCD', 'USB_STORAGE', 'USB_UAS',
               'BLK_DEV_NVME', 'SATA_AHCI',
               'I2C_HID_CORE', 'I2C_HID_ACPI',
               'EROFS_FS', 'EROFS_FS_ZIP', 'OVERLAY_FS',
               'DRM_I915', 'DRM_AMDGPU', 'DRM_SIMPLEDRM', 'DRM_VIRTIO_GPU',
               'SYSFB_SIMPLEFB',
               'INPUT', 'INPUT_KEYBOARD', 'SERIO', 'SERIO_I8042', 'KEYBOARD_ATKBD',
               'VT', 'VT_CONSOLE', 'FRAMEBUFFER_CONSOLE'):
    require(kernel.get('CONFIG_' + symbol) == 'y', f'{symbol} must be built in')

for symbol in ('BLK_DEV_FD', 'PARPORT', 'SOUND', 'MODULES', 'NFS_FS', 'CIFS', 'XFS_FS'):
    require(kernel.get('CONFIG_' + symbol) == 'n', f'{symbol} must be disabled')

for symbol in ('CONFIG_FEATURE_MOUNT_LABEL', 'CONFIG_FEATURE_SWAPON_PRI',
               'CONFIG_SWITCH_ROOT', 'CONFIG_MOUNTPOINT', 'CONFIG_GETTY'):
    require(busybox.get(symbol) == 'y', f'BusyBox {symbol} must be enabled')

for value in br.values():
    for relative in re.findall(r'\$\(BR2_EXTERNAL_HYPEROS_PATH\)/([^" ]+)', value):
        require((ext / relative).exists(), f'missing config input {relative}')

for required_script in ('initramfs/init', 'initramfs/build-initramfs.sh',
                        'board/post-build.sh', 'board/post-image.sh',
                        'rootfs-overlay/etc/sv/getty-tty1/run'):
    p = ext / required_script
    require(p.is_file(), f'missing required script {required_script}')
    require(os.access(p, os.X_OK), f'{p} must be executable')

for runit_ref in ('rootfs-overlay/etc/runit/1', 'board/post-build.sh'):
    require('getty-tty1' in (ext / runit_ref).read_text(),
            f'{runit_ref} must include getty-tty1 service')

for path in ext.rglob('*'):
    if path.is_file() and (path.suffix == '.sh' or path.name in ('init', 'run', '1', '2', '3')):
        require(os.access(path, os.X_OK), f'{path} must be executable')
        subprocess.run(['sh', '-n', str(path)], check=True)
for script in ('scripts/validate-configs.sh', 'scripts/test-v1-release.sh'):
    sp = root / script
    require(sp.is_file() and os.access(sp, os.X_OK), f'{sp} must exist and be executable')
    subprocess.run(['sh', '-n', str(sp)], check=True)

require('echo 0 > /sys/kernel/mm/ksm/run' in
        (ext / 'rootfs-overlay/etc/sv/ksm/run').read_text(), 'KSM must default off')
subprocess.run(['sh', '-n', str(ext / 'rootfs-overlay/usr/bin/hyperos-vm')], check=True)

# Guard against regression in GNU readelf PT_INTERP parsing and post-staging audit.
build_initramfs = (ext / 'initramfs/build-initramfs.sh').read_text()
require('[Requesting program interpreter: ' in build_initramfs,
        'build-initramfs.sh must match GNU readelf "[Requesting program interpreter: ...]" format')
require('requires missing ELF interpreter' in build_initramfs,
        'build-initramfs.sh must fail closed when a staged binary is missing its ELF interpreter')

# Validate hybrid disk image, GRUB boot menu, and initramfs contracts.
genimage_cfg = (ext / 'board/genimage.cfg').read_text()
for needle in ('partition-table-type = "hybrid"', 'extraargs = "-F 32"',
               'EFI/BOOT/BOOTX64.EFI', 'size = 100M', 'size = 512M',
               'image = "rootfs.erofs"', 'image = "data.btrfs"',
               'partition-type-uuid = U', 'partition-type = 0xef'):
    require(needle in genimage_cfg, f'genimage.cfg missing required setting: {needle}')
require(genimage_cfg.rfind('partition data') > genimage_cfg.rfind('partition root'),
        'partition data must be placed last in genimage.cfg for first-boot auto-expansion')

grub_cfg = (ext / 'rootfs-overlay/boot/grub/grub.cfg').read_text()
for needle in ('Persistent Mode', 'Stateless RAM-Only Mode',
               'hyperos.mode=persistent', 'hyperos.mode=stateless',
               '/boot/bzImage', '/boot/initramfs.cpio.zst', 'HYPEROS_EFI'):
    require(needle in grub_cfg, f'grub.cfg missing required entry: {needle}')

initramfs_init = (ext / 'initramfs/init').read_text()
for needle in ('btrfs filesystem resize max', 'compress=zstd:3,noatime,commit=120',
               'OVERLAY_SIZE="128M"', 'mount -t overlay', 'mount -t erofs',
               'HYPEROS_DATA', 'zram-control/hot_add', 'switch_root'):
    require(needle in initramfs_init, f'initramfs/init missing required logic: {needle}')

post_image = (ext / 'board/post-image.sh').read_text()
for needle in ('build-initramfs.sh', 'BOOTX64.EFI', '-L HYPEROS_DATA', '2048', '2049'):
    require(needle in post_image, f'post-image.sh missing required step: {needle}')

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
    require(len(args) == 2 and args[0] in ('--resolved-buildroot', '--resolved-kernel', '--image'),
            'usage: validate-configs.sh [--resolved-buildroot .config | --resolved-kernel .config | --image hyperos.img]')
    if args[0] == '--image':
        validate_hybrid_image(Path(args[1]))
        print('PASS: source configuration, shell checks, and hybrid GPT/MBR disk image verified')
    else:
        expected, prefix = (br, 'BR2_') if args[0] == '--resolved-buildroot' else (kernel, 'CONFIG_')
        actual = config(Path(args[1]), prefix)
        for key, value in expected.items():
            require(actual.get(key, 'n') == value,
                    f'{key}: requested {value}, resolved {actual.get(key, "n")}')
        print('PASS: source configuration and shell checks; resolved config matches')
else:
    print('PASS: source configuration and shell checks')
PY
