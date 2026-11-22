# HyperOS architecture and integration contract

This file is the single source of truth for OS, daemon and UI integration. The
image targets x86_64 machines with 4 GiB RAM and VT-x/AMD-V enabled, booting via
UEFI or legacy BIOS/CSM from one hybrid GPT disk image. The host idle target is
**less than 350 MiB used RAM**, measured before any VM starts; it is a performance
acceptance target, not a measurement established by configuration, and no real
measurement has been published yet. No desktop, network listener, login shell, or
guest is started by the base image. Wi-Fi and audio are disabled in the v1 kernel.
`desktop-shell/` and `vm-control-ui/` exist as source crates (with `--mock` modes)
but are **not yet packaged into the image**; no display server or GUI runs on the
booted host today.

Open review findings and the work plan that addresses them live in
[`docs/REVIEW_FIXES.md`](docs/REVIEW_FIXES.md). Where a section below says
"current behavior", it describes the code as it is, not the intended end state.

## Repository ownership

```text
ARCHITECTURE.md                     # shared contract, change with consumers
kernel-buildroot/                  # Buildroot BR2_EXTERNAL=... tree
  external.desc, external.mk, Config.in
  configs/hyperos_x86_64_defconfig
  configs/busybox.fragment
  kernel/hyperos.config             # minimal seed expanded by olddefconfig
  package/runit/                    # upstream runit, missing from Buildroot
  package/vmm-daemon/               # builds vmm-daemon for x86_64-unknown-linux-musl
  package/crosvm/                   # installs a user-supplied prebuilt crosvm (opt-in)
  initramfs/                        # early-boot init: EROFS + overlay root, data mount/grow
  board/                           # GRUB (UEFI + BIOS), genimage partition layout
  rootfs-overlay/                   # runit stages and service definitions
vmm-daemon/                        # Rust daemon supervising crosvm children
desktop-shell/                     # Smithay + Slint Wayland shell (source only, not packaged)
vm-control-ui/                     # Slint VM Studio JSON-RPC client (source only, not packaged)
guest-images/                     # Alpine / Debian guest disk builders, no disks in git
docs/                             # testing guides and the review work plan
scripts/validate-configs.sh         # local/CI acceptance checks
scripts/test-v1-release.sh          # QEMU boot + lifecycle release runner
.github/workflows/build-os.yml      # kernel + rootfs + hybrid disk image artifact
.github/workflows/vmm-daemon-ci.yml # daemon tests (glibc + musl)
```

`/usr/bin/vmm-daemon` is built from `vmm-daemon/` by Buildroot and always
packaged (`BR2_PACKAGE_VMM_DAEMON=y`). `/usr/bin/crosvm` is **not** built: the
`crosvm` package only installs a prebuilt binary the builder supplies via
`BR2_PACKAGE_CROSVM_PREBUILT_BIN`, and it is off in the defconfig, so the default
image cannot start a VM. The guest kernel is the host's own `/boot/bzImage`
(direct kernel boot, `root=/dev/vda1`). The daemon accepts no arguments, reads no
UI-provided host paths, listens on the socket below, uses `/dev/kvm` without
software emulation fallback, supervises crosvm children, and handles SIGTERM by
stopping children and persisting metadata before exit. The UI crates own
display/session integration; packaging them is open work.

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
* Current behavior: crosvm children are spawned by the root daemon and keep
  root as their main-process identity; only crosvm's own jail (pivot-root,
  seccomp when policies are installed) restricts them. There is no unprivileged
  crosvm user yet.
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
cold disk snapshot, not a RAM/device checkpoint. No restore, list or delete
snapshot RPC is defined in v1, so snapshots cannot yet be used from the API. The
storage layer has an internal `ephemeral` flag that purges a VM's disk on stop,
but no RPC can set it; disposable VMs are not exposed. `set_network_mode` requires stopped; changes are persisted atomically and
applied at next start. Balloon target resets to maximum at each cold start.
Success responses are sent only after the operation and metadata commit; on
failure keep the prior valid state or expose failed, never partial success.

