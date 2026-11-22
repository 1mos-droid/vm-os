#!/usr/bin/env bash
# Reproducible Alpine Linux virtio guest base-image builder for HyperOS.
#
# Produces an ARCHITECTURE.md-compliant base image directory:
#   <OUT_DIR>/<IMAGE_ID>/
#     disk.raw       # sparse raw bootable MBR+ext4 guest disk
#     manifest.json  # {"image_id": "...", "sha256": "<64-hex>", "format": "raw"}
#
# Guest features configured:
#   - virtio-blk / virtio-pci / virtio-net / virtio-console (ttyS0, hvc0)
#   - virtio-balloon (for vmm-daemon set_balloon_ram dynamic memory control)
#   - virtio-fs host sharing (tag "hyperos-share" -> /mnt/host, nofail)
#   - Wayland and PipeWire forwarding over virtio-vsock (CID 2, ports 5000/5001)
set -euo pipefail

ALPINE_BRANCH="${ALPINE_BRANCH:-v3.21}"
ALPINE_VERSION="${ALPINE_VERSION:-3.21.3}"
ALPINE_MIRROR="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}"
IMAGE_ID="${IMAGE_ID:-alpine-3-21-virtio}"
DISK_SIZE_MIB="${DISK_SIZE_MIB:-2048}"
OUT_DIR="${OUT_DIR:-$(pwd)/out}"
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-1735689600}"
WAYLAND_VSOCK_PORT="${WAYLAND_VSOCK_PORT:-5000}"
PIPEWIRE_VSOCK_PORT="${PIPEWIRE_VSOCK_PORT:-5001}"
VIRTIOFS_TAG="${VIRTIOFS_TAG:-hyperos-share}"
DRY_RUN=0

# Deterministic partition & filesystem identifiers for reproducible output
MBR_DISK_ID="0x48504131"
ROOT_PARTUUID="48504131-01"
ROOT_FS_UUID="48595045-414c-4033-8231-000000000001"

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Build a reproducible, minimal Alpine Linux virtio guest image for HyperOS.

Options:
  --out-dir <DIR>       Output parent directory (default: ${OUT_DIR})
  --image-id <ID>       Base image ID matching ^[A-Za-z0-9_-]{1,64}\$ (default: ${IMAGE_ID})
  --size-mib <MIB>      Virtual sparse disk size in MiB (default: ${DISK_SIZE_MIB})
  --mirror <URL>        Alpine Linux mirror URL (default: ${ALPINE_MIRROR})
  --dry-run             Validate arguments, host tools, and print build plan without building
  -h, --help            Show this help message

Environment overrides:
  ALPINE_BRANCH, ALPINE_VERSION, SOURCE_DATE_EPOCH,
  WAYLAND_VSOCK_PORT, PIPEWIRE_VSOCK_PORT, VIRTIOFS_TAG
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --out-dir)    OUT_DIR="$2"; shift 2 ;;
        --image-id)   IMAGE_ID="$2"; shift 2 ;;
        --size-mib)   DISK_SIZE_MIB="$2"; shift 2 ;;
        --mirror)     ALPINE_MIRROR="$2"; shift 2 ;;
        --dry-run)    DRY_RUN=1; shift ;;
        -h|--help)    usage; exit 0 ;;
        *)            echo "ERROR: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if ! [[ "${IMAGE_ID}" =~ ^[A-Za-z0-9_-]{1,64}$ ]]; then
    echo "ERROR: image_id '${IMAGE_ID}' must match ^[A-Za-z0-9_-]{1,64}$ (ARCHITECTURE.md)" >&2
    exit 2
fi

if ! [[ "${DISK_SIZE_MIB}" =~ ^[0-9]+$ ]] || [ "${DISK_SIZE_MIB}" -lt 512 ]; then
    echo "ERROR: --size-mib must be an integer >= 512" >&2
    exit 2
fi

REQUIRED_CMDS=(curl tar sfdisk mkfs.ext4 e2fsck losetup mount umount chroot sha256sum truncate dd)
missing=()
for cmd in "${REQUIRED_CMDS[@]}"; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        missing+=("${cmd}")
    fi
done

TARGET_DIR="${OUT_DIR}/${IMAGE_ID}"
DISK_RAW="${TARGET_DIR}/disk.raw"
MANIFEST_JSON="${TARGET_DIR}/manifest.json"
MINIROOTFS_TARBALL="alpine-minirootfs-${ALPINE_VERSION}-x86_64.tar.gz"
MINIROOTFS_URL="${ALPINE_MIRROR}/${ALPINE_BRANCH}/releases/x86_64/${MINIROOTFS_TARBALL}"

if [ "${DRY_RUN}" -eq 1 ]; then
    cat <<EOF
