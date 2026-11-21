## Summary

<!-- Concisely explain what changed and why. -->

Closes #<!-- issue number, if applicable -->

---

## Subsystem Scope

Check the subsystem(s) modified by this PR (keep PRs scoped to a single subsystem unless landing an approved cross-component contract change):

- [ ] `kernel-buildroot/` (Buildroot, Linux kernel, initramfs, GRUB/hybrid image, runit overlay)
- [ ] `vmm-daemon/` (JSON-RPC server, `crosvm` supervision, Btrfs CoW, networking)
- [ ] `desktop-shell/` (Smithay Wayland compositor, Top Context Rail, Action Deck)
- [ ] `vm-control-ui/` (Zero-Script VM Studio / GUI client)
- [ ] `guest-images/` (CoW base image builders & manifests)
- [ ] `docs/`
- [ ] `scripts/` or `.github/workflows/`
- [ ] `ARCHITECTURE.md` (contract update)

---

## Type of Change

- [ ] Bug fix (non-breaking change fixing an issue)
- [ ] New feature (non-breaking change adding functionality)
- [ ] Security fix (if sensitive, ensure coordinated disclosure per `SECURITY.md`)
- [ ] Contract / breaking change (updates `ARCHITECTURE.md`)
- [ ] Documentation or repository governance
- [ ] Maintenance / refactor / dependency pin update

---

## `ARCHITECTURE.md` & Security Invariants Checklist

- [ ] Matches [`ARCHITECTURE.md`](ARCHITECTURE.md) verbatim (socket path `/run/hyperos/vmm.sock`, JSON-RPC 2.0 schemas/error codes, `/var/lib/hyperos` CoW storage layout) — **or** updates `ARCHITECTURE.md` in coordination with all affected components.
- [ ] Preserves strict IPC security (no shell interpolation of RPC input, `SO_PEERCRED` UID 0 enforcement, strict JSON-RPC framing & unknown-key rejection).
- [ ] Preserves hypervisor & storage safety (`crosvm` KVM + seccomp, strict Btrfs `FICLONE`/reflink cloning, ephemeral cleanup, hard power-off warnings in UI).
- [ ] Respects the `< 350 MiB` host idle RAM target on 4 GiB machines.
- [ ] No binary disk images (`*.raw`, `*.img`, `*.qcow2`, `*.iso`), build artifacts (`/output/`, `/buildroot/`, `/dl/`, `target/`), or secrets are committed.

---

## Verification

List the exact commands you ran and their results:

- [ ] `scripts/validate-configs.sh`
- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` *(if Rust crates modified)*
- [ ] `sh -n` / `shellcheck` and `--dry-run` *(if shell scripts or `guest-images/` modified)*
- [ ] `--resolved-buildroot` / `--resolved-kernel` / `--image` checks *(if `kernel-buildroot/` modified)*

```sh
# Paste verification commands and summary output here
```

---

## Additional Notes for Reviewers

<!-- Optional: call out design trade-offs, follow-ups, or manual test steps. -->
