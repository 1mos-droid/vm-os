# HyperOS (`1mos-droid/vm-os`)

HyperOS is a minimal, security-focused `x86_64` virtualization host OS built to run isolated `crosvm` virtual machines efficiently on **4 GiB RAM** machines with VT-x/AMD-V and BIOS/CSM boot.

- **Host idle RAM target:** `< 350 MiB` used RAM (`MemTotal - MemAvailable`) before any VM starts
- **Minimal attack surface:** Read-only 128 MiB ext4 root, `runit` PID 1 supervision, no default login shell, and no default network listeners
- **Memory & storage efficiency:** `zstd` ZRAM swap (65% of `MemTotal`, priority 100), global KSM page merging, `virtio-balloon` dynamic memory control, and zero-overhead Btrfs copy-on-write (`FICLONE`/reflink) VM disks with cold read-only subvolume snapshots
- **Contract-driven integration:** Every Unix socket path, JSON-RPC 2.0 schema, error code, and storage path is specified verbatim in [`ARCHITECTURE.md`](ARCHITECTURE.md)

---

## Repository Layout

```text
ARCHITECTURE.md              # Single source of truth for OS, daemon, UI & storage contracts
CONTRIBUTING.md              # Contribution rules, security invariants, and PR checklist
kernel-buildroot/            # Buildroot BR2_EXTERNAL tree (Linux 6.12.20, runit 2.1.2, GRUB)
vmm-daemon/                  # Rust VM daemon supervising crosvm children over Unix RPC
vm-control-ui/               # Lightweight native Rust desktop GUI client (supports --mock)
guest-images/                # Reproducible CoW guest image builders & manifests (no disks in git)
docs/                        # Cloud nested-virtualization testing and operational guides
scripts/                     # Local/CI config validation and GNU17 host compiler wrapper
.github/workflows/           # Automated Buildroot OS image CI (build-os.yml)
```

---

## Architecture Highlights

### Local JSON-RPC 2.0 Control Plane

- **Transport:** `/run/hyperos/vmm.sock` (`AF_UNIX`/`SOCK_STREAM`, `root:root`, mode `0600`, parent `/run/hyperos` `0700`, `SO_PEERCRED` UID 0 only)
- **Framing:** Single-line UTF-8 JSON-RPC 2.0 object terminated by LF (`\n`), max 65,536 bytes; no batches, notifications, positional params, or unknown keys
- **Methods:** `create_vm`, `start_vm`, `stop_vm`, `pause_vm`, `snapshot_vm`, `set_balloon_ram`, `set_network_mode`, `list_vms`
- **Network sandboxing:** `isolated` (no virtual NIC), `nat` (per-VM TAP + outbound masquerade), or `host-only` (per-VM TAP for host–guest traffic only)
- **Admission control:** Total maximum RAM of running/paused guests must not exceed `MemTotal - 768 MiB` (ZRAM swap is not admission capacity)

### Persistent Btrfs CoW Storage

`/var/lib/hyperos/` is a dedicated Btrfs filesystem (`root:root`, mode `0700`, mounted `noatime,compress=zstd`):

```text
/var/lib/hyperos/
  base-images/<image_id>/
    disk.raw                              # Immutable sparse raw bootable guest disk
    manifest.json                         # image_id, sha256 (64 lowercase hex), format="raw"
  instances/<vm_id>/
    metadata.json                         # Atomically replaced + fsynced VM object
    current/                              # Btrfs subvolume
      disk.raw                            # Strict reflink clone of base disk
    snapshots/<snapshot_id>/              # Read-only Btrfs snapshot of current/
    snapshot-metadata/<snapshot_id>.json  # Immutable VM metadata + Snapshot object
```

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the complete, normative specification.

---

## Quick Start

### 1. Validate Repository Configuration

Requires only Python 3 and a POSIX shell (no root or downloads):

```sh
scripts/validate-configs.sh
```

### 2. Build the HyperOS BIOS Disk Image

Pinned baseline: **Buildroot `2025.02.1`**, **Linux `6.12.20`**, **runit `2.1.2`**.

```sh
# Download and verify pinned Buildroot 2025.02.1
curl --fail --location --retry 3 \
  "https://buildroot.org/downloads/buildroot-2025.02.1.tar.xz" -o buildroot.tar.xz
echo 'a3216907e7ce1bbd38413e20b0861641b160ecc40dd7b598febb7f12984b3206  buildroot.tar.xz' | sha256sum -c -
mkdir -p buildroot && tar -xf buildroot.tar.xz -C buildroot --strip-components=1

# Resolve, validate, and build
export HOSTCC="$PWD/scripts/host-gcc-gnu17"
export BR2_CMAKE_CANDIDATES=""
make -C buildroot O="$PWD/output" BR2_EXTERNAL="$PWD/kernel-buildroot" hyperos_x86_64_defconfig
scripts/validate-configs.sh --resolved-buildroot output/.config
make -C buildroot O="$PWD/output" -j2
scripts/validate-configs.sh --resolved-kernel output/build/linux-6.12.20/.config
```

### 3. Boot HyperOS Locally in QEMU/KVM (4 GiB RAM)

```sh
qemu-system-x86_64 \
  -enable-kvm \
  -cpu host \
  -m 4096 \
  -nographic \
  -drive file=output/images/hyperos.img,format=raw,if=virtio
```

> **Note:** `hyperos.img` includes a deliberately small 512 MiB Btrfs starter data partition that must be grown offline before importing real guest images. It also uses a fixed MBR root `PARTUUID`, so attach only one copy per machine.

---

## Contributing & Community

Contributions are welcome! Please read [`CONTRIBUTING.md`](CONTRIBUTING.md), [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md), and [`ARCHITECTURE.md`](ARCHITECTURE.md) before opening an issue or pull request.

To report a security vulnerability privately, see [`SECURITY.md`](SECURITY.md).

---

## License

HyperOS source code in this repository is dual-licensed under either of:

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option. Upstream third-party components configured and assembled during OS builds (such as the Linux kernel, Buildroot, GRUB2, `runit`, and `crosvm`) retain their respective upstream licenses.
