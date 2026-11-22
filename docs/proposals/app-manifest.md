# Declarative VM App Manifest (`app-manifest.json`) — Draft Proposal

**Status:** Proposal / Draft (F12). **This document is NOT an `ARCHITECTURE.md` contract change.** It captures the design for declarative VM application templates so the maintainer can review security boundaries and IPC extensions before approving promotion into `ARCHITECTURE.md`.

---

## 1. Design Goals

In HyperOS, an "app" is a declarative launch profile over an immutable base guest image — **not** a host script, container tarball on the root filesystem, or arbitrary binary installer:

1. **Zero rootfs mutation:** App manifests and static icons live **only on the writable data partition** under `/var/lib/hyperos/apps/<app_id>/`. The host EROFS root partition stays strictly read-only and minimal.
2. **Data, not code:** `app-manifest.json` contains strictly validated JSON configuration tokens and content hashes. It has **no** lifecycle shell hooks, custom kernel command lines, environment injections, or executable payloads.
3. **`manifest.json`-aligned integrity:** Base image references and optional static assets use the same lowercase 64-character SHA-256 hex digest style established in [`/var/lib/hyperos/base-images/<image_id>/manifest.json`](../../ARCHITECTURE.md).
4. **Immediate UI usability + clean daemon path:** Existing Contract v2 RPCs (`create_vm`, `start_vm`, `set_balloon_ram`) can instantiate basic manifests client-side today, while security-critical fields (`base_image_rootfs_sha256` verification, `shared_folders` policy mapping, `clipboard_policy`) motivate clean future daemon RPC extensions.

---

## 2. Data-Partition Filesystem Layout

Apps MUST NOT be stored inside the host root filesystem (`/usr`, `/etc`, `/opt`). They live strictly on the writable `/var/lib/hyperos` Btrfs data partition:

```text
/var/lib/hyperos/
├── base-images/              # root:root 0700 (immutable Btrfs subvolumes + manifest.json)
├── instances/                # root:root 0700 (per-VM current/, snapshots/, metadata.json)
├── captures/                 # root:hyperos 0750 (packet capture pcaps)
└── apps/                     # root:hyperos 0750 (declarative app bundles)
    ├── disposable-browser/   # root:hyperos 0750
    │   ├── app-manifest.json # root:hyperos 0640
    │   └── icon.svg          # root:hyperos 0640 (optional static asset)
    └── offline-vault/        # root:hyperos 0750
        └── app-manifest.json # root:hyperos 0640
```

Permission & confinement rules:
- Directory names `<app_id>` MUST match the manifest's `app_id` and match `^[a-z0-9][a-z0-9_-]{0,47}$`.
- Symlinks escaping `/var/lib/hyperos/apps/<app_id>/` (or pointing into `/var/lib/hyperos/base-images/` or `instances/`) are rejected.
- Unprivileged UIs running in group `hyperos` MAY read `app-manifest.json` (up to `64 KiB`) and static icons, or query a future `vmm-daemon` `list_apps` RPC, without ever gaining read/write access to VM disks (`instances/`) or root image blocks (`base-images/`).

---

## 3. Schema Specification (`schema_version: 1`)

All parsers MUST enforce `#[serde(deny_unknown_fields)]`, UTF-8 validity, exact JSON integer types (rejecting floats/strings), and a `64 KiB` maximum file size (`65_536` bytes).

| Field | Type | Validation / Constraint | Purpose |
| :--- | :--- | :--- | :--- |
| `schema_version` | `u32` integer | Exact `1` | Forward schema versioning |
| `app_id` | `string` | `^[a-z0-9][a-z0-9_-]{0,47}$`; matches `<app_id>` dir | Unique app identifier & default VM name stem |
| `display_name` | `string` | `1..=64` printable UTF-8 chars, trimmed, no control chars | Human-readable label shown in Action Deck / VM Studio |
| `version` | `string` | `^[0-9A-Za-z][0-9A-Za-z._+-]{0,31}$` | Informative release tag (no executable meaning) |
| `base_image_id` | `string` | `^[a-z0-9][a-z0-9_-]{0,47}$` | Guest template under `/var/lib/hyperos/base-images/` |
| `base_image_rootfs_sha256` | `string` | Exact 64 lowercase hex chars (`^[0-9a-f]{64}$`) | Pinned SHA-256 matching `manifest.json`'s `rootfs.sha256` |
| `resources` | `object` | `deny_unknown_fields` | Guest CPU and RAM ceilings (`vcpu_count`, `ram_mib`, `idle_balloon_mib`) |
| `resources.vcpu_count` | `u32` integer | `1..=16` | Passed to `create_vm.vcpu_count` |
| `resources.ram_mib` | `u32` integer | `128..=8192` | Passed to `create_vm.ram_mib` |
| `resources.idle_balloon_mib` | `u32` integer | `64..=resources.ram_mib` | Initial `virtio-balloon` floor used by `Reclaim RAM` / idle shrink |
| `network_mode` | `string` enum | `"isolated"` \| `"nat"` \| `"host-only"` | Sandbox policy (`nat` blocks RFC1918/LAN/host/inter-VM; internet only) |
| `disposable` | `bool` | Required boolean | Contract v2 `disposable` default (reset writable disk on stop/crash; snapshots disabled) |
| `display_mode` | `string` enum | `"seamless"` \| `"full"` | Window presentation mode in `desktop-shell` |
| `shared_folders` | `string[]` | `0..=8` items, each `^[a-z0-9][a-z0-9_-]{0,31}$` | Logical policy tags **only** (never raw host paths) |
| `clipboard_policy` | `string` enum | `"disabled"` \| `"paste-only"` \| `"bidirectional"` | Cross-VM / host clipboard policy enforced by compositor / broker |
| `icon` | `object` *(optional)* | `deny_unknown_fields`: `file`, `sha256`, `bytes` | Static `.png`/`.svg` asset confined inside `<app_id>` (`bytes <= 262_144`, `sha256` 64 lowercase hex) |

