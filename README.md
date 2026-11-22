# HyperOS (`1mos-droid/vm-os`)

HyperOS is a minimal, security-focused `x86_64` virtualization host OS built to boot directly from a USB pendrive or internal drive (**UEFI and Legacy BIOS**) and run isolated `crosvm` virtual machines efficiently on **4 GiB RAM** machines with VT-x/AMD-V.

- **Hybrid UEFI + Legacy BIOS Bootable Image (`hyperos.img`):**
  - **Partition 1 (`100M`, FAT32 ESP `HYPEROS_EFI`):** GRUB2 for both UEFI (`EFI/BOOT/BOOTX64.EFI`) and Legacy BIOS, offering **Persistent Mode** and **Stateless RAM-Only Mode** at boot.
  - **Partition 2 (`512M`, read-only `EROFS`):** LZ4HC-compressed, deduplicated immutable root filesystem combined at boot with a `128M` `tmpfs` `OverlayFS` upper layer so host logs and temp writes never wear USB flash.
  - **Partition 3 (`512M` initial, Btrfs `HYPEROS_DATA`):** Mounted at `/var/lib/hyperos` with `compress=zstd:3,noatime,commit=120`. In **Persistent Mode**, the early-boot initramfs **automatically expands Partition 3 and grows Btrfs (`btrfs filesystem resize max`) into trailing unallocated space on first boot**. In **Stateless RAM-Only Mode**, `/var/lib/hyperos` runs on a dedicated ZRAM-backed Btrfs volume without touching any disk.
- **Host Idle RAM Acceptance Target:** `< 350 MiB` used RAM (`MemTotal - MemAvailable`) before any VM starts (measured on a booted target image — publish headless and `desktop-shell` measurements from real runs before treating this target as a benchmark claim).
- **Memory & Storage Efficiency:** `zstd` ZRAM swap (`/dev/zram0`, 65% of `MemTotal`, priority 100), `virtio-balloon` dynamic memory control, strict `MemTotal - 768 MiB` RAM admission control, and zero-overhead Btrfs copy-on-write (`cp --reflink=always` / `FICLONE`) VM disks with cold read-only subvolume snapshots.
- **v1 Hardware Profile:** Built-in monolithic Linux `6.12.20` drivers for QEMU/KVM (`virtio-*`), USB 3.0 UAS, NVMe, AHCI, I2C HID touchpads, and Intel (`i915`), AMD (`amdgpu`), and `simpledrm` displays. Wireless (`WLAN`) and local ALSA `SOUND` are intentionally disabled in the v1 monolithic kernel profile; `desktop-shell` and `vm-control-ui` probe real sysfs/procfs capabilities and mark Wi-Fi/Audio unavailable when absent.
- **KSM off:** the kernel has `CONFIG_KSM`, but `/etc/sv/ksm/run` sets `run=0` and guest memory is never marked `MADV_MERGEABLE`, because cross-VM page sharing is a side channel.
- **Contract-Driven Integration:** Every Unix socket path, JSON-RPC 2.0 schema, error code, and storage path is specified verbatim in [`ARCHITECTURE.md`](ARCHITECTURE.md).

> **Warning before writing `hyperos.img` to a drive:** Flashing `hyperos.img` overwrites the target drive's partition table, and booting **Persistent Mode** automatically expands Partition 3 (`HYPEROS_DATA`) into any unallocated space behind it on first boot. Use a dedicated USB pendrive, internal drive, or scratch disk image.

## Current Status & Known Gaps

HyperOS is a working virtualization host foundation, not yet a finished security or research OS. What the default image does **not** do yet:

- **Run a GUI by default.** `desktop-shell` (DRM/KMS) and VM Studio have optional Buildroot packages and an unprivileged session, but they are off by default until the graphical session is booted on real hardware. See `kernel-buildroot/README.md`.
- **Run crosvm unprivileged.** `vmm-daemon` and `crosvm` run as root (crosvm keeps its jail/seccomp). The control socket itself no longer needs root: members of the `hyperos` group may use it.
- **Verify boot.** No dm-verity, signed Secure Boot or TPM measurement; no A/B updates.

The plan for closing these gaps is in [`docs/REVIEW_FIXES.md`](docs/REVIEW_FIXES.md).

---

## Repository Layout

```text
ARCHITECTURE.md              # Single source of truth for OS, daemon, UI & storage contracts
CONTRIBUTING.md              # Contribution rules, security invariants, and PR checklist
CODE_OF_CONDUCT.md           # Contributor Covenant v2.1
SECURITY.md                  # Private vulnerability disclosure policy & threat boundaries
LICENSE-MIT / LICENSE-APACHE # Dual-license terms (MIT OR Apache-2.0)
kernel-buildroot/            # Buildroot BR2_EXTERNAL tree (Linux 6.12.20, runit, vmm-daemon,
                             #   crosvm packaging, initramfs, hybrid GRUB2, nftables, iproute2)
vmm-daemon/                  # Rust VM daemon supervising crosvm children over Unix RPC
desktop-shell/               # Workspace-First Wayland compositor (Smithay + Slint, --mock; optional image package)
vm-control-ui/               # Zero-Script VM Studio desktop GUI client (Slint, --mock; optional image package)
guest-images/                # Reproducible Alpine & Debian Slim CoW base image builders
docs/                        # Cloud nested-virtualization testing and operational guides
scripts/                     # Config/image validator, v1 release runner, GNU17 CC wrapper
.github/workflows/           # Automated OS image CI (build-os.yml) & vmm-daemon CI
```

---

## Architecture Highlights

### Local JSON-RPC 2.0 Control Plane