[dry-run] Alpine HyperOS guest build plan:
  image_id:            ${IMAGE_ID}
  alpine_version:      ${ALPINE_VERSION} (${ALPINE_BRANCH})
  minirootfs_url:      ${MINIROOTFS_URL}
  disk_size_mib:       ${DISK_SIZE_MIB} (sparse raw)
  output_disk:         ${DISK_RAW}
  output_manifest:     ${MANIFEST_JSON}
  root_partuuid:       ${ROOT_PARTUUID}
  virtiofs_tag:        ${VIRTIOFS_TAG} -> /mnt/host
  wayland_vsock:       cid=2 port=${WAYLAND_VSOCK_PORT} -> /run/hyperos-guest/wayland-0
  pipewire_vsock:      cid=2 port=${PIPEWIRE_VSOCK_PORT} -> /run/hyperos-guest/pipewire-0
  source_date_epoch:   ${SOURCE_DATE_EPOCH}
EOF
    if [ "${#missing[@]}" -gt 0 ]; then
        echo "[dry-run] Missing host commands (install before actual build): ${missing[*]}"
    else
        echo "[dry-run] All required host commands are present."
    fi
    exit 0
fi

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: building and mounting loop-backed rootfs requires root privileges (use sudo)." >&2
    exit 1
fi

if [ "${#missing[@]}" -gt 0 ]; then
    echo "ERROR: missing required host commands: ${missing[*]}" >&2
    exit 1
fi

WORK_DIR="$(mktemp -d -t hyperos-alpine-build.XXXXXX)"
MOUNT_DIR="${WORK_DIR}/mnt"
LOOP_DEV=""

cleanup() {
    set +e
    for sub in dev/pts dev sys proc; do
        if mountpoint -q "${MOUNT_DIR}/${sub}" 2>/dev/null; then
            umount -lf "${MOUNT_DIR}/${sub}"
        fi
    done
    if mountpoint -q "${MOUNT_DIR}" 2>/dev/null; then
        umount "${MOUNT_DIR}"
    fi
    if [ -n "${LOOP_DEV}" ]; then
        losetup -d "${LOOP_DEV}"
    fi
    rm -rf "${WORK_DIR}"
}
trap cleanup EXIT

mkdir -p "${TARGET_DIR}" "${MOUNT_DIR}"

echo "==> Fetching pinned Alpine minirootfs ${ALPINE_VERSION} and verifying SHA-256"
curl --fail --location --retry 3 "${MINIROOTFS_URL}" -o "${WORK_DIR}/${MINIROOTFS_TARBALL}"
curl --fail --location --retry 3 "${MINIROOTFS_URL}.sha256" -o "${WORK_DIR}/${MINIROOTFS_TARBALL}.sha256"
(
    cd "${WORK_DIR}"
    sha256sum -c "${MINIROOTFS_TARBALL}.sha256"
)

echo "==> Allocating sparse ${DISK_SIZE_MIB} MiB raw disk and MBR partition table"
STAGE_DISK="${WORK_DIR}/disk.raw"
truncate -s "${DISK_SIZE_MIB}M" "${STAGE_DISK}"
sfdisk --quiet "${STAGE_DISK}" <<EOF
label: dos
label-id: ${MBR_DISK_ID}
unit: sectors

start=2048, type=83, bootable
EOF

LOOP_DEV="$(losetup --find --show --partscan "${STAGE_DISK}")"
ROOT_PART="${LOOP_DEV}p1"

echo "==> Formatting ext4 root filesystem (UUID=${ROOT_FS_UUID})"
mkfs.ext4 -q -F -L "hyperos-alpine" -U "${ROOT_FS_UUID}" -O "^metadata_csum_seed" "${ROOT_PART}"
mount -o noatime "${ROOT_PART}" "${MOUNT_DIR}"

echo "==> Extracting Alpine minirootfs"
tar -xzf "${WORK_DIR}/${MINIROOTFS_TARBALL}" -C "${MOUNT_DIR}"

# Bind pseudo-filesystems for chroot package installation
mkdir -p "${MOUNT_DIR}/proc" "${MOUNT_DIR}/sys" "${MOUNT_DIR}/dev/pts" "${MOUNT_DIR}/etc"
cp /etc/resolv.conf "${MOUNT_DIR}/etc/resolv.conf"
mount --bind /proc "${MOUNT_DIR}/proc"
mount --bind /sys "${MOUNT_DIR}/sys"
mount --bind /dev "${MOUNT_DIR}/dev"
mount --bind /dev/pts "${MOUNT_DIR}/dev/pts"

cat > "${MOUNT_DIR}/etc/apk/repositories" <<EOF
${ALPINE_MIRROR}/${ALPINE_BRANCH}/main
${ALPINE_MIRROR}/${ALPINE_BRANCH}/community
EOF

