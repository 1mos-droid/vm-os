# Contributing to HyperOS (`1mos-droid/vm-os`)

Thank you for your interest in contributing to HyperOS! Because HyperOS integrates a minimal Linux host OS, a privileged VMM daemon, a desktop control UI, and guest image tooling around a strict security and low-RAM budget, please follow the rules below for every contribution.

---

## 1. Contract-First Rule (`ARCHITECTURE.md`)

[`ARCHITECTURE.md`](ARCHITECTURE.md) is the **single source of truth** for the entire repository.

- Every Unix socket path (`/run/hyperos/vmm.sock`), JSON-RPC 2.0 request/response schema, error code, and persistent Btrfs storage path (`/var/lib/hyperos/...`) **must match `ARCHITECTURE.md` verbatim**.
- **Never silently deviate from the contract.** If a feature requires adding or changing an RPC method, parameter, error code, socket permission, or storage layout, propose and review the change to `ARCHITECTURE.md` first and coordinate updates across all affected components (`kernel-buildroot/`, `vmm-daemon/`, `vm-control-ui/`, `desktop-shell/`, `guest-images/`).

---

## 2. Subsystem Boundaries & Scoped Pull Requests

Keep pull requests focused on a single subsystem unless you are landing an approved cross-component contract change:

| Directory | Scope |
| --- | --- |
| `kernel-buildroot/` | Buildroot `BR2_EXTERNAL` configs, kernel config, `runit` packaging, board/GRUB image generation, rootfs overlay |
| `vmm-daemon/` | Rust VM lifecycle daemon, `crosvm` supervision, Btrfs storage & network sandboxing |
| `vm-control-ui/` | Lightweight native desktop GUI client over `/run/hyperos/vmm.sock` (plus `--mock` mode) |
| `desktop-shell/` | Workspace-First Wayland compositor (Smithay + Slint) talking to `/run/hyperos/vmm.sock` (plus `--mock` mode) |
| `guest-images/` | Reproducible guest base-image build scripts and import manifests |
| `docs/` | Operational, cloud-testing, and user documentation |
| `scripts/` & `.github/workflows/` | Config validators, compiler wrappers, and CI pipelines |

### What Must Never Be Committed

- Binary guest disk images (`*.raw`, `*.img`, `*.qcow2`, `*.iso`)
- Buildroot sources, download caches, or build artifacts (`/buildroot/`, `/output/`, `/dl/`, `target/`)
- Secrets, private keys, tokens, or machine-specific host paths

---

## 3. Branching, Commits, and Pull Request Workflow

1. **Never push directly to `main`.** Always branch off an up-to-date `main` using a clear prefix:
   - `feat/<short-topic>` for new features
   - `fix/<short-topic>` for bug fixes
   - `docs/<short-topic>` for documentation
   - `chore/<short-topic>` for maintenance or dependency pins
2. **Keep commits clean and atomic.** Commit messages should summarize *what* changed and *why*.
3. **Open a Pull Request** that includes:
   - A concise summary of the change and which subsystem(s) it touches
   - Exact verification commands executed and their results
   - Explicit confirmation of whether `ARCHITECTURE.md` was modified or matched verbatim

---

## 4. Non-Negotiable Technical & Security Invariants

All contributions are reviewed against the following invariants from `ARCHITECTURE.md`:

### JSON-RPC 2.0 & IPC Security
- **Strict framing:** Single UTF-8 JSON object followed by LF (`\n`), max `65536` bytes including LF. Close oversized connections without parsing further.
- **Strict schemas:** Reject unknown keys, batches, notifications, positional parameters, and non-integer numeric values (no floats, booleans, or numeric strings for integer fields). Never return both `result` and `error`.
- **No shell interpolation:** Never interpolate RPC request values into a shell command.
- **Socket authorization:** Keep `/run/hyperos` (`0700`) and `/run/hyperos/vmm.sock` (`0600`) owned by `root:root` with `SO_PEERCRED` UID 0 verification. Never expose the socket to unprivileged users or forward it over TCP without an approved authorization design in `ARCHITECTURE.md`.
- **No blind retries on mutations:** Clients may reconnect and call `list_vms` after a timeout, but must not blindly retry mutating RPCs (`create_vm`, `start_vm`, `stop_vm`, `pause_vm`, `snapshot_vm`, `set_balloon_ram`, `set_network_mode`).

### Hypervisor, Memory, and UX Safety
- **Hardware virtualization only:** Use `/dev/kvm` with `crosvm` jail/seccomp enabled; never fall back to software emulation.
- **4 GiB RAM discipline:** Respect the `< 350 MiB` host idle RAM acceptance target. Avoid unnecessary background services, heavy runtimes, or bloated GUI dependencies.
- **Admission control:** Enforce `MemTotal - 768 MiB` against active running/paused guest maximums; never count ZRAM swap as admission capacity.
- **Hard power-off warning:** `stop_vm` performs a hard power-off; any UI triggering `stop_vm` must explicitly warn the user about possible guest data loss.

### Persistent CoW Storage Safety
- Keep base images (`/var/lib/hyperos/base-images/<image_id>/`) and instances (`/var/lib/hyperos/instances/<vm_id>/`) on the same Btrfs filesystem.
- Always validate `manifest.json` (`image_id`, 64-char lowercase hex `sha256`, `format="raw"`) before admitting a base image, canonicalize paths, and reject symlink traversal.
- Always clone base disks with strict `FICLONE` / `cp --reflink=always` and snapshot with read-only Btrfs subvolumes—**never** fall back to full copies, share a writable base disk, delete a referenced base image, or auto-format disks on boot.

### Reproducible OS Builds
- Keep Buildroot, Linux kernel, and `runit` versions and archive SHA-256 digests pinned in CI and `kernel-buildroot/`. Any version bump must update the verified digests and pass full CI validation.

---

## 5. Required Verification Before Submitting a PR

Run the checks applicable to the files you modified:

### All Pull Requests
```sh
scripts/validate-configs.sh
```

### `kernel-buildroot/` and Shell Scripts
- Every `.sh` script and `runit` stage/service file (`run`, `1`, `2`, `3`) must be executable (`chmod +x`) and pass syntax checks:
  ```sh
  sh -n path/to/script
  ```
- For Buildroot or Linux kernel config changes, verify the resolved configs match expectations:
  ```sh
  scripts/validate-configs.sh --resolved-buildroot output/.config
  scripts/validate-configs.sh --resolved-kernel output/build/linux-6.12.20/.config
  ```

### Rust Crates (`vmm-daemon/`, `vm-control-ui/`, `desktop-shell/`)
```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```
- Any RPC client or daemon change must include unit tests verifying JSON-RPC 2.0 serialization, deserialization, unknown-field rejection, and error handling for affected methods.

### Guest Image Builders (`guest-images/`)
```sh
bash -n guest-images/<script>.sh
shellcheck guest-images/<script>.sh
./guest-images/<script>.sh --dry-run
```
- Scripts must clean up mounts and loop devices on exit (`trap`), use deterministic identifiers where possible, and output `disk.raw` + `manifest.json` conforming to `ARCHITECTURE.md`.

---

## 6. Code of Conduct & Security Reporting

- **Be constructive and respectful** in code reviews, issues, and discussions. Focus critique on technical trade-offs, security invariants, and maintainability.
- **Security vulnerabilities:** If you discover a sandbox escape, KVM/seccomp bypass, path traversal, or socket authorization flaw, **do not** open a public issue with a working exploit. Report it privately to the repository maintainers via GitHub Security Advisories (or direct maintainer contact) with reproduction steps and impact details.