Network semantics owned by daemon:

* `isolated`: no virtual NIC, no host/guest or inter-guest network path.
* `nat`: per-VM TAP/private subnet, host forwarding + masquerade for outbound
  traffic through an existing uplink; no unsolicited inbound or inter-VM traffic.
  The guest reaches the public IPv4 internet only; see "NAT semantics" in the
  v2 section below for the blocked ranges and the input-hook filter.
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

## Contract v2 additions (proposed, pending maintainer approval)

This section is the A0 step of [`docs/REVIEW_FIXES.md`](docs/REVIEW_FIXES.md).
Everything above describes v1 as implemented. Once approved, implementers code
against this section; when a step lands, its text is folded into the v1 sections
above and removed here. All v1 rules (framing, unknown-key rejection, integer
types, no implicit defaults, error format, atomic commit) apply unchanged.

### Socket authorization (A3)

* Group `hyperos` (fixed GID, supplied by the image). Socket `root:hyperos` mode
  `0660`; `/run/hyperos` `root:hyperos` mode `0750`. Daemon umask stays 077 and
  sets these modes explicitly after bind.
* `SO_PEERCRED`: accept UID 0, or a peer whose primary or supplementary groups
  (read from `/proc/<pid>/status` `Groups:` for the peer PID, re-checked per
  connection) include the `hyperos` GID. Reject everything else by closing the
  connection without a response.
* Members of `hyperos` may call every method. The check lives in one function so
  per-method roles can be added later. No new error code is introduced.
* Membership in `hyperos` is equivalent to full control of every VM. The GUI
  session user is the only intended member. Never forward the socket over TCP.

### Unprivileged crosvm (A2)

* The daemon stays root and performs all privileged setup (TAP creation, disk
  and control-socket preparation, nftables, cgroups if any).
* Each running VM's crosvm runs under its own UID/GID from the range
  `61000`–`61254` (UID = GID = 61000 + slot), allocated at start and released on
  reap. Never 0. Supplementary groups are exactly `kvm`. `/dev/kvm` is
  `root:kvm 0660` (supplied by the image).
* The per-VM disk, TAP and control socket are owned by that UID for the life of
  the process and returned to `root` on stop. crosvm's jail and seccomp remain
  enabled. A daemon restart reaps orphans before reallocating UIDs.

### NAT semantics (A1)

`nat` gives a guest the public internet only. The forward chain drops guest
traffic to every non-global IPv4 destination: `0.0.0.0/8`, `10.0.0.0/8`,
`100.64.0.0/10`, `127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`,
`192.0.0.0/24`, `192.168.0.0/16`, `198.18.0.0/15`, `224.0.0.0/4` and
`240.0.0.0/4` (which includes broadcast). All guest IPv6 is dropped. An input
chain drops every packet from the guest TAP addressed to the host except
`ct state established,related`. The host provides no DHCP or DNS: guests use
static addressing (`10.200.<slot>.2/30`, gateway `.1`) and a public resolver.
Known limit: an uplink LAN that uses public (non-RFC1918) addresses is not
blocked by these ranges.

### Disposable VMs (A4)

* `create_vm` params gain a required `"disposable": boolean`. The VM object
  gains a required `"disposable": boolean`, fixed at creation.
* A disposable VM's writable disk is discarded on every stop or crash and
  re-cloned from its base at the next start. Its metadata persists (v1 has no
  VM deletion).
* `snapshot_vm` and `restore_snapshot` on a disposable VM return -32002.
* Metadata written by v1 (no `disposable` key) loads as `false`. On the wire
  the key is always present and always required in `create_vm`.

### Snapshot methods (A4)

| Method | params object | result |
| --- | --- | --- |
| `list_snapshots` | `{"vm_id": uuid}` | `{"snapshots": [Snapshot, ...]}` sorted by `created_at`, then `snapshot_id` |
| `restore_snapshot` | `{"vm_id": uuid, "snapshot_id": uuid}` | VM object |
| `delete_snapshot` | `{"vm_id": uuid, "snapshot_id": uuid}` | `{}` |

