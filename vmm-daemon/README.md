# HyperOS VMM Daemon (`vmm-daemon/`)

Privileged Rust control-plane daemon supervising `crosvm` child processes for HyperOS over the exact JSON-RPC 2.0 Unix socket contract defined in [`ARCHITECTURE.md`](../ARCHITECTURE.md).

## Key Capabilities

- **JSON-RPC 2.0 Unix Socket Server (`src/server.rs`, `src/rpc.rs`):**
  - Binds `/run/hyperos/vmm.sock` (`AF_UNIX`/`SOCK_STREAM`, mode `0600`, parent `/run/hyperos` `0700`, `umask 077`, `SO_PEERCRED` UID 0 only in production).
  - Serializes daemon startup via `flock(LOCK_EX | LOCK_NB)` and only unlinks stale sockets after proving no live daemon owns them.
  - Serves concurrent clients asynchronously via Tokio with strict 65,536-byte LF-delimited frames, unique outstanding request ID tracking, and verbatim `ARCHITECTURE.md` error codes (`-32700` to `-32005`).
- **Zero-Overhead Btrfs CoW Storage & Persistence (`src/storage.rs`):**
  - Validates `manifest.json` SHA-256 checksums and rejects symlink traversal outside `/var/lib/hyperos/base-images/`.
  - Clones base disks with `cp --reflink=always` into per-VM Btrfs subvolumes (`instances/<vm_id>/current/disk.raw`), rejecting non-reflink filesystems in production without full-copy fallback.
  - Persists `metadata.json` atomically (`write` -> `fsync` -> `rename` -> `fsync(dir)`), creates cold read-only Btrfs subvolume snapshots (`snapshots/<snapshot_id>/` + `snapshot-metadata/<snapshot_id>.json`), and recovers interrupted staging on startup. The storage layer can purge a VM's CoW overlay on stop for an internal `ephemeral` flag, but no RPC sets it yet, so disposable VMs are not reachable through the API.
- **`crosvm` Supervision, Ballooning & Network Sandboxing (`src/hypervisor.rs`, `src/network.rs`, `src/manager.rs`):**
  - Spawns `crosvm run` with `virtio-blk`, `virtio-balloon`, `virtio-vsock`, `virtio-gpu`, jail/seccomp, and optional `virtio-net` TAP.
  - Wires `pause_vm` (`crosvm suspend`), resume (`crosvm resume`), `set_balloon_ram` (`crosvm balloon <bytes>`), `stop_vm` (`crosvm stop` + signal/reap), and runtime snapshot helper (`crosvm snapshot take`).
  - Enforces `MemTotal - 768 MiB` RAM admission control, per-VM + global locks, startup state reconciliation, `isolated`/`nat`/`host-only` network sandboxing via `ip` and `nft` (zero shell interpolation), and graceful `SIGTERM` child shutdown.

## Build, Test, and Dry-Run

```sh
cd vmm-daemon

# Run all unit and end-to-end AF_UNIX integration tests (no /dev/kvm or crosvm required)
cargo test --all-targets

# Launch the daemon locally in --dry-run mode with a mock hypervisor and network controller
cargo run -- --dry-run --socket /tmp/hyperos-test/vmm.sock --storage-dir /tmp/hyperos-test/storage
```

In production on HyperOS, `runit` launches `/usr/bin/vmm-daemon` with zero arguments (see `kernel-buildroot/rootfs-overlay/etc/sv/vmm-daemon/run`).
