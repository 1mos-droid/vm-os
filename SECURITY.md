# Security Policy

HyperOS (`1mos-droid/vm-os`) is a security-focused virtualization host OS designed to isolate workloads inside hardware-virtualized `crosvm` microVMs. We take security vulnerabilities seriously and appreciate responsible disclosure from the community.

---

## Supported Versions

Security fixes are applied to the `main` branch and included in subsequent OS image and crate releases:

| Version / Branch | Supported |
| --- | --- |
| `main` (Buildroot `2025.02.1` / Linux `6.12.20` baseline) | Yes |
| Older commits / unmaintained forks | No |

---

## Reporting a Vulnerability

**Please do NOT open a public GitHub issue, pull request, or discussion with a working exploit or sensitive vulnerability details.**

Instead, report security vulnerabilities privately using **GitHub Security Advisories**:

- Submit a private report: https://github.com/1mos-droid/vm-os/security/advisories/new

### What to Include

To help us reproduce, triage, and fix the issue quickly, please include:

1. **Affected subsystem** (`kernel-buildroot/`, `vmm-daemon/`, `vm-control-ui/`, `desktop-shell/`, `guest-images/`, or CI/build pipeline).
2. **Impact & threat boundary crossed** (for example: guest-to-host escape, `SO_PEERCRED` UID 0 bypass, Btrfs path traversal, or network sandbox escape).
3. **Minimal reproduction steps** or proof-of-concept (including host kernel version, boot mode, and relevant JSON-RPC payloads or logs).
4. **Suggested mitigation or patch**, if you have one.

### Response Timeline

| Milestone | Target SLA |
| --- | --- |
| Initial acknowledgment | Within **48 hours** |
| Triage & severity confirmation | Within **5 business days** |
| Remediation plan & status updates | Every **7 days** until resolved |
| Public disclosure | Coordinated after a fix is merged and released |

---

## Security Scope

### In Scope

Vulnerabilities that violate HyperOS's documented security invariants in [`ARCHITECTURE.md`](ARCHITECTURE.md):

- **Hypervisor & VM Isolation:** `crosvm` seccomp/jail bypasses, KVM/virtio host escapes (`virtio-blk`, `virtio-net`, `virtio-vsock`, `virtio-fs`, `virtio-gpu`), or accidental fallback to software emulation.
- **Control Plane & IPC (`/run/hyperos/vmm.sock`):** Bypassing `root:root` `0600` / `SO_PEERCRED` UID 0 authorization, JSON-RPC 2.0 framing/parser memory safety issues, oversized-frame DoS, or shell/argument injection from RPC parameters.
- **Btrfs CoW Storage & Ephemeral Lifecycle (`/var/lib/hyperos`):** Path or symlink traversal outside base-image/instance directories, `manifest.json` SHA-256 verification bypasses, cross-VM disk sharing, or failure to purge ephemeral CoW overlays and runtime artifacts on VM shutdown/crash.
- **Network Sandboxing:** Packet leakage out of `isolated` VMs, unauthorized outbound internet access from `host-only` VMs, or `nftables`/bridge isolation bypasses.
- **Desktop Shell & UI Trust Indicators (`desktop-shell/`, `vm-control-ui/`):** Spoofing VM origin borders/badges across surfaces, unauthorized cross-VM input/clipboard access, or unconfirmed destructive `stop_vm` actions.
- **Boot & Build Supply Chain:** Integrity flaws in hybrid UEFI/BIOS boot staging, initramfs early-boot mounting, or pinned Buildroot/kernel digest verification.

### Out of Scope

- Issues that only occur in developer-only `--mock` or `--dry-run` simulation modes without affecting production paths.
- Local denial-of-service attacks that already require `root` (`UID 0`) privileges on the host.
- Upstream vulnerabilities in third-party guest OS packages (Alpine, Debian, etc.) unrelated to HyperOS configuration or `guest-images/` build scripts.
- Theoretical attacks without a demonstrated security boundary violation.
- Known, documented gaps tracked in [`docs/REVIEW_FIXES.md`](docs/REVIEW_FIXES.md): `crosvm` running as root, KSM enabled globally, and the absence of verified boot. Reports that bypass these *further* are still in scope.

---

## Coordinated Disclosure & Safe Harbor

We support good-faith security research conducted in accordance with this policy:

- Test against your own local HyperOS builds or VMs—never target infrastructure or data you do not own.
- Give maintainers reasonable time to develop, verify, and ship a fix before publishing technical details.
- With your permission, we will credit you in the security advisory and release notes once the fix is published.
