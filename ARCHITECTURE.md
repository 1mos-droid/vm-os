# HyperOS architecture and integration contract

This file is the single source of truth for OS, daemon and UI integration. The
bootstrap targets x86_64 machines with 4 GiB RAM, VT-x/AMD-V enabled, and BIOS/CSM
boot. The host idle target is **less than 350 MiB used RAM**, measured before any
VM starts; it is a performance acceptance target, not a measurement established
by configuration. No desktop, network listener, login shell, or guest is started
by the base image. USB, Wi-Fi, audio and UEFI-only machines are outside this first
hardware profile. The virtio GPU driver supports a future UI; no display server
or GUI is supplied here.

## Repository ownership

```text
ARCHITECTURE.md                     # shared contract, change with consumers
kernel-buildroot/                  # Buildroot BR2_EXTERNAL=... tree
  external.desc, external.mk, Config.in
  configs/hyperos_x86_64_defconfig
  configs/busybox.fragment
  kernel/hyperos.config             # minimal seed expanded by olddefconfig
  package/runit/                    # upstream runit, missing from Buildroot
  board/                           # GRUB, partition image generation
  rootfs-overlay/                   # runit stages and service definitions
vmm-daemon/                        # Rust daemon + crosvm packaging, separate work
vm-control-ui/                     # GUI JSON-RPC client, separate work
guest-images/                     # image import manifests, no guest disks in git
scripts/validate-configs.sh         # local/CI acceptance checks
.github/workflows/build-os.yml      # kernel + rootfs + BIOS disk image artifact
```

The daemon team supplies musl-compatible `/usr/bin/vmm-daemon` and
`/usr/bin/crosvm` (and any runtime assets) through Buildroot packaging. The daemon
accepts no arguments, reads no UI-provided host paths, listens on the socket below,
uses `/dev/kvm` without software emulation fallback, supervises crosvm children,
and handles SIGTERM by stopping children and persisting metadata before exit.
The UI team owns display/session integration. The bootstrap deliberately ships
neither application nor crosvm; VM launch acceptance requires their integration.

## Local RPC transport and authorization

* Address: **`/run/hyperos/vmm.sock`**, AF_UNIX/SOCK_STREAM.
* UTF-8 JSON-RPC **2.0**, one complete JSON object followed by LF per frame.
  Maximum frame: 65536 bytes including LF. Close oversized connections without
  parsing further. No batches, notifications, positional parameters or events.
* Request schema: exactly `{"jsonrpc":"2.0","id":ID,"method":METHOD,"params":OBJECT}`.
  ID is a string of 1–64 ASCII letters, digits, `_` or `-`. Every request needs a
  unique ID among outstanding requests on that connection. Responses may arrive
  out of order; match by ID. The UI may reconnect and call `list_vms` after a
  timeout; do not blindly retry mutating operations.
* Success: exactly `{"jsonrpc":"2.0","id":ID,"result":RESULT}`. Error: exactly
  `{"jsonrpc":"2.0","id":ID_OR_NULL,"error":{"code":INTEGER,"message":STRING}}`.
  Use null only when a valid request ID cannot be recovered. Never return both
  result and error. All objects below reject unknown keys; numeric fields must
  be JSON integers, never floats, booleans or numeric strings.
* Daemon runs as root for KVM, mounts and network setup, with umask 077. Socket
  and parent directory are root:root, modes 0600 and 0700. Check SO_PEERCRED and
  accept UID 0 only. The first UI integration is a trusted local root client;
  do not expose the socket to unprivileged users or forward it over TCP. A later
  unprivileged GUI needs an explicit authorization design and contract change.
* Only unlink an existing socket after proving no daemon owns it; serialize
  daemon startup. Serialize mutations per VM and storage allocation globally.
  Never interpolate request values into a shell. Canonicalize and validate image
  IDs against the image registry, reject symlink traversal and paths outside the
  managed storage tree. Launch crosvm with its jail/seccomp enabled, private
  per-VM directories and only required devices/file descriptors.

### Exact method schemas

Notation: `uuid` is a lowercase canonical UUID string
`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`; `image_id` is 1–64 ASCII letters, digits,
`_` or `-`; `network_mode` is exactly `isolated`, `nat` or `host-only`.
Every parameter shown is required. No implicit defaults.