echo "==> Installing virtio kernel, syslinux, and minimal Wayland/PipeWire vsock tooling"
chroot "${MOUNT_DIR}" /bin/sh -eux <<'CHROOT_EOF'
apk update
apk add --no-cache \
    alpine-base \
    linux-virt \
    syslinux \
    e2fsprogs \
    util-linux \
    iproute2 \
    dhcpcd \
    socat \
    wayland-libs-client \
    pipewire-libs

# Ensure initramfs includes all virtio drivers (blk, pci, net, console, balloon, fs, vsock)
cat > /etc/mkinitfs/mkinitfs.conf <<'MKINITFS'
features="base ext4 virtio"
MKINITFS

kver="$(ls /lib/modules | head -n 1)"
mkinitfs -o /boot/initramfs-virt "${kver}"

# Install extlinux bootloader for standalone BIOS raw-disk boot
extlinux --install /boot
CHROOT_EOF

# Install syslinux MBR boot code into the raw image
if [ -f "${MOUNT_DIR}/usr/share/syslinux/mbr.bin" ]; then
    dd if="${MOUNT_DIR}/usr/share/syslinux/mbr.bin" of="${LOOP_DEV}" bs=440 count=1 conv=notrunc status=none
fi

cat > "${MOUNT_DIR}/boot/extlinux.conf" <<EOF
DEFAULT hyperos-alpine
PROMPT 0
TIMEOUT 10

LABEL hyperos-alpine
    KERNEL /boot/vmlinuz-virt
    INITRD /boot/initramfs-virt
    APPEND root=PARTUUID=${ROOT_PARTUUID} rootfstype=ext4 rw quiet console=ttyS0,115200 console=hvc0
EOF

echo "==> Configuring virtio-balloon, virtio-fs, and vsock Wayland/PipeWire forwarding"
cat > "${MOUNT_DIR}/etc/hostname" <<'EOF'
hyperos-alpine
EOF

cat > "${MOUNT_DIR}/etc/modules" <<'EOF'
virtio_pci
virtio_blk
virtio_net
virtio_console
virtio_balloon
virtiofs
vsock
vmw_vsock_virtio_transport
EOF

mkdir -p "${MOUNT_DIR}/mnt/host" "${MOUNT_DIR}/etc/profile.d" "${MOUNT_DIR}/run/hyperos-guest"

cat > "${MOUNT_DIR}/etc/fstab" <<EOF
PARTUUID=${ROOT_PARTUUID}  /          ext4      rw,noatime                0 1
${VIRTIOFS_TAG}            /mnt/host  virtiofs  rw,nofail,_netdev         0 0
EOF

cat > "${MOUNT_DIR}/etc/hyperos-guest.conf" <<EOF
# HyperOS guest forwarding parameters (virtio-vsock & virtio-fs)
HOST_VSOCK_CID=2
WAYLAND_VSOCK_PORT=${WAYLAND_VSOCK_PORT}
PIPEWIRE_VSOCK_PORT=${PIPEWIRE_VSOCK_PORT}
HYPEROS_RUNTIME_DIR=/run/hyperos-guest
VIRTIOFS_TAG=${VIRTIOFS_TAG}
VIRTIOFS_MOUNT=/mnt/host
EOF

cat > "${MOUNT_DIR}/etc/profile.d/hyperos-guest.sh" <<'EOF'
export XDG_RUNTIME_DIR=/run/hyperos-guest
export WAYLAND_DISPLAY=wayland-0
export PIPEWIRE_REMOTE=pipewire-0
EOF
chmod 0644 "${MOUNT_DIR}/etc/profile.d/hyperos-guest.sh"

cat > "${MOUNT_DIR}/etc/init.d/hyperos-virtiofs" <<'EOF'
#!/sbin/openrc-run
description="Mount HyperOS host virtio-fs share if attached"

depend() {
    need localmount modules
}

start() {
    [ -f /etc/hyperos-guest.conf ] && . /etc/hyperos-guest.conf
    tag="${VIRTIOFS_TAG:-hyperos-share}"
    target="${VIRTIOFS_MOUNT:-/mnt/host}"
    mkdir -p "${target}"
    if mountpoint -q "${target}"; then
        return 0
    fi
    # Gracefully succeed if crosvm was launched without a shared directory.
    mount -t virtiofs "${tag}" "${target}" 2>/dev/null || true
    return 0
}
EOF
chmod 0755 "${MOUNT_DIR}/etc/init.d/hyperos-virtiofs"

cat > "${MOUNT_DIR}/etc/init.d/hyperos-vsock-bridge" <<'EOF'
#!/sbin/openrc-run
description="Forward Wayland and PipeWire Unix sockets over virtio-vsock to HyperOS host"