`restore_snapshot` requires the VM to be stopped. It stages a writable Btrfs
snapshot of `snapshots/<snapshot_id>/`, swaps it in for `current/` atomically,
then deletes the old `current/`; on any failure `current/` is unchanged. Other
snapshots are unaffected. `delete_snapshot` works in any VM state. Both return
-32001 for an unknown snapshot or VM.

### Packet capture methods (A4)

| Method | params object | result |
| --- | --- | --- |
| `start_capture` | `{"vm_id": uuid}` | Capture object |
| `stop_capture` | `{"vm_id": uuid}` | Capture object |
| `list_captures` | `{"vm_id": uuid}` | `{"captures": [Capture, ...]}` sorted by `started_at`, then `capture_id` |
| `delete_capture` | `{"vm_id": uuid, "capture_id": uuid}` | `{}` |

Capture object (all keys required):

```json
{"capture_id":"0c2f6e9a-7b1d-4c3e-9f80-2d5a6b7c8e91","vm_id":"b6f51eed-8dc7-47f6-a8e7-81b6ae783a21","started_at":"2026-11-22T00:00:00Z","state":"running","bytes":0}
```

`state` is exactly `running`, `stopped` or `limit_reached`. `bytes` is the
current pcap file size.

* `start_capture` requires a running VM with a NIC (`nat` or `host-only`):
  -32002 if not running, -32004 for `isolated`, -32002 if a capture is already
  running for that VM.
* The daemon records every frame on the VM's TAP (both directions) into a
  classic pcap file (Ethernet link type) using its own `AF_PACKET` socket. No
  external capture binary is used.
* A capture stops when `stop_capture` is called (idempotent if none is running:
  it returns the latest capture or -32001 if there is none), when the VM stops
  or crashes, or when the file reaches 256 MiB (`limit_reached`).
* Files live at `/var/lib/hyperos/captures/<vm_id>/<capture_id>.pcap`, owned by
  `root:hyperos`, mode `0640`; the directories are `root:hyperos 0750`, and
  `/var/lib/hyperos` itself becomes `root:hyperos 0710` (traverse-only) so the
  GUI user can open known capture paths and nothing else. The path is derived
  from IDs, never taken from a request. Captures contain raw guest traffic,
  including anything malware sends; treat them as untrusted input.
* Capture metadata persists under `captures/<vm_id>/<capture_id>.json`. Only
  `delete_capture` removes a capture. At daemon start, any capture left in
  `running` is marked `stopped`.

### Storage layout additions

```text
/var/lib/hyperos/                  # root:hyperos 0710 (was root:root 0700)
  captures/<vm_id>/                # root:hyperos 0750
    <capture_id>.pcap              # root:hyperos 0640
    <capture_id>.json              # Capture object, atomically replaced
```

`base-images/` and `instances/` stay `root:root 0700`.

### Image requirements for Codex (B2/B4)

The image must provide the `hyperos` group, the `kvm` group with `/dev/kvm`
`root:kvm 0660`, an unprivileged GUI user in `hyperos`, and the
`/var/lib/hyperos` and `captures/` modes above (set by `/etc/sv/storage/run`).
KSM is `run=0`.

## Build, boot and acceptance

Pinned baseline: Buildroot 2025.02.1, Linux 6.12.20, runit 2.1.2. CI verifies the
Buildroot archive digest and upstream package hashes. These pins are a reproducible
bootstrap, not a current security maintenance claim; update and audit before
production use.