| Method | params object | result |
| --- | --- | --- |
| `create_vm` | `{"name": string, "base_image_id": image_id, "vcpu_count": integer, "ram_mib": integer, "network_mode": network_mode}` | VM object |
| `start_vm` | `{"vm_id": uuid}` | VM object |
| `stop_vm` | `{"vm_id": uuid}` | VM object |
| `pause_vm` | `{"vm_id": uuid}` | VM object |
| `snapshot_vm` | `{"vm_id": uuid}` | Snapshot object |
| `set_balloon_ram` | `{"vm_id": uuid, "ram_mib": integer}` | VM object |
| `set_network_mode` | `{"vm_id": uuid, "network_mode": network_mode}` | VM object |
| `list_vms` | `{}` | `{"vms": [VM, ...]}` sorted by vm_id |

`name` is 1–64 Unicode code points without control characters. `vcpu_count` is
1–64, also no greater than online host CPUs. `create_vm.ram_mib` is 128–3072
and sets the fixed crosvm maximum guest memory. Reject start/create if the total
maximum RAM of running/paused guests plus the new VM exceeds host MemTotal minus
768 MiB; swap is not admission capacity. Use MiB = 1048576 bytes throughout.
`set_balloon_ram.ram_mib` is the desired usable guest RAM, 128 through that VM's
creation maximum. Translate to balloon bytes = `(maximum - desired) * 1048576`.
This is a target, not an immediate measurement or host allocation guarantee.
Ballooning requires a running VM with a working virtio balloon device; do not
assume it succeeds merely because crosvm accepted a control command.

VM object (all keys required):

```json
{"vm_id":"b6f51eed-8dc7-47f6-a8e7-81b6ae783a21","name":"demo","base_image_id":"linux-demo","vcpu_count":1,"ram_mib":1024,"balloon_target_mib":1024,"network_mode":"isolated","state":"stopped"}
```

`state` is exactly `stopped`, `running`, `paused` or `failed`. Fields retain their
creation values except balloon target, network mode and state. `create_vm`
allocates a server UUID and a reflink disk, persists metadata, returns stopped,
and never boots. `start_vm` starts stopped/failed VMs or resumes paused VMs;
starting running is idempotent. `pause_vm` suspends running VMs; paused is
idempotent. `stop_vm` terminates and reaps crosvm, flushes disk/metadata, and returns
stopped; stopped is idempotent. It is a **hard power-off**: UI must warn about
possible guest data loss. Other invalid states return -32002. A crosvm crash
produces failed. At daemon restart, reconcile/reap existing children before
accepting mutations; do not report a persisted running state without a live VM.

`snapshot_vm` is allowed only while stopped. It creates a read-only Btrfs disk
snapshot plus immutable metadata and returns exactly:

```json
{"snapshot_id":"4f281e57-3717-40fe-9d90-025f37962bbd","vm_id":"b6f51eed-8dc7-47f6-a8e7-81b6ae783a21","created_at":"2026-11-20T00:00:00Z"}
```

IDs are generated by the daemon; timestamp is UTC RFC3339 seconds. This is a
cold disk snapshot, not a RAM/device checkpoint. No restore/delete RPC is defined
in v1. `set_network_mode` requires stopped; changes are persisted atomically and
applied at next start. Balloon target resets to maximum at each cold start.
Success responses are sent only after the operation and metadata commit; on
failure keep the prior valid state or expose failed, never partial success.

Network semantics owned by daemon:

* `isolated`: no virtual NIC, no host/guest or inter-guest network path.
* `nat`: per-VM TAP/private subnet, host forwarding + masquerade for outbound
  traffic through an existing uplink; no unsolicited inbound or inter-VM traffic.
* `host-only`: per-VM TAP/private subnet for host–guest communication; no uplink
  forwarding, NAT, or inter-VM communication.

Daemon allocates non-overlapping subnets, guest addressing and firewall rules,
rolls back partial setup and cleans up on stop. Host has no configured uplink in
this bootstrap; return -32004 if required networking cannot be supplied. The
kernel supports TAP, bridge, namespaces and nftables; this does not implement a
network manager.

Error codes: -32700 parse error; -32600 invalid envelope (including batches or
notifications); -32601 unknown method; -32602 invalid/unknown/missing params;
-32603 internal error; -32001 VM/image not found; -32002 invalid VM state;
-32003 insufficient RAM/storage/CPU resources; -32004 unavailable capability
(KVM, balloon, crosvm, network); -32005 storage/process I/O failure. Messages are
human-readable, omit secrets/host paths, and are not parsed by clients. Invalid
requests must not mutate VM, disk or network state.

Example exchange (each line is one frame):

```json
{"jsonrpc":"2.0","id":"create-1","method":"create_vm","params":{"name":"demo","base_image_id":"linux-demo","vcpu_count":1,"ram_mib":1024,"network_mode":"isolated"}}
{"jsonrpc":"2.0","id":"create-1","result":{"vm_id":"b6f51eed-8dc7-47f6-a8e7-81b6ae783a21","name":"demo","base_image_id":"linux-demo","vcpu_count":1,"ram_mib":1024,"balloon_target_mib":1024,"network_mode":"isolated","state":"stopped"}}
{"jsonrpc":"2.0","id":"list-1","method":"list_vms","params":{}}
{"jsonrpc":"2.0","id":"list-1","result":{"vms":[]}}
```