---

## 4. Security Invariants

1. **No raw host paths in `shared_folders`:** Allowing strings like `"/home/user/.ssh"` or `"/etc"` in an app manifest would let malicious manifests exfiltrate host secrets. `shared_folders` permits only short logical tags (e.g., `"downloads"`, `"scratch-ro"`) that map through a fixed host policy table in trusted system components.
2. **SHA-256 pinning against `manifest.json`:** `base_image_rootfs_sha256` MUST be verified against `/var/lib/hyperos/base-images/<base_image_id>/manifest.json` before spawning or attaching a cloned rootfs. Mismatch fails closed instead of running an unintended or stale base image.
3. **Disposable invariant inheritance:** When `disposable: true`, the launched VM cannot create snapshots (`snapshot_vm` returns `-32002`) and its CoW writable disk is wiped/re-cloned on stop or crash per Contract v2 (A4).
4. **Origin color isolation:** App manifests intentionally have **no** window border color field; `desktop-shell` and `vm-control-ui` always assign the 2px security origin color from trusted `network_mode` + `disposable` state.
5. **No code execution / no shell interpolation:** Neither UIs nor `vmm-daemon` pass manifest strings through `sh -c`, URL handlers, or custom hypervisor flags.
6. **Future catalog signing (deferred F12 Phase 2):** When online catalog distribution lands later, signature metadata will wrap or accompany `app-manifest.json` externally (detached Ed25519 manifest envelope) rather than relaxing strict `deny_unknown_fields` on core runtime manifests today.

---

## 5. Mapping to Contract v2 RPCs vs. Future Daemon Additions

### Immediate Contract v2 Flow (Client-Side Orchestration)

With the approved Contract v2 (`list_vms`, `create_vm`, `start_vm`, `set_balloon_ram`, etc.), a non-root UI can realize the core lifecycle cleanly:

1. Validate `/var/lib/hyperos/apps/<app_id>/app-manifest.json` strictly (`deny_unknown_fields`, bounds, 64 KiB cap).
2. Send `create_vm` with `name`, `base_image_id`, `resources.vcpu_count`, `resources.ram_mib`, `network_mode`, and `disposable`.
3. Send `start_vm` (`vm_id`), track `display_mode` and `resources.idle_balloon_mib` in client policy, and apply compositor clipboard policy (`clipboard_policy`) at the Wayland boundary.

### Proposed Future `vmm-daemon` Extensions (Requires Maintainer Approval)

For strong kernel/hypervisor enforcement independent of client UI state:
- `launch_app` *(or extended `create_vm` verification)*: accepts `app_id`, verifies `base_image_rootfs_sha256` against root-owned `base-images/<base_image_id>/manifest.json` (which non-root UIs cannot read due to `0700` permissions), and resolves `shared_folders` logical tags to sandboxed `virtio-fs` mounts.
- Clipboard enforcement hook between the compositor (`desktop-shell`) and guest agent adhering to `clipboard_policy`.

---

## 6. Complete Examples

### Example 1: Disposable Internet Browser (`disposable-browser/app-manifest.json`)

```json
{
  "schema_version": 1,
  "app_id": "disposable-browser",
  "display_name": "Disposable Web Browser",
  "version": "1.0.0",
  "base_image_id": "alpine-minimal",
  "base_image_rootfs_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "resources": {
    "vcpu_count": 2,
    "ram_mib": 1024,
    "idle_balloon_mib": 384
  },
  "network_mode": "nat",
  "disposable": true,
  "display_mode": "seamless",
  "shared_folders": [
    "downloads"
  ],
  "clipboard_policy": "paste-only",
  "icon": {
    "file": "icon.svg",
    "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
    "bytes": 4096
  }
}
```

### Example 2: Persistent Air-Gapped Vault (`offline-vault/app-manifest.json`)

```json
{
  "schema_version": 1,
  "app_id": "offline-vault",
  "display_name": "Offline Document Vault",
  "version": "1.0.0",
  "base_image_id": "alpine-minimal",
  "base_image_rootfs_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "resources": {
    "vcpu_count": 1,
    "ram_mib": 512,
    "idle_balloon_mib": 256
  },
  "network_mode": "isolated",
  "disposable": false,
  "display_mode": "full",
  "shared_folders": [],
  "clipboard_policy": "disabled"
}
```

---

## 7. Open Questions for Maintainer Review

1. **Digest check ownership:** Should `vmm-daemon` expose a read-only `list_base_images` RPC so non-root UIs can verify `base_image_rootfs_sha256` before calling `create_vm`, or should `vmm-daemon` accept an `expected_rootfs_sha256` field on `create_vm` / `launch_app` and verify against `root:root 0700` base images itself?
2. **`shared_folders` backing store:** Which fixed host subvolume layout (e.g., `/var/lib/hyperos/shared/<tag>/`) and read-only default flags (`:ro` vs `:rw`) should map logical tags to `crosvm --shared-dir`?
3. **Clipboard gate:** Should `clipboard_policy` live purely in the Wayland compositor (`desktop-shell`) data-device filter, or also require a guest agent capability negotiation?
4. **Catalog signing format:** For future online app catalogs, should signatures live in a sibling `app-manifest.sig` Ed25519 envelope verified on import, keeping `app-manifest.json` pure data?