depend() {
    need localmount modules
}

start() {
    [ -f /etc/hyperos-guest.conf ] && . /etc/hyperos-guest.conf
    cid="${HOST_VSOCK_CID:-2}"
    wl_port="${WAYLAND_VSOCK_PORT:-5000}"
    pw_port="${PIPEWIRE_VSOCK_PORT:-5001}"
    rdir="${HYPEROS_RUNTIME_DIR:-/run/hyperos-guest}"

    mkdir -p "${rdir}"
    chmod 0755 "${rdir}"
    rm -f "${rdir}/wayland-0" "${rdir}/pipewire-0"

    start-stop-daemon --start --background --make-pidfile \
        --pidfile /run/hyperos-wayland-vsock.pid \
        --exec /usr/bin/socat -- \
        "UNIX-LISTEN:${rdir}/wayland-0,fork,mode=0660" "VSOCK-CONNECT:${cid}:${wl_port}"

    start-stop-daemon --start --background --make-pidfile \
        --pidfile /run/hyperos-pipewire-vsock.pid \
        --exec /usr/bin/socat -- \
        "UNIX-LISTEN:${rdir}/pipewire-0,fork,mode=0660" "VSOCK-CONNECT:${cid}:${pw_port}"
}

stop() {
    start-stop-daemon --stop --quiet --pidfile /run/hyperos-wayland-vsock.pid || true
    start-stop-daemon --stop --quiet --pidfile /run/hyperos-pipewire-vsock.pid || true
    rm -f /run/hyperos-wayland-vsock.pid /run/hyperos-pipewire-vsock.pid
}
EOF
chmod 0755 "${MOUNT_DIR}/etc/init.d/hyperos-vsock-bridge"

# Enable serial consoles (ttyS0 + hvc0) for crosvm console attachment
cat > "${MOUNT_DIR}/etc/inittab" <<'EOF'
::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default
ttyS0::respawn:/sbin/getty -L 115200 ttyS0 vt100
hvc0::respawn:/sbin/getty -L 115200 hvc0 vt100
::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
EOF

# A serial marker proves that the guest reached userspace under the host kernel.
mkdir -p "${MOUNT_DIR}/etc/local.d"
cat > "${MOUNT_DIR}/etc/local.d/hyperos-ready.start" <<'EOF'
#!/bin/sh
echo HYPEROS_GUEST_READY >/dev/ttyS0
# The opt-in host acceptance service listens here; normal boots need no listener.
(printf 'HYPEROS_GUEST_READY\n' | socat -T 5 - VSOCK-CONNECT:2:5002) >/dev/null 2>&1 &
EOF
chmod 0755 "${MOUNT_DIR}/etc/local.d/hyperos-ready.start"

# Enable essential minimal OpenRC services inside chroot
chroot "${MOUNT_DIR}" /bin/sh -eux <<'CHROOT_SVC'
rc-update add devfs sysinit
rc-update add dmesg sysinit
rc-update add mdev sysinit
rc-update add modules boot
rc-update add hostname boot
rc-update add local default
rc-update add bootmisc boot
rc-update add hyperos-virtiofs default
rc-update add hyperos-vsock-bridge default
# Remove ephemeral resolv.conf and package caches for clean reproducibility
rm -f /etc/resolv.conf
rm -rf /var/cache/apk/* /tmp/*
CHROOT_SVC

# Clamp mtimes for reproducible image builds
find "${MOUNT_DIR}" -xdev -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} + 2>/dev/null || true

# Unmount cleanly before sparsifying and checksumming
for sub in dev/pts dev sys proc; do
    umount "${MOUNT_DIR}/${sub}"
done
sync
umount "${MOUNT_DIR}"
e2fsck -fy "${ROOT_PART}" >/dev/null 2>&1 || true
losetup -d "${LOOP_DEV}"
LOOP_DEV=""

if command -v fallocate >/dev/null 2>&1; then
    fallocate --dig-holes "${STAGE_DISK}" || true
fi

mv -f "${STAGE_DISK}" "${DISK_RAW}"
chmod 0644 "${DISK_RAW}"

SHA256_HEX="$(sha256sum "${DISK_RAW}" | awk '{print $1}')"
cat > "${MANIFEST_JSON}" <<EOF
{"image_id":"${IMAGE_ID}","sha256":"${SHA256_HEX}","format":"raw"}
EOF
chmod 0644 "${MANIFEST_JSON}"

echo "==> Built Alpine guest base image successfully:"
echo "    Disk:     ${DISK_RAW}"
echo "    Manifest: ${MANIFEST_JSON}"
echo "    SHA-256:  ${SHA256_HEX}"