The list example is an independent empty-host exchange.

## Persistent CoW storage

```text
/var/lib/hyperos/                  # separate Btrfs filesystem, root:root 0700
  base-images/<image_id>/          # imported, validated, immutable base
    disk.raw                      # sparse raw bootable guest disk
    manifest.json                 # image_id, sha256 (64 lowercase hex), format="raw"
  instances/<vm_id>/
    metadata.json                 # VM object, atomically replaced + fsync
    current/                      # Btrfs subvolume
      disk.raw                    # reflink of base disk; never write the base
    snapshots/<snapshot_id>/      # read-only snapshot of current subvolume
    snapshot-metadata/<snapshot_id>.json # immutable VM metadata + Snapshot object
```

All images are bootable raw disks; guest firmware/kernel requirements belong to
image import/crosvm packaging. Import happens outside this RPC API. Validate
manifest checksum before admitting a base. Keep base and instances on the same
Btrfs filesystem; use FICLONE/reflink and Btrfs read-only subvolume snapshots,
never a full-copy fallback or shared writable base. Reject ENOSPC and unsupported
CoW. Stage create/snapshot operations and commit atomically; recover interrupted
staging on startup. Never delete a base referenced by an instance/snapshot. No
service formats disks on boot. A failed data mount blocks daemon startup.

## Build, boot and acceptance

Pinned baseline: Buildroot 2025.02.1, Linux 6.12.20, runit 2.1.2. CI verifies the
Buildroot archive digest and upstream package hashes. These pins are a reproducible
bootstrap, not a current security maintenance claim; update and audit before
production use.

```sh
scripts/validate-configs.sh
# Download/extract the pinned Buildroot archive (see workflow), then:
export HOSTCC="gcc -std=gnu17"
export BR2_CMAKE_CANDIDATES=""
make -C buildroot O="$PWD/output" BR2_EXTERNAL="$PWD/kernel-buildroot" hyperos_x86_64_defconfig
scripts/validate-configs.sh --resolved-buildroot output/.config
make -C buildroot O="$PWD/output" -j2
scripts/validate-configs.sh --resolved-kernel output/build/linux-6.12.20/.config
qemu-system-x86_64 -enable-kvm -cpu host -m 4096 -nographic -drive file=output/images/hyperos.img,format=raw,if=virtio
```

`hyperos.img` is a complete BIOS disk: GRUB + 128 MiB ext4 read-only root + 512 MiB
Btrfs starter data partition. This deliberately small data partition must be
grown offline before importing real guests. Artifact also contains bzImage and
rootfs.ext2. Writing the full image to a disk destroys that disk's prior contents;
there is no in-place updater or installer. Boot requires working serial console
(ttyS0), virtio-blk, AHCI or NVMe; no initramfs is needed because drivers are built
in. Root uses a fixed MBR PARTUUID; attach only one copy of the image per machine.

runit PID 1 mounts proc/sysfs/tmpfs, then supervises zram, KSM, storage and daemon.
`/run` holds supervision and readiness state so root remains read-only. ZRAM is
65% of MemTotal, zstd, priority 100; it consumes physical memory on demand and
is not extra RAM. KSM is enabled globally; daemon must mark guest mappings
mergeable. Cross-VM page sharing has side-channel implications: this host profile
is for mutually trusted guests. Stop/reboot via `runit-init 0` / `runit-init 6`;
shutdown stops daemon before unmounting storage. No guest images are bundled.

CI keeps ubuntu-latest and explicitly builds host C tools in GNU17 mode; the
pinned host m4/gnulib predates newer compilers defaulting to C23. This does not
change the target musl toolchain. Empty `BR2_CMAKE_CANDIDATES` makes Buildroot
build its pinned CMake 3.31.5 rather than adopting an incompatible host CMake 4.

CI validates shell syntax, source config invariants, resolved Buildroot options,
and resolved Linux options, builds and uploads the image. On a KVM-capable target,
verify boot, `/dev/kvm`, Btrfs mount, zram compressor/size, KSM run=1, and shutdown.
After 60 seconds idle with no VM, record `/proc/meminfo`: used = MemTotal -
MemAvailable must be <358400 KiB. Repeat with integrated daemon/UI, then verify
VM lifecycle, balloon response, isolation and snapshot consistency. CI compilation
does not establish runtime correctness, idle RAM or near-native performance.
