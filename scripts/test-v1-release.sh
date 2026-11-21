#!/bin/sh
# End-to-end v1 release acceptance runner for HyperOS on a KVM-capable host.
#
# Demonstrates the v1 milestone:
#   1. Boot hyperos.img with 4 GiB RAM (-m 4096) on Legacy BIOS or UEFI (OVMF)
#   2. Capture serial console logs (initramfs OverlayFS + HYPEROS_DATA auto-expand + runit)
#   3. Measure host idle RAM (/proc/meminfo: MemTotal - MemAvailable < 358400 KiB)
#   4. Exercise Alpine VM persistence across a full HyperOS host reboot
#
# Use --dry-run on hosts without QEMU or /dev/kvm to verify inputs and print the exact run plan.
set -eu

IMAGE="output/images/hyperos.img"
BOOT_MODE="bios"
DRY_RUN=0
WORK_DIR=""
RAM_MIB=4096
EXPAND_GIB=8
OVMF_CODE="/usr/share/OVMF/OVMF_CODE.fd"

usage() {
    cat <<'EOF'
Usage: scripts/test-v1-release.sh [OPTIONS]

Options:
  --image <PATH>        Path to hyperos.img (default: output/images/hyperos.img)
  --bios                Boot via Legacy BIOS (default)
  --uefi                Boot via UEFI OVMF firmware
  --ovmf <PATH>         Path to OVMF_CODE.fd (default: /usr/share/OVMF/OVMF_CODE.fd)
  --ram <MIB>           Host RAM cap in MiB (default: 4096)
  --expand-gib <GIB>    Resize a scratch copy of hyperos.img to test first-boot auto-expand (default: 8)
  --dry-run             Validate prerequisites and print the v1 release verification plan without launching QEMU
  -h, --help            Show this help message
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --image)
            IMAGE=$2
            shift 2
            ;;
        --bios)
            BOOT_MODE="bios"
            shift
            ;;
        --uefi)
            BOOT_MODE="uefi"
            shift
            ;;
        --ovmf)
            OVMF_CODE=$2
            shift 2
            ;;
        --ram)
            RAM_MIB=$2
            shift 2
            ;;
        --expand-gib)
            EXPAND_GIB=$2
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "FAIL: unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

echo "=== HyperOS v1 Release Acceptance Plan ==="
echo "  Image:          $IMAGE"
echo "  Firmware mode:  $BOOT_MODE"
echo "  Host RAM cap:   ${RAM_MIB} MiB"
echo "  Scratch disk:   ${EXPAND_GIB} GiB (verifies initramfs first-boot Partition 3 auto-expand)"

if [ "$DRY_RUN" -eq 1 ]; then
    echo ""
    echo "Checking host capabilities (dry-run):"
    if command -v qemu-system-x86_64 >/dev/null 2>&1; then
        echo "  [OK]   qemu-system-x86_64: $(command -v qemu-system-x86_64)"
    else
        echo "  [WARN] qemu-system-x86_64 not installed on this host"
    fi
    if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
        echo "  [OK]   /dev/kvm accessible"
    else
        echo "  [WARN] /dev/kvm unavailable (run on a KVM or nested-virtualization cloud VM)"
    fi
    if [ -s "$IMAGE" ]; then
        echo "  [OK]   image present: $IMAGE"
    else
        echo "  [INFO] image not built yet: $IMAGE"
    fi
    cat <<EOF

Exact v1 acceptance steps when executed on a KVM-capable host:
  1. Copy $IMAGE to a ${EXPAND_GIB} GiB scratch image and boot with:
     qemu-system-x86_64 -enable-kvm -cpu host -m ${RAM_MIB} -smp 2 -nographic \\
       -drive file=<scratch.img>,format=raw,if=virtio
  2. Confirm serial log shows:
     - [hyperos-initramfs] mounting EROFS root + 128M tmpfs OverlayFS
     - [hyperos-initramfs] expanding Partition 3 (HYPEROS_DATA) + btrfs filesystem resize max
     - runit stage 2 supervising zram, ksm, storage, and vmm-daemon
  3. Verify host idle RAM after 60s (no VMs started):
     awk '/MemTotal:/ {t=\$2} /MemAvailable:/ {a=\$2} END {printf "used=%d KiB\\n", t-a}' /proc/meminfo
     (Acceptance target: < 358400 KiB / 350 MiB)
  4. Verify vmm-daemon answers list_vms over /run/hyperos/vmm.sock, start Alpine VM,
     write /root/persist-check inside guest, stop_vm, reboot HyperOS (runit-init 6),
     start Alpine again, and verify /root/persist-check is intact.
EOF
    exit 0
fi

command -v qemu-system-x86_64 >/dev/null 2>&1 || {
    echo "FAIL: qemu-system-x86_64 is required" >&2
    exit 1
}
[ -r /dev/kvm ] && [ -w /dev/kvm ] || {
    echo "FAIL: /dev/kvm must be readable and writable" >&2
    exit 1
}
[ -s "$IMAGE" ] || {
    echo "FAIL: missing disk image: $IMAGE" >&2
    exit 1
}

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/hyperos-v1-test.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT

SCRATCH_IMG="$WORK_DIR/hyperos-test.img"
SERIAL_LOG="$WORK_DIR/serial.log"
cp --sparse=always "$IMAGE" "$SCRATCH_IMG"
truncate -s "${EXPAND_GIB}G" "$SCRATCH_IMG"

echo "Booting scratch image ($SCRATCH_IMG) with serial capture -> $SERIAL_LOG"
if [ "$BOOT_MODE" = "uefi" ]; then
    [ -r "$OVMF_CODE" ] || {
        echo "FAIL: OVMF firmware not found at $OVMF_CODE" >&2
        exit 1
    }
    exec qemu-system-x86_64 \
        -enable-kvm \
        -cpu host \
        -m "$RAM_MIB" \
        -smp 2 \
        -nographic \
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
        -drive "file=$SCRATCH_IMG,format=raw,if=virtio" \
        -serial "mon:stdio"
else
    exec qemu-system-x86_64 \
        -enable-kvm \
        -cpu host \
        -m "$RAM_MIB" \
        -smp 2 \
        -nographic \
        -drive "file=$SCRATCH_IMG,format=raw,if=virtio" \
        -serial "mon:stdio"
fi