- **Transport:** `/run/hyperos/vmm.sock` (`AF_UNIX`/`SOCK_STREAM`, `root:root`, mode `0600`, parent `/run/hyperos` `0700`, `SO_PEERCRED` UID 0 only)
- **Framing:** Single-line UTF-8 JSON-RPC 2.0 object terminated by LF (`\n`), max 65,536 bytes; no batches, notifications, positional params, or unknown keys
- **Methods:** `create_vm`, `start_vm`, `stop_vm`, `pause_vm`, `snapshot_vm`, `set_balloon_ram`, `set_network_mode`, `list_vms`
- **Network sandboxing:** `isolated` (no virtual NIC), `nat` (per-VM TAP + `nftables` outbound masquerade to the public internet only; LAN, link-local and other VMs blocked), or `host-only` (per-VM isolated bridge for host–guest traffic only)
- **Admission control:** Total maximum RAM of running/paused guests must not exceed `MemTotal - 768 MiB` (ZRAM swap is not admission capacity)

### Persistent Btrfs CoW Storage

`/var/lib/hyperos/` is a dedicated Btrfs filesystem (`root:root`, mode `0700`, mounted `compress=zstd:3,noatime,commit=120`):

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

### 2. Build the Hybrid UEFI/BIOS HyperOS Disk Image

Pinned baseline: **Buildroot `2025.02.1`**, **Linux `6.12.20`**, **runit `2.1.2`**, plus Rust stable (`x86_64-unknown-linux-musl` target) for packaging `/usr/bin/vmm-daemon`.

```sh
# Download and verify pinned Buildroot 2025.02.1
curl --fail --location --retry 3 \
  "https://buildroot.org/downloads/buildroot-2025.02.1.tar.xz" -o buildroot.tar.xz
echo 'a3216907e7ce1bbd38413e20b0861641b160ecc40dd7b598febb7f12984b3206  buildroot.tar.xz' | sha256sum -c -
mkdir -p buildroot && tar -xf buildroot.tar.xz -C buildroot --strip-components=1

# Resolve, validate, and build (includes /usr/bin/vmm-daemon, /boot/bzImage, nft, ip)
export HOSTCC="$PWD/scripts/host-gcc-gnu17"
export BR2_CMAKE_CANDIDATES=""
make -C buildroot O="$PWD/output" BR2_EXTERNAL="$PWD/kernel-buildroot" hyperos_x86_64_defconfig
scripts/validate-configs.sh --resolved-buildroot output/.config
make -C buildroot O="$PWD/output" -j2
scripts/validate-configs.sh --resolved-kernel output/build/linux-6.12.20/.config
scripts/validate-configs.sh --image output/images/hyperos.img
```

> **Packaging `crosvm`:** The default image builds pinned musl-linked crosvm and minijail sources with x86_64 seccomp policies. See [image build and acceptance instructions](kernel-buildroot/README.md).

### 3. Run the v1 End-to-End Release Demonstration (4 GiB RAM)

Preview prerequisites without QEMU/KVM:

```sh
scripts/test-v1-release.sh --dry-run
```

Boot a scratch copy (resized to 8 GiB to exercise first-boot `HYPEROS_DATA` auto-expansion) on a KVM-capable host or nested-virtualization cloud VM (see [`docs/CLOUD_TESTING.md`](docs/CLOUD_TESTING.md)):

```sh
# Legacy BIOS boot
scripts/test-v1-release.sh --bios --image output/images/hyperos.img

# UEFI boot (requires OVMF)
scripts/test-v1-release.sh --uefi --image output/images/hyperos.img
```

The v1 release milestone verifies:
1. Boot `hyperos.img` with `-m 4096` and confirm `initramfs` mounts EROFS + `128M` OverlayFS and auto-expands `HYPEROS_DATA`.
2. Capture serial logs and `/proc/meminfo` after 60s idle (target `< 350 MiB` used RAM).
3. Start an Alpine guest via `/usr/bin/vmm-daemon` over `/run/hyperos/vmm.sock`, write a file inside the guest, shut down, reboot HyperOS, and read the file back.

---

## Credits

The v0.2.0 work was split across three AI coding agents working from one plan,
[`docs/REVIEW_FIXES.md`](docs/REVIEW_FIXES.md), under the maintainer's review:

| Agent | Built |
| --- | --- |
| **Claude** (Anthropic) | Docs audit and work plan, the v2 RPC contract, NAT guests limited to the internet, `hyperos` group socket access, disposable VMs, snapshot restore and delete, daemon packet capture |
| **Codex** (OpenAI) | crosvm and minijail built from pinned sources, KSM off by default, guest image import and one-command VM boot, UI and session packaging, verified-boot and A/B update design |
| **Antigravity** (Google) | Musl/KMS-capable desktop shell and VM Studio, non-root socket connection, UI for snapshots, disposable VMs, capture and NAT, data-only themes, app manifest proposal |

Each commit names the agent that wrote it in a `Co-Authored-By` trailer.

---

## Contributing & Community

Contributions are welcome! Please read [`CONTRIBUTING.md`](CONTRIBUTING.md), [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md), and [`ARCHITECTURE.md`](ARCHITECTURE.md) before opening an issue or pull request.

To report a security vulnerability privately, see [`SECURITY.md`](SECURITY.md).

---

## License

HyperOS source code in this repository is dual-licensed under either of:

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option. Upstream third-party components configured and assembled during OS builds (such as the Linux kernel, Buildroot, GRUB2, `runit`, `nftables`, `iproute2`, and `crosvm`) retain their respective upstream licenses.
