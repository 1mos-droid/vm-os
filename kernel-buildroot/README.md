# HyperOS image work

The default headless image builds crosvm and minijail from pinned upstream
commits with checked SHA-256 source archives, and installs the x86_64 seccomp
policies. It enables balloon, 2D GPU and TAP networking to match the daemon.
The daemon argument builder uses upstream `--pivot-root` and
`--seccomp-policy-dir`; balloon support is on by default in this crosvm build.
No sandbox-disabling fallback is installed.

Build with the existing Buildroot 2025.02.1 instructions. In addition to the
usual Buildroot prerequisites, install Cargo/Rust with the
`x86_64-unknown-linux-musl` standard library, Python 3, clang/libclang and
pkg-config on the build host. CI installs these and builds crosvm as part of
the image. The crosvm binary uses the target musl loader and libcap/Wayland
libraries, all installed by its Buildroot dependencies; the daemon stays static.
Full crosvm/image compilation and hardware boot remain required acceptance,
not implied by passing configuration checks.

KSM stays built into the kernel but its runit service writes `0`, preserving
the daemon readiness marker. The image supplies `hyperos` GID 970, `kvm` GID 78,
and a locked `desktop` account UID 1000 in `hyperos` and `video`. Per-VM UIDs
61000–61254 are reserved for the daemon; they are not persistent login users.
`/dev/kvm` is `root:kvm 0660` at boot and under udev.

## Import and boot

Only import into an offline image; never into one attached to a running VM.

```sh
sudo guest-images/build-alpine-guest.sh --out-dir out
sudo scripts/import-guest-image.py out/alpine-3-21-virtio output/images/hyperos.img
# Inside a booted HyperOS image, as root or a member of hyperos:
hyperos-vm alpine-3-21-virtio
```

Or perform the full build/import/scratch-host/guest boot flow on a nested-KVM
machine in one command:

```sh
sudo scripts/test-v1-release.sh --image output/images/hyperos.img
```

The runner needs QEMU, accessible `/dev/kvm`, mtools, Python 3, util-linux,
Btrfs mounting support and the Alpine builder's prerequisites. It preserves the
input image, retains serial logs and removes the large scratch disk. Use
`--guest-dir out/alpine-3-21-virtio` to reuse a guest, `--uefi --ovmf PATH` for
OVMF, or `--dry-run` without root/KVM. It checks headless host RAM before VM
creation, KSM off, daemon start success and a guest userspace vsock marker.
Existing images built before the marker was added must be rebuilt for this test.
The vsock listener exists only when the scratch GRUB command line explicitly
requests `hyperos.boot_image=<image_id>`; ordinary host boot starts no VM.

The runner does not test guest persistence across host reboot. It does not
measure graphical idle RAM. No measured RAM numbers are published until real
hardware acceptance produces them.

## UI packaging (B4)

`BR2_PACKAGE_DESKTOP_SHELL` builds the shell with
`--no-default-features --features kms`: Antigravity's C1 DRM/KMS backend with a
software framebuffer, libinput and seatd, and no winit, Mesa, EGL or GBM.
`BR2_PACKAGE_VM_CONTROL_UI` builds VM Studio with
`--no-default-features --features wayland`. Both use locked crate dependencies,
the musl target, and select fontconfig, FreeType and DejaVu fonts for Slint's
software text rendering. They stay off in the default defconfig because the
graphical session has not yet been booted on real hardware.

To try a graphical build, enable eudev device management and both packages.
When those binaries are installed, runit starts udev and seatd and launches the
shell under UID 1000, with `hyperos` and `video` groups, a private XDG runtime
directory and the seatd backend; the shell picks its KMS backend on its own.
VM Studio is installed for launching from the session. No root GUI or password
login is enabled. Membership in `hyperos` lets the session use the daemon's
control socket (A3).

## Verified boot and updates (B5)

[VERIFIED_BOOT.md](VERIFIED_BOOT.md) is the design required before implementation
by `docs/REVIEW_FIXES.md` B5. The current partition layout and boot behavior are
unchanged. Authenticated UEFI boot, dm-verity enforcement, two root slots,
activation counters and rollback are not implemented by this change.

## Local checks

```sh
scripts/validate-configs.sh
python3 scripts/test-image-tools.py
scripts/test-v1-release.sh --dry-run
```

CI also validates resolved Buildroot/kernel configs, disk partition structure,
installed crosvm/policies, target loader/CLI availability and the packaged daemon
RPC. A real KVM boot and graphical session remain separate acceptance gates.