```sh
scripts/validate-configs.sh
# Download/extract the pinned Buildroot archive (see workflow), then:
export HOSTCC="$PWD/scripts/host-gcc-gnu17"
export BR2_CMAKE_CANDIDATES=""
make -C buildroot O="$PWD/output" BR2_EXTERNAL="$PWD/kernel-buildroot" hyperos_x86_64_defconfig
scripts/validate-configs.sh --resolved-buildroot output/.config
make -C buildroot O="$PWD/output" -j2
scripts/validate-configs.sh --resolved-kernel output/build/linux-6.12.20/.config
scripts/validate-configs.sh --image output/images/hyperos.img
qemu-system-x86_64 -enable-kvm -cpu host -m 4096 -nographic -drive file=output/images/hyperos.img,format=raw,if=virtio
```

`hyperos.img` is a hybrid GPT disk (protective MBR + BIOS boot code) bootable by
UEFI and legacy BIOS:

| # | Size | Content | Identifier |
| --- | --- | --- | --- |
| 1 | 100 MiB | FAT32 ESP: GRUB (`EFI/BOOT/BOOTX64.EFI` + BIOS core), `boot/bzImage`, `boot/initramfs.cpio.zst` | label `HYPEROS_EFI` |
| 2 | 512 MiB | read-only EROFS root (LZ4HC, dedupe) | PARTUUID `48595045-0000-4000-8000-000000000002` |
| 3 | 512 MiB | Btrfs data, mounted at `/var/lib/hyperos` | label `HYPEROS_DATA` |

GRUB offers two entries. Both boot the kernel with an initramfs that mounts the
EROFS root under a 128 MiB tmpfs OverlayFS upper layer (writes are lost at
reboot). **Persistent Mode** mounts `HYPEROS_DATA`, and on first boot grows
partition 3 and the Btrfs filesystem into trailing unallocated space.
**Stateless RAM-Only Mode** never mounts the disk's data partition; it formats a
separate zram device as Btrfs for `/var/lib/hyperos`. The artifact also contains
`bzImage`, `rootfs.erofs`, `initramfs.cpio.zst`, `BOOTX64.EFI` and the packaged
`vmm-daemon`. Writing the image to a disk destroys that disk's prior contents;
there is no in-place updater, A/B slot or installer. Nothing verifies the root's
integrity at boot: no dm-verity, no signed Secure Boot chain, no TPM measurement.
The kernel is monolithic (`CONFIG_MODULES` unset). Partition UUIDs are fixed;
attach only one copy of the image per machine.

runit PID 1 mounts proc/sysfs/tmpfs, then supervises zram, KSM, storage and daemon.
`/run` holds supervision and readiness state so root remains read-only. ZRAM is
65% of MemTotal, zstd, priority 100; it consumes physical memory on demand and
is not extra RAM. Current behavior: `/etc/sv/ksm/run` sets
`/sys/kernel/mm/ksm/run=1` globally, but neither the daemon nor crosvm marks
guest memory `MADV_MERGEABLE`, so KSM merges nothing today. Cross-VM page sharing
has side-channel implications and is unsuitable for untrusted guests; the plan is
to default it off. Stop/reboot via `runit-init 0` / `runit-init 6`; shutdown stops
daemon before unmounting storage. No guest images are bundled.

CI keeps ubuntu-latest and explicitly builds host C tools in GNU17 mode; the
pinned host m4/gnulib predates newer compilers defaulting to C23. This does not
change the target musl toolchain. Empty `BR2_CMAKE_CANDIDATES` makes Buildroot
build its pinned CMake 3.31.5 rather than adopting an incompatible host CMake 4.

CI uses the Bootlin external musl toolchain with ccache. It validates shell
syntax, source config invariants, resolved Buildroot options, resolved Linux
options and the image layout, smoke-tests the packaged musl `vmm-daemon` with
`list_vms`, and uploads the image. CI never boots the image or starts a VM. On a
KVM-capable target, verify boot, `/dev/kvm`, Btrfs mount, zram compressor/size,
the KSM setting, and shutdown.
After 60 seconds idle with no VM, record `/proc/meminfo`: used = MemTotal -
MemAvailable must be <358400 KiB. Repeat with integrated daemon/UI, then verify
VM lifecycle, balloon response, isolation and snapshot consistency. CI compilation
does not establish runtime correctness, idle RAM or near-native performance.
