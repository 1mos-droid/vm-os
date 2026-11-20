#!/usr/bin/env bash
# Reproducible Debian Slim virtio guest base-image builder for HyperOS.
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

DEBIAN_SUITE="${DEBIAN_SUITE:-bookworm}"
DEBIAN_MIRROR="${DEBIAN_MIRROR:-https://deb.debian.org/debian}"
IMAGE_ID="${IMAGE_ID:-debian-12-slim-virtio}"
DISK_SIZE_MIB="${DISK_SIZE_MIB:-4096}"
OUT_DIR="${OUT_DIR:-$(pwd)/out}"
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-1735689600}"
WAYLAND_VSOCK_PORT="${WAYLAND_VSOCK_PORT:-5000}"
PIPEWIRE_VSOCK_PORT="${PIPEWIRE_VSOCK_PORT:-5001}"
VIRTIOFS_TAG="${VIRTIOFS_TAG:-hyperos-share}"
DRY_RUN=0

# Deterministic partition & filesystem identifiers for reproducible output
MBR_DISK_ID="0x48504431"
ROOT_PARTUUID="48504431-01"
ROOT_FS_UUID="48595045-4445-4231-8200-000000000001"

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Build a reproducible, minimal Debian Slim virtio guest image for HyperOS.

Options:
  --out-dir <DIR>       Output parent directory (default: ${OUT_DIR})
  --image-id <ID>       Base image ID matching ^[A-Za-z0-9_-]{1,64}\$ (default: ${IMAGE_ID})
  --size-mib <MIB>      Virtual sparse disk size in MiB (default: ${DISK_SIZE_MIB})
  --suite <SUITE>       Debian suite (default: ${DEBIAN_SUITE})
  --mirror <URL>        Debian APT mirror URL (default: ${DEBIAN_MIRROR})
  --dry-run             Validate arguments, host tools, and print build plan without building
  -h, --help            Show this help message

Environment overrides:
  DEBIAN_SUITE, DEBIAN_MIRROR, SOURCE_DATE_EPOCH,
  WAYLAND_VSOCK_PORT, PIPEWIRE_VSOCK_PORT, VIRTIOFS_TAG
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --out-dir)    OUT_DIR="$2"; shift 2 ;;
        --image-id)   IMAGE_ID="$2"; shift 2 ;;
        --size-mib)   DISK_SIZE_MIB="$2"; shift 2 ;;
        --suite)      DEBIAN_SUITE="$2"; shift 2 ;;
        --mirror)     DEBIAN_MIRROR="$2"; shift 2 ;;
        --dry-run)    DRY_RUN=1; shift ;;
        -h|--help)    usage; exit 0 ;;
        *)            echo "ERROR: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if ! [[ "${IMAGE_ID}" =~ ^[A-Za-z0-9_-]{1,64}$ ]]; then
    echo "ERROR: image_id '${IMAGE_ID}' must match ^[A-Za-z0-9_-]{1,64}$ (ARCHITECTURE.md)" >&2
    exit 2
fi

if ! [[ "${DISK_SIZE_MIB}" =~ ^[0-9]+$ ]] || [ "${DISK_SIZE_MIB}" -lt 1024 ]; then
    echo "ERROR: --size-mib must be an integer >= 1024" >&2
    exit 2
fi

REQUIRED_CMDS=(debootstrap sfdisk mkfs.ext4 e2fsck losetup mount umount chroot sha256sum truncate dd)
missing=()
for cmd in "${REQUIRED_CMDS[@]}"; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        missing+=("${cmd}")
    fi
done

TARGET_DIR="${OUT_DIR}/${IMAGE_ID}"
DISK_RAW="${TARGET_DIR}/disk.raw"
MANIFEST_JSON="${TARGET_DIR}/manifest.json"

if [ "${DRY_RUN}" -eq 1 ]; then
    cat <<EOF
[dry-run] Debian Slim HyperOS guest build plan:
  image_id:            ${IMAGE_ID}
  debian_suite:        ${DEBIAN_SUITE}
  debian_mirror:       ${DEBIAN_MIRROR}
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
    echo "ERROR: debootstrap and loop-backed rootfs creation require root privileges (use sudo)." >&2
    exit 1
fi

if [ "${#missing[@]}" -gt 0 ]; then
    echo "ERROR: missing required host commands: ${missing[*]}" >&2
    exit 1
fi

WORK_DIR="$(mktemp -d -t hyperos-debian-build.XXXXXX)"
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
mkfs.ext4 -q -F -L "hyperos-debian" -U "${ROOT_FS_UUID}" -O "^metadata_csum_seed" "${ROOT_PART}"
mount -o noatime "${ROOT_PART}" "${MOUNT_DIR}"

echo "==> Bootstrapping Debian ${DEBIAN_SUITE} (minbase variant)"
debootstrap \
    --variant=minbase \
    --arch=amd64 \
    --include=systemd,systemd-sysv,dbus,ca-certificates,iproute2,isc-dhcp-client,kmod,udev \
    "${DEBIAN_SUITE}" \
    "${MOUNT_DIR}" \
    "${DEBIAN_MIRROR}"

# Bind pseudo-filesystems for chroot configuration
mount --bind /proc "${MOUNT_DIR}/proc"
mount --bind /sys "${MOUNT_DIR}/sys"
mount --bind /dev "${MOUNT_DIR}/dev"
mount --bind /dev/pts "${MOUNT_DIR}/dev/pts"

