# HyperOS CoW Guest Base Images (`guest-images/`)

This directory contains reproducible builders for minimal `virtio`-enabled guest base images consumed by HyperOS (`vmm-daemon` + `crosvm`). No binary guest disk images are stored in git.

## Builders

| Script | Default `image_id` | Guest Profile |
| --- | --- | --- |
| [`build-alpine-guest.sh`](build-alpine-guest.sh) | `alpine-3-21-virtio` | Minimal Alpine Linux 3.21 (`musl`, `linux-virt`, OpenRC) |
| [`build-debian-slim-guest.sh`](build-debian-slim-guest.sh) | `debian-12-slim-virtio` | Minimal Debian 12 Bookworm (`minbase`, `linux-image-cloud-amd64`, systemd) |

Both scripts produce an `ARCHITECTURE.md`-compliant output directory containing a sparse raw bootable disk image and a validated manifest:

```text
<OUT_DIR>/<image_id>/
  disk.raw        # sparse raw bootable MBR + ext4 guest disk
  manifest.json   # {"image_id":"<image_id>","sha256":"<64 lowercase hex>","format":"raw"}
```

### Usage

Preview the build plan and verify host prerequisites without root:

```sh
./guest-images/build-alpine-guest.sh --dry-run
./guest-images/build-debian-slim-guest.sh --dry-run
```

Build the base images (requires root for `losetup`, `mount`, and `chroot`):

```sh
sudo ./guest-images/build-alpine-guest.sh --out-dir ./out
sudo ./guest-images/build-debian-slim-guest.sh --out-dir ./out
```

Both scripts honor `SOURCE_DATE_EPOCH`, pin deterministic MBR `PARTUUID` and ext4 filesystem UUIDs, clean package caches, and sparsify empty blocks with `fallocate --dig-holes` before hashing `disk.raw`.

## Guest `virtio` Features Configured

Each generated guest image is preconfigured for HyperOS controlled VMs:

1. **`virtio-balloon`:** Loads `virtio_balloon` at boot so `vmm-daemon`'s `set_balloon_ram` RPC (`balloon_bytes = (maximum - desired) * 1048576`) can reclaim or return guest RAM dynamically via `crosvm`.
2. **`virtio-fs` host sharing:** Loads `virtiofs` and mounts tag `hyperos-share` at `/mnt/host` with `nofail` (boots cleanly even when no shared directory is attached).
3. **Wayland & PipeWire forwarding over `virtio-vsock`:** Loads `vsock` and `vmw_vsock_virtio_transport`, exports `XDG_RUNTIME_DIR=/run/hyperos-guest`, `WAYLAND_DISPLAY=wayland-0`, and `PIPEWIRE_REMOTE=pipewire-0` in `/etc/profile.d/hyperos-guest.sh`, and runs lightweight `socat` bridges from guest Unix sockets to host CID `2` (`WAYLAND_VSOCK_PORT=5000`, `PIPEWIRE_VSOCK_PORT=5001`, configurable in `/etc/hyperos-guest.conf`).
4. **Serial console & virtio storage/net:** Configured with `virtio_pci`, `virtio_blk`, `virtio_net`, and `virtio_console` (`ttyS0` and `hvc0`).

## Persistent CoW Storage Layout & Btrfs Reflinks

Per `ARCHITECTURE.md`, `/var/lib/hyperos/` is a dedicated Btrfs filesystem (`root:root`, mode `0700`, mounted with `noatime,compress=zstd`):

```text
/var/lib/hyperos/
  base-images/<image_id>/
    disk.raw                              # immutable sparse raw base disk
    manifest.json                         # image_id, sha256 (64 lowercase hex), format="raw"
  instances/<vm_id>/
    metadata.json                         # VM object, atomically replaced + fsync
    current/                              # Btrfs subvolume
      disk.raw                            # reflink clone of base disk; never write the base
    snapshots/<snapshot_id>/              # read-only snapshot of current subvolume
    snapshot-metadata/<snapshot_id>.json  # immutable VM metadata + Snapshot object
```

### Importing a Base Image

Import a builder output into an **offline** host image:

```sh
sudo scripts/import-guest-image.py out/alpine-3-21-virtio output/images/hyperos.img
```

The host needs Python 3, GNU `cp`, `losetup`, `blkid`, and Btrfs mounting support.
The tool validates the image ID and SHA-256 before mounting, verifies the copy,
preserves sparse extents, installs read-only files, and publishes atomically.
Existing IDs are never overwritten. The destination must have the current
partition-3 `HYPEROS_DATA` Btrfs layout and must not be in use by a VM or mounted
elsewhere. Unmount or loop-detach failures are errors and preserve the data.

