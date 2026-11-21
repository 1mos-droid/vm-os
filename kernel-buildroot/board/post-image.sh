#!/bin/sh
set -eu

EXT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
HOST_DIR=${HOST_DIR:-$(dirname "$BINARIES_DIR")/host}

# 1. Build the minimal Zstd-compressed initramfs (OverlayFS + GPT/Btrfs auto-expand + Stateless ZRAM).
"$EXT_DIR/initramfs/build-initramfs.sh" "$TARGET_DIR" "$BINARIES_DIR"

# 2. Stage UEFI BOOTX64.EFI and shared GRUB2 config for both UEFI and Legacy BIOS boot.
mkdir -p "$BINARIES_DIR/efi-part/EFI/BOOT" "$BINARIES_DIR/efi-part/boot/grub"
if [ ! -s "$BINARIES_DIR/efi-part/EFI/BOOT/bootx64.efi" ]; then
    echo "FAIL: missing UEFI GRUB2 image $BINARIES_DIR/efi-part/EFI/BOOT/bootx64.efi" >&2
    exit 1
fi
cp "$BINARIES_DIR/efi-part/EFI/BOOT/bootx64.efi" "$BINARIES_DIR/efi-part/EFI/BOOT/BOOTX64.EFI"
cp "$EXT_DIR/rootfs-overlay/boot/grub/grub.cfg" "$BINARIES_DIR/efi-part/EFI/BOOT/grub.cfg"
cp "$EXT_DIR/rootfs-overlay/boot/grub/grub.cfg" "$BINARIES_DIR/efi-part/boot/grub/grub.cfg"

# 3. Patch GRUB2 i386-pc boot.img and core grub.img to load from LBA 2048 (offset 1M),
#    keeping LBAs 1..33 clear for the GPT header and partition entry array.
python3 - "$BINARIES_DIR" <<'PY'
from pathlib import Path
import struct
import sys

binaries = Path(sys.argv[1])
boot_src = binaries / 'boot.img'
grub_src = binaries / 'grub.img'

boot = bytearray(boot_src.read_bytes())
if len(boot) != 512:
    sys.exit(f'FAIL: expected 512-byte boot.img, got {len(boot)}')
orig_kernel_sector = struct.unpack_from('<Q', boot, 0x5c)[0]
if orig_kernel_sector != 1:
    sys.exit(f'FAIL: unexpected GRUB boot.img kernel_sector={orig_kernel_sector} (expected 1)')
# Point MBR stage-1 to LBA 2048 (offset 1 MiB).
struct.pack_into('<Q', boot, 0x5c, 2048)
(binaries / 'boot-hybrid.img').write_bytes(boot)

grub = bytearray(grub_src.read_bytes())
if len(grub) < 1024 or len(grub) > 1024 * 1024:
    sys.exit(f'FAIL: grub.img size {len(grub)} out of bounds for [1M, 2M) BIOS boot gap')
orig_block_start = struct.unpack_from('<Q', grub, 0x1f4)[0]
block_len = struct.unpack_from('<H', grub, 0x1fc)[0]
if orig_block_start != 2:
    sys.exit(f'FAIL: unexpected GRUB diskboot blocklist start={orig_block_start} (expected 2)')
if block_len == 0 or (block_len + 1) * 512 > 1024 * 1024:
    sys.exit(f'FAIL: invalid GRUB diskboot blocklist length={block_len} sectors')
# Point diskboot.img blocklist to LBA 2049 (second sector of grub.img at offset 1 MiB).
struct.pack_into('<Q', grub, 0x1f4, 2049)
(binaries / 'grub-hybrid.img').write_bytes(grub)
PY

# 4. genimage 18 has no Btrfs backend; create the initial 512M HYPEROS_DATA filesystem first.
rm -f "$BINARIES_DIR/data.btrfs"
truncate -s 512M "$BINARIES_DIR/data.btrfs"
"$HOST_DIR/bin/mkfs.btrfs" -f -L HYPEROS_DATA "$BINARIES_DIR/data.btrfs"

exec support/scripts/genimage.sh "$@"