# Strip docs/locales to keep guest rootfs slim
cat > "${MOUNT_DIR}/etc/dpkg/dpkg.cfg.d/01-hyperos-nodoc" <<'EOF'
path-exclude /usr/share/doc/*
path-exclude /usr/share/man/*
path-exclude /usr/share/info/*
path-exclude /usr/share/locale/*
path-include /usr/share/locale/en*
EOF

cat > "${MOUNT_DIR}/etc/apt/sources.list" <<EOF
deb ${DEBIAN_MIRROR} ${DEBIAN_SUITE} main
deb ${DEBIAN_MIRROR} ${DEBIAN_SUITE}-updates main
deb https://security.debian.org/debian-security ${DEBIAN_SUITE}-security main
EOF

# Ensure virtio modules are baked into initramfs and loaded on boot
mkdir -p "${MOUNT_DIR}/etc/initramfs-tools" "${MOUNT_DIR}/etc/modules-load.d"
cat > "${MOUNT_DIR}/etc/initramfs-tools/modules" <<'EOF'
virtio_pci
virtio_blk
virtio_net
virtio_console
virtio_balloon
virtiofs
vsock
vmw_vsock_virtio_transport
EOF

cat > "${MOUNT_DIR}/etc/modules-load.d/hyperos-virtio.conf" <<'EOF'
virtio_balloon
virtiofs
vsock
vmw_vsock_virtio_transport
EOF

echo "==> Installing cloud virtio kernel, extlinux, and Wayland/PipeWire vsock runtime"
chroot "${MOUNT_DIR}" /usr/bin/env DEBIAN_FRONTEND=noninteractive /bin/sh -eux <<'CHROOT_EOF'
apt-get update
apt-get install -y --no-install-recommends \
    linux-image-cloud-amd64 \
    extlinux \
    syslinux-common \
    socat \
    libwayland-client0 \
    libpipewire-0.3-0

update-initramfs -u -k all
extlinux --install /boot

# Clean apt caches and ephemeral machine-id for reproducible base images
apt-get clean
rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*
: > /etc/machine-id
CHROOT_EOF

# Install syslinux MBR boot code into the raw image
if [ -f "${MOUNT_DIR}/usr/lib/syslinux/mbr/mbr.bin" ]; then
    dd if="${MOUNT_DIR}/usr/lib/syslinux/mbr/mbr.bin" of="${LOOP_DEV}" bs=440 count=1 conv=notrunc status=none
fi

KERNEL_REL="$(find "${MOUNT_DIR}/boot" -maxdepth 1 -name 'vmlinuz-*' -printf '%f\n' | sort | tail -n 1)"
INITRD_REL="$(find "${MOUNT_DIR}/boot" -maxdepth 1 -name 'initrd.img-*' -printf '%f\n' | sort | tail -n 1)"

cat > "${MOUNT_DIR}/boot/extlinux.conf" <<EOF
DEFAULT hyperos-debian
PROMPT 0
TIMEOUT 10

LABEL hyperos-debian
    KERNEL /boot/${KERNEL_REL}
    INITRD /boot/${INITRD_REL}
    APPEND root=PARTUUID=${ROOT_PARTUUID} rootfstype=ext4 rw quiet console=ttyS0,115200 console=hvc0
EOF

echo "==> Configuring virtio-fs mount, serial consoles, and systemd vsock bridges"
cat > "${MOUNT_DIR}/etc/hostname" <<'EOF'
hyperos-debian
EOF

mkdir -p "${MOUNT_DIR}/mnt/host" "${MOUNT_DIR}/etc/profile.d" "${MOUNT_DIR}/etc/systemd/system"

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

cat > "${MOUNT_DIR}/etc/systemd/system/hyperos-wayland-vsock.service" <<EOF
[Unit]
Description=Forward guest Wayland Unix socket over virtio-vsock to HyperOS host
After=systemd-modules-load.service local-fs.target

[Service]
Type=simple
EnvironmentFile=-/etc/hyperos-guest.conf
RuntimeDirectory=hyperos-guest
RuntimeDirectoryMode=0755
RuntimeDirectoryPreserve=yes
ExecStartPre=/bin/rm -f /run/hyperos-guest/wayland-0
ExecStart=/usr/bin/socat UNIX-LISTEN:/run/hyperos-guest/wayland-0,fork,mode=0660 VSOCK-CONNECT:\${HOST_VSOCK_CID}:\${WAYLAND_VSOCK_PORT}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF

cat > "${MOUNT_DIR}/etc/systemd/system/hyperos-pipewire-vsock.service" <<EOF
[Unit]
Description=Forward guest PipeWire Unix socket over virtio-vsock to HyperOS host
After=systemd-modules-load.service local-fs.target

[Service]
Type=simple
EnvironmentFile=-/etc/hyperos-guest.conf
RuntimeDirectory=hyperos-guest
RuntimeDirectoryMode=0755
RuntimeDirectoryPreserve=yes
ExecStartPre=/bin/rm -f /run/hyperos-guest/pipewire-0
ExecStart=/usr/bin/socat UNIX-LISTEN:/run/hyperos-guest/pipewire-0,fork,mode=0660 VSOCK-CONNECT:\${HOST_VSOCK_CID}:\${PIPEWIRE_VSOCK_PORT}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF

# Enable systemd services & serial consoles inside the image
chroot "${MOUNT_DIR}" /bin/sh -eux <<'CHROOT_ENABLE'
systemctl enable serial-getty@ttyS0.service
systemctl enable serial-getty@hvc0.service
systemctl enable hyperos-wayland-vsock.service
systemctl enable hyperos-pipewire-vsock.service
rm -f /etc/resolv.conf
CHROOT_ENABLE

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

echo "==> Built Debian Slim guest base image successfully:"
echo "    Disk:     ${DISK_RAW}"
echo "    Manifest: ${MANIFEST_JSON}"
echo "    SHA-256:  ${SHA256_HEX}"