After booting HyperOS, run `hyperos-vm alpine-3-21-virtio` as root or a member of
`hyperos` to create and start a 512 MiB isolated VM. For a complete host-side
scratch build/import/boot check, use
`sudo scripts/test-v1-release.sh --image output/images/hyperos.img`.

The daemon directly boots the host's `/boot/bzImage` with `root=/dev/vda1`.
Its built-in virtio block, ext4 and console drivers are therefore required; the
guest's installed kernel and initramfs are not used by this path. The Alpine
builder includes an OpenRC userspace marker, sent to the serial console and,
when the opt-in release-test listener is present, host vsock port 5002. The
release runner requires that marker; a crosvm PID alone does not prove boot.

### Zero-Overhead Cloning with `cp --reflink=always`

When `create_vm` provisions a new VM instance `<vm_id>`, both the base image and the instance live on the **same Btrfs filesystem** (`/var/lib/hyperos`). A per-instance Btrfs subvolume is created and the base disk is cloned with a strict reflink (`FICLONE` ioctl):

```sh
VM_ID="b6f51eed-8dc7-47f6-a8e7-81b6ae783a21"
IMAGE_ID="alpine-3-21-virtio"

install -d -o root -g root -m 0700 \
    "/var/lib/hyperos/instances/${VM_ID}" \
    "/var/lib/hyperos/instances/${VM_ID}/snapshots" \
    "/var/lib/hyperos/instances/${VM_ID}/snapshot-metadata"

btrfs subvolume create "/var/lib/hyperos/instances/${VM_ID}/current"

# Strict CoW clone: shares 100% of underlying Btrfs extents (zero initial disk overhead)
cp --reflink=always --sparse=always \
    "/var/lib/hyperos/base-images/${IMAGE_ID}/disk.raw" \
    "/var/lib/hyperos/instances/${VM_ID}/current/disk.raw"
chmod 0600 "/var/lib/hyperos/instances/${VM_ID}/current/disk.raw"
```

#### Why `--reflink=always` Matters

- **Zero initial disk overhead:** `cp --reflink=always` issues the Linux `FICLONE` ioctl so `/var/lib/hyperos/instances/<vm_id>/current/disk.raw` points to the exact same Btrfs data extents as `/var/lib/hyperos/base-images/<image_id>/disk.raw`. Cloning a 2 GiB base image completes in milliseconds and consumes only a few kilobytes of Btrfs metadata.
- **Copy-on-Write isolation:** The first time the guest writes to a 4 KiB block, Btrfs allocates a new extent exclusively for that instance's `current/disk.raw`, leaving `base-images/<image_id>/disk.raw` untouched and immutable.
- **No silent full-copy fallback:** `ARCHITECTURE.md` explicitly forbids full-copy fallbacks or cross-filesystem copies. Passing `--reflink=always` (never `--reflink=auto`) guarantees the clone fails immediately if CoW is unavailable or if the source and destination are on different filesystems.

### Cold Read-Only Snapshots (`snapshot_vm`)

While a VM is `stopped`, `snapshot_vm` creates a read-only Btrfs subvolume snapshot of `current/`:

```sh
SNAP_ID="4f281e57-3717-40fe-9d90-025f37962bbd"
btrfs subvolume snapshot -r \
    "/var/lib/hyperos/instances/${VM_ID}/current" \
    "/var/lib/hyperos/instances/${VM_ID}/snapshots/${SNAP_ID}"
```

### Verifying Shared Extents (Zero Initial Overhead)

After cloning one or more instances, verify that the exclusive bytes are `0.00B` and shared extents match the base image:

```sh
btrfs filesystem du -s \
    /var/lib/hyperos/base-images/alpine-3-21-virtio/disk.raw \
    /var/lib/hyperos/instances/*/current/disk.raw

# Inspect physical extent sharing directly:
filefrag -v /var/lib/hyperos/base-images/alpine-3-21-virtio/disk.raw | head -n 20
filefrag -v /var/lib/hyperos/instances/${VM_ID}/current/disk.raw | head -n 20
```

Expected `btrfs filesystem du` output immediately after cloning: `Exclusive` is `0.00B` for both the base image and the freshly cloned instance, while `Set shared` reflects the single physical copy of the compressed extents. Never delete a base image in `/var/lib/hyperos/base-images/<image_id>/` while any instance or snapshot still references it.
