#!/bin/sh
# Boot a scratch host image and an imported Alpine VM; preserve the input image.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
IMAGE=output/images/hyperos.img
GUEST_DIR=
BOOT_MODE=bios
OVMF_CODE=/usr/share/OVMF/OVMF_CODE.fd
RAM_MIB=4096
EXPAND_GIB=8
DRY_RUN=0
usage() {
    echo 'Usage: sudo scripts/test-v1-release.sh [--image PATH] [--guest-dir DIR] [--bios|--uefi] [--ovmf PATH] [--ram MIB] [--expand-gib GIB] [--dry-run]'
}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --image|--guest-dir|--ovmf|--ram|--expand-gib)
            [ "$#" -ge 2 ] || { usage >&2; exit 2; }
            case "$1" in
                --image) IMAGE=$2;;
                --guest-dir) GUEST_DIR=$2;;
                --ovmf) OVMF_CODE=$2;;
                --ram) RAM_MIB=$2;;
                --expand-gib) EXPAND_GIB=$2;;
            esac
            shift 2;;
        --bios) BOOT_MODE=bios; shift;;
        --uefi) BOOT_MODE=uefi; shift;;
        --dry-run) DRY_RUN=1; shift;;
        -h|--help) usage; exit 0;;
        *) usage >&2; exit 2;;
    esac
done
case "$RAM_MIB:$EXPAND_GIB" in *[!0-9:]*|:*|*:) usage >&2; exit 2;; esac
[ "$RAM_MIB" -ge 2048 ] && [ "$EXPAND_GIB" -ge 4 ] || { echo 'Need >=2048 MiB RAM and >=4 GiB scratch disk' >&2; exit 2; }
if [ "$DRY_RUN" -eq 1 ]; then
    cat <<PLAN
Scratch boot plan ($BOOT_MODE):
  Copy $IMAGE, grow to ${EXPAND_GIB} GiB; input image remains unchanged.
  Build Alpine unless --guest-dir is provided; verify and import into HYPEROS_DATA.
  Add hyperos.boot_image=<manifest image_id> to the scratch GRUB configuration.
  Boot QEMU with KVM, CPU host, ${RAM_MIB} MiB RAM and serial capture.
  Require KSM=0, idle host RAM <358400 KiB, daemon VM running, and Alpine userspace marker.
  Does not verify guest persistence across a host reboot or graphical idle RAM.
PLAN
    exit 0
fi
[ "$(id -u)" -eq 0 ] || { echo 'Run with sudo (offline loop mount)' >&2; exit 1; }
for tool in python3 qemu-system-x86_64 mcopy losetup mount umount blkid cp truncate; do
    command -v "$tool" >/dev/null || { echo "Missing $tool" >&2; exit 1; }
done
[ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo '/dev/kvm is required' >&2; exit 1; }
[ -s "$IMAGE" ] || { echo "Missing $IMAGE" >&2; exit 1; }
if [ "$BOOT_MODE" = uefi ]; then
    [ -r "$OVMF_CODE" ] || { echo "Missing $OVMF_CODE" >&2; exit 1; }
fi
work=$(mktemp -d "${TMPDIR:-/tmp}/hyperos-release.XXXXXX")
pid=
cleanup() {
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    # Retain logs after failure as well as success, but reclaim the large scratch disk.
    rm -f "$work/host.img"
    echo "Release logs: $work/serial.log"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
cp --sparse=always "$IMAGE" "$work/host.img"
image_bytes=$(wc -c < "$work/host.img")
[ "$image_bytes" -lt "$((EXPAND_GIB * 1024 * 1024 * 1024))" ] || { echo 'Scratch size must exceed image size' >&2; exit 1; }
truncate -s "${EXPAND_GIB}G" "$work/host.img"
if [ -z "$GUEST_DIR" ]; then
    "$repo/guest-images/build-alpine-guest.sh" --out-dir "$work/guests"
    GUEST_DIR="$work/guests/alpine-3-21-virtio"
fi
image_id=$("$repo/scripts/import-guest-image.py" "$GUEST_DIR" "$work/host.img")
python3 - "$repo/kernel-buildroot/rootfs-overlay/boot/grub/grub.cfg" "$work/grub.cfg" "$image_id" <<'PY'
from pathlib import Path
import sys
source, destination, image_id = sys.argv[1:]
Path(destination).write_text(Path(source).read_text().replace('hyperos.mode=persistent ', f'hyperos.mode=persistent hyperos.boot_image={image_id} '))
PY
for cfg in ::EFI/BOOT/grub.cfg ::boot/grub/grub.cfg; do
    mcopy -o -i "$work/host.img@@2097152" "$work/grub.cfg" "$cfg"
done
set -- qemu-system-x86_64 -enable-kvm -cpu host -m "$RAM_MIB" -smp 2 -nographic -monitor none -no-reboot -drive "file=$work/host.img,format=raw,if=virtio"
if [ "$BOOT_MODE" = uefi ]; then
    set -- "$@" -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE"
fi
"$@" >"$work/serial.log" 2>&1 &
pid=$!
attempt=0
while [ "$attempt" -lt 240 ]; do
    if grep -q HYPEROS_VM_FAILED "$work/serial.log"; then cat "$work/serial.log"; exit 1; fi
    if grep -q HYPEROS_GUEST_READY "$work/serial.log" && grep -q HYPEROS_VM_RUNNING "$work/serial.log"; then
        grep -q '^HYPEROS_KSM=0' "$work/serial.log"
        used=$(sed -n 's/^HYPEROS_IDLE_RAM_KIB=\([0-9]*\).*/\1/p' "$work/serial.log" | head -n 1)
        [ -n "$used" ] && [ "$used" -lt 358400 ]
        echo "PASS: host boot, KSM off, idle RAM ${used} KiB, real Alpine VM reached userspace"
        exit 0
    fi
    kill -0 "$pid" 2>/dev/null || { cat "$work/serial.log"; exit 1; }
    attempt=$((attempt + 1))
    sleep 1
done
cat "$work/serial.log"
echo 'FAIL: guest did not reach userspace in 240 seconds' >&2
exit 1
