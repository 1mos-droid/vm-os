# Review fixes: work plan for three agents

This plan turns the external review of HyperOS (November 2026) into work for three
coding agents: **Claude**, **Codex** and **Antigravity**. Each agent reads this
whole file, then does only its own part.

Every finding below was checked against the code on `chore/optimize-ci-build`
(commit `2b77843`). The docs (`README.md`, `ARCHITECTURE.md`,
`docs/CLOUD_TESTING.md`, `SECURITY.md`, `CONTRIBUTING.md`, `vmm-daemon/README.md`)
were corrected to describe current behavior before this plan was written. Where
`ARCHITECTURE.md` says "current behavior", that is the starting point you change.

---

## 1. Verified findings

| # | Finding | Evidence | Owner |
| --- | --- | --- | --- |
| F1 | The default image cannot start a VM: crosvm is not built, only a user-supplied prebuilt is installed, and the package is off. | `kernel-buildroot/package/crosvm/crosvm.mk`, `BR2_PACKAGE_CROSVM` absent from `configs/hyperos_x86_64_defconfig` | Codex |
| F2 | No base-image import tool; guest disks are copied onto partition 3 by hand. | `docs/CLOUD_TESTING.md` §2, `guest-images/README.md` | Codex |
| F3 | `desktop-shell` and `vm-control-ui` are not packaged into the image. | `kernel-buildroot/package/` only has `crosvm`, `runit`, `vmm-daemon` | Antigravity (make buildable), then Codex (package) |
| F4 | KSM is set to `run=1` globally. Nothing is merged today because no one marks guest memory `MADV_MERGEABLE`, but the setting is wrong for untrusted guests. | `rootfs-overlay/etc/sv/ksm/run` | Codex |
| F5 | NAT guests can reach the uplink LAN: the forward rule only excludes `10.200.0.0/15`. | `vmm-daemon/src/network.rs:195` | Claude |
| F6 | crosvm runs as root (the daemon spawns it directly; only crosvm's jail restricts it). | `vmm-daemon/src/hypervisor.rs:230` | Claude |
| F7 | Only UID 0 may use the control socket, so any GUI must run as root. | `vmm-daemon/src/server.rs:167` | Claude (daemon + contract), Antigravity (clients) |
| F8 | Snapshots can be taken but not listed, restored or deleted. | `ARCHITECTURE.md` method table | Claude, then Antigravity (UI) |
| F9 | Disposable VMs are half-built: `storage.rs` has an `ephemeral` flag that purges the disk on stop, but no RPC can set it. | `vmm-daemon/src/storage.rs:41`, `:322` | Claude, then Antigravity (UI) |
| F10 | No packet capture. | — | Claude, then Antigravity (UI) |
| F11 | No verified boot (dm-verity, signed Secure Boot, TPM) and no A/B update path. | `board/genimage.cfg`, `initramfs/init` | Codex |
| F12 | No theme or app catalog. | — | Antigravity (themes + manifest draft) |

The docs-disagreement finding from the review (ARCHITECTURE.md describing
BIOS-only, ext4 root, no initramfs) is **already fixed** in this commit.

---

## 2. Rules for all agents

1. **Stay in your files.** Each part below lists the paths you own. If you need a
   change elsewhere, write it under "Requests to other agents" in your PR
   description instead of making it.
2. **The contract changes first, and only through Claude.** Any new RPC method,
   parameter, error code, socket permission or storage path goes into
   `ARCHITECTURE.md` in Claude's contract PR (step A0) before anyone codes
   against it. Codex and Antigravity read the contract; they do not edit it.
3. **Don't weaken the existing invariants** in `CONTRIBUTING.md` §4: no shell
   interpolation, no `--disable-sandbox`, no software-emulation fallback, strict
   unknown-key rejection, `stop_vm` warning in every UI.
4. **Run the checks** from `CONTRIBUTING.md` §5 for every file you touched, and
   paste the results into your PR. CI does not boot the image, so say plainly
   which runtime behavior you could and could not verify.
5. **One PR per step**, branched from `main` after the docs commit merges, named
   `<agent>/<step>-<slug>` (for example `claude/a1-nat-lan-block`). Keep PRs
   small enough to review in one sitting.
6. **Update docs in the same PR.** When you change behavior, update the matching
   "current behavior" text in `ARCHITECTURE.md` (Claude) or the relevant README,
   and tick the step in §6 of this file.

---

## 3. Part A — Claude: daemon, network, authorization, contract

**Owns:** `ARCHITECTURE.md`, `vmm-daemon/`, `.github/workflows/vmm-daemon-ci.yml`.

### A0. Contract PR (do this first; it unblocks the others)

**Status:** approved 2026-11-22 ("Contract v2 additions" in `ARCHITECTURE.md`).

Propose all API changes for phases 2–3 in one `ARCHITECTURE.md` PR, and have the
maintainer approve it before implementing. It must define:

- **Snapshots:** `list_snapshots {vm_id}`, `restore_snapshot {vm_id, snapshot_id}`
  (VM must be stopped; replaces `current/` with a writable snapshot of the chosen
  read-only one, atomically), `delete_snapshot {vm_id, snapshot_id}`.
- **Disposable VMs:** a required `disposable: boolean` on `create_vm`, and the
  field on the VM object. A disposable VM's disk resets to its base on every stop
  or crash. Keep "no implicit defaults": clients must send the field.
- **Packet capture:** `start_capture {vm_id}` / `stop_capture {vm_id}` for a
  running VM with a NIC, writing pcap files to a daemon-owned path under
  `/var/lib/hyperos/instances/<vm_id>/captures/`. Clients get an ID, never a
  host path. Define a size cap and what happens when it is reached.
- **NAT semantics:** NAT reaches the internet only. Specify the blocked ranges.
- **Socket authorization** (see A3).
- **New error code**, if needed, for "permission denied for this method".

### A1. Block LAN access from NAT guests (F5)

In `build_nat_nft_ruleset`, drop forwarded traffic from the guest to private,
link-local, CGNAT, multicast and broadcast destinations (IPv4: `10.0.0.0/8`,
`172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`, `100.64.0.0/10`,
`224.0.0.0/4`, `255.255.255.255`). The table is `inet`, so also drop IPv6
`fc00::/7`, `fe80::/10` and `ff00::/8`, or drop guest IPv6 forwarding entirely.
Add an input-hook rule so the guest cannot reach host services on the TAP except
what the guest needs to get an address. Extend the existing ruleset unit test in
`network.rs` to assert every blocked range.

### A2. Run crosvm unprivileged (F6)

The daemon stays root and does the privileged setup. For each VM it then starts
crosvm as a dedicated unprivileged user: create the TAP owned by that user,
open the disk and hand over file descriptors, and give `/dev/kvm` access through
a group. Keep the jail and seccomp. Spectrum OS is doing the same thing with
crosvm and is worth reading first. Codex adds the user and group to the image
(request it in your PR). Add a test that the spawn path never runs crosvm as
UID 0.

### A3. GUI authorization (F7)

**Recommended design** (confirm with the maintainer in A0): a `hyperos` group;
socket `root:hyperos 0660`, `/run/hyperos` `0750`; `SO_PEERCRED` accepts UID 0
or any peer whose GID or supplementary groups include `hyperos`. All methods are
allowed to group members in v2 (the GUI user is the machine's owner); keep the
check in one function so per-method roles can be added later without a contract
break. Never forward the socket over TCP.

### A4. Snapshot restore, disposable VMs, packet capture (F8–F10)

**Done.** Breaking change for clients: `create_vm` now requires `"disposable"`
and the VM object has 9 keys. Antigravity (C3) must send it from both UIs;
Codex must add it to `scripts/test-v1-release.sh`, `hyperos-vm` and the
`docs/CLOUD_TESTING.md` examples, and set `/var/lib/hyperos` to
`root:hyperos 0710` in `/etc/sv/storage/run` (the daemon also sets it at start).

Implement the A0 methods with unit and integration tests in the style of
`tests/lifecycle_integration.rs`. Reuse the existing `ephemeral` storage path for
disposable VMs rather than adding a second mechanism. For capture, prefer a
small `AF_PACKET` pcap writer in the daemon (it already depends on `libc`) over
shipping `tcpdump` in the host image; if you choose `tcpdump`, request that
Codex package it.

### A5. KSM in the daemon

KSM stays off (Codex sets that in B2). Don't add `MADV_MERGEABLE`. If KSM ever
comes back it should be an opt-in per VM, which needs its own contract change.

---

## 4. Part B — Codex: OS image, boot, packaging, CI

**Owns:** `kernel-buildroot/`, `scripts/`, `guest-images/`,
`.github/workflows/build-os.yml`, `docs/CLOUD_TESTING.md`.

### B1. Build crosvm into the image (F1)

Replace the prebuilt-only package with one that builds a pinned crosvm release
(commit hash plus source checksum) for `x86_64-unknown-linux-musl` and installs
its x86_64 seccomp policies into `/usr/share/policy/crosvm`. Turn it on in the
defconfig. If a musl source build is not practical, fall back to a pinned,
hash-verified binary built by a CI job in this repo, and document why. Make
`scripts/validate-configs.sh --resolved-buildroot` fail when crosvm is missing.

### B2. KSM off by default (F4)

Change `rootfs-overlay/etc/sv/ksm/run` to write `0` and keep the readiness
marker so the daemon's startup wait still works. Keep `CONFIG_KSM` in the kernel.
Update `scripts/test-v1-release.sh` and `docs/CLOUD_TESTING.md` §4.4 to expect
`0`.

### B3. Import tool and a VM with one command (F2)

Add an import script (for example `scripts/import-guest-image.sh`) that copies a
`guest-images/` output into a `hyperos.img` data partition. It must verify the
`manifest.json` SHA-256 and validate the `image_id`, and it must clean up loop
devices on exit. Then make `scripts/test-v1-release.sh` do the whole flow from
build output to a running Alpine guest with one command. Note that the daemon
boots the host's `/boot/bzImage` with `root=/dev/vda1`, so check that the guest
images work with that kernel.

### B4. Package the UIs (F3; after C1)

Add Buildroot packages for `desktop-shell` (with `--features kms`) and
`vm-control-ui`, plus their runtime dependencies (seatd, libinput, DRM), and a
runit service that starts the shell as an unprivileged user in the `hyperos`
group once A3 lands. Add the `hyperos` group and the crosvm user for A2. Measure
idle RAM headless and with the shell, and publish both numbers in the README.

### B5. Verified boot and A/B updates (F11; design first)

Write a short design PR before coding: dm-verity over the EROFS root with the
root hash on the kernel command line, a signed UEFI path (shim or a signed UKI),
optional TPM measurement, and two root slots with a boot counter and fallback in
GRUB. The BIOS boot path cannot be verified the same way; say what it gets.
Implementation follows in separate PRs.

### B6 (optional). Boot test in CI

If the runners expose `/dev/kvm`, add a CI job that boots the image in QEMU and
checks the readiness markers, so boot regressions stop relying on manual tests.

---

## 5. Part C — Antigravity: desktop shell and VM Studio

**Owns:** `desktop-shell/`, `vm-control-ui/`.

### C1. Make both UIs packageable (unblocks B4)

Both crates must build for `x86_64-unknown-linux-musl`, and the shell's `kms`
backend must work on the target. Document every runtime library and device node
they need in each README so Codex can package them. Keep the `--mock` modes
working.

### C2. Run as a non-root user (after A0/A3)

Connect as a member of the `hyperos` group, and show a clear message when the
socket refuses the connection (wrong group, daemon not running), not a generic
error.

### C3. UI for the new research features (after A0; mock first)

Add these to VM Studio and the shell, built against `--mock` first, then the
real daemon once A4 merges:
- a snapshot list with restore and delete (restore warns that current disk
  changes are lost)
- a "Disposable" toggle on create, and a clear badge on disposable VMs
- packet capture start and stop, with status
- a network badge showing that NAT is internet-only

Extend the existing unit tests for any RPC client changes, as `CONTRIBUTING.md`
§5 requires.

### C4. Themes as data (F12)

Load a theme from a JSON token file (colors, fonts, corner radii, spacing, and a
layout preset name) at startup into Slint globals. Validate it strictly: reject
unknown keys and out-of-range values, and fall back to the built-in theme. A
theme must never be able to run code. Ship two themes. Signing and the catalog
come later.

### C5. App manifest draft (F12; proposal only)

Write `docs/proposals/app-manifest.md`: an app is a VM template (base image ID
plus RAM, network mode, shared folders, clipboard policy, disposable default),
installed only onto the data partition, with hashes in the existing
`manifest.json` style. This is a proposal for the maintainer, not a contract
change. Claude moves it into `ARCHITECTURE.md` if it is approved.

---

## 6. Order and dependencies

```text
Phase 1  B1 crosvm   B2 KSM off   B3 import + one-command VM   C1 buildable UIs   A0 contract PR
Phase 2  A1 NAT LAN block   A2 unprivileged crosvm   A3 socket auth   B4 package UIs (needs C1, A3)   C2 (needs A3)
Phase 3  A4 restore / disposable / capture   C3 UI for them (mock first, needs A0)
Phase 4  B5 verified boot + A/B
Phase 5  C4 themes   C5 app manifest proposal
```

Everyone can start right away: Claude on A0, Codex on B1–B3, Antigravity on C1.
Nothing in phase 2 or 3 that touches the API starts until A0 is approved.

### Progress

- [x] Docs match the code (this commit)
- [x] A0 (approved 2026-11-22) · [x] A1 · [ ] A2 · [x] A3 · [x] A4 · [x] A5
- [ ] B1 · [ ] B2 · [ ] B3 · [ ] B4 · [ ] B5 · [ ] B6
- [x] C1 · [x] C2 · [x] C3 · [ ] C4 · [ ] C5
