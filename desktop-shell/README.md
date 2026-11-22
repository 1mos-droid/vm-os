# HyperOS Workspace-First Desktop Shell (`desktop-shell/`)

An original, ultra-low-RAM Wayland desktop shell built in Rust with **Smithay 0.7** and **Slint** (pure-Rust CPU software renderer), targeting **< 100 MB total RAM usage** on 4 GiB HyperOS machines.

## Design Highlights (Workspace-First, Not a macOS/Windows Clone)

- **Top Context Rail (replaces docks & menu bars):**
  - Pill-shaped Space tabs for active VMs showing color-coded origin dots and live RAM usage.
  - `+ New Space` quick-launch trigger.
  - Compact Host + ZRAM memory meter, Wi-Fi, Audio, and Battery status indicators.
  - One-click **`Reclaim RAM`** button that deflates idle VM `virtio-balloon` targets via `set_balloon_ram` over `/run/hyperos/vmm.sock`.
- **Origin-Aware Smithay Wayland Compositor (`src/compositor.rs`, `src/origin.rs`, `src/layout.rs`):**
  - Every window displays a **2px colored border** and a subtle **top-left origin badge pill** indicating whether it belongs to the **Host** or a specific VM Space (e.g., Cyan for *Disposable Browser*, Emerald for *Work Pod*, Amber for *Offline Vault*).
  - **Magnetic half & quarter screen snapping** (28px edge/corner attraction zones + one-click/shortcut snap targets) with **smooth 180 ms ease-out cubic window transitions**.
- **`Super`-Key Action Deck (`src/action_deck.rs`, `ui/shell.slint`):**
  - Instant search across apps, active Spaces, and presets.
  - One-click instant environment launchers (**Disposable Browser**, **Work Pod**, **Offline Vault**).
  - Quick hardware toggles (Wi-Fi, Audio Mute, Display Scaling, Global Offline Shield).
- **Bare-Metal DRM/KMS + libinput Backend (`src/kms.rs`, `src/framebuffer.rs`):**
  - Double-buffered `XRGB8888` DRM dumb buffers (`set_crtc` + `page_flip` with `simpledrm` fallback), software arrow cursor overlay, `libseat` VT switching (`Ctrl+Alt+F1..F12`), and `Super` key handling without Mesa/GBM/EGL/X11.

## Build, Test, and Run

```sh
cd desktop-shell

# Run compositor, framebuffer, snapping, transition, origin-border, VMM bridge, and headless Slint UI tests
cargo test --all-targets

# Launch the desktop shell with mock VMs and fallback telemetry (no vmm-daemon required)
cargo run -- --mock

# Run the Smithay Wayland compositor headlessly (useful for CI smoke tests)
cargo run -- --mock --headless

# Bare-metal DRM/KMS build without winit/X11/Mesa (requires libseat, libinput, udev, libdrm headers)
cargo build --release --no-default-features --features kms

# Compile-check DRM/KMS + libinput + udev without seatd/libseat installed
cargo check --no-default-features --features kms-direct --all-targets
```

## Packaging for HyperOS (`x86_64-unknown-linux-musl`)

Buildroot packaging (`kernel-buildroot/package/desktop-shell/`, owned by Codex B4) should compile the bare-metal `kms` target **dynamically linked against musl** (`-crt-static` — required because `fontconfig` / `dlopen` helpers load shared objects on musl):

```sh
PKG_CONFIG_ALLOW_CROSS=1 \
PKG_CONFIG_SYSROOT_DIR="$(STAGING_DIR)" \
PKG_CONFIG_PATH="$(STAGING_DIR)/usr/lib/pkgconfig:$(STAGING_DIR)/usr/share/pkgconfig" \
RUST_FONTCONFIG_DLOPEN=1 \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$(TARGET_CC)" \
RUSTFLAGS="-C target-feature=-crt-static" \
cargo build \
    --release \
    --locked \
    --no-default-features \
    --features kms \
    --target x86_64-unknown-linux-musl
```

### Runtime Dependencies Required in `rootfs.erofs`

| Library / Asset | Buildroot Package | Purpose |
| --- | --- | --- |
| `libseat.so.1` + `seatd` daemon | `BR2_PACKAGE_SEATD` | Unprivileged DRM master & `/dev/input/*` access + VT switching |
| `libinput.so.10` | `BR2_PACKAGE_LIBINPUT` | Keyboard, mouse, and I2C HID touchpad events |
| `libudev.so.1` (`eudev`) | `BR2_PACKAGE_EUDEV` | `/dev/dri/card*` and `/dev/input/event*` enumeration |
| `libdrm.so` (headers at build) | `BR2_PACKAGE_LIBDRM` | Direct kernel modesetting (`drm` crate uses ioctls; headers needed if `bindgen` runs) |
| `libfontconfig.so.1` + `libfreetype.so.6` + `libexpat.so.1` | `BR2_PACKAGE_FONTCONFIG`, `BR2_PACKAGE_FREETYPE`, `BR2_PACKAGE_EXPAT` | Slint software text rendering |
| At least one TTF font (e.g. DejaVu Sans) | `BR2_PACKAGE_DEJAVU` | Default UI font discovered via Fontconfig |

**Explicitly NOT required:** Mesa, GBM, EGL, OpenGL, Pixman, X11/Xorg/XCB, or D-Bus (`zbus`).

### Device Nodes, Sockets, and Environment Variables

| Path / Variable | Access Required | Notes |
| --- | --- | --- |
| `/dev/dri/card*` | Via `seatd` (`libseat`) or `video` group | Primary DRM/KMS scanout (`i915`, `amdgpu`, `virtio_gpu`, `simpledrm`) |
| `/dev/input/event*` | Via `seatd` (`libseat`) or `input` group | Keyboard, pointer, and touchpad input |
| `/run/seatd.sock` | Read/write by shell user (`seat` group) | `seatd` must be started and ready before `desktop-shell` |
| `/run/hyperos/vmm.sock` | `root:hyperos 0660` (dir `0750`) | `vmm-daemon` JSON-RPC 2.0 socket (user in `hyperos` group) |
| `XDG_RUNTIME_DIR` | Writable directory (`0700`, e.g. `/run/user/1000`) | Required for Wayland listener socket (`wayland-hyperos-*`) |
| `HYPEROS_DRM_OUTPUT` | Optional env var (e.g. `eDP-1`, `Virtual-1`) | Override automatic first-connected DRM output selection |
| `XDG_SEAT` | Optional env var (default `seat0`) | Seat name passed to `libseat` and `libinput` |

### Running as a Non-Root User (`hyperos` group)

Per [`ARCHITECTURE.md`](../ARCHITECTURE.md) Contract v2 (A3):
- `vmm-daemon` runs as `root` with `/run/hyperos` set to `root:hyperos 0750` and `/run/hyperos/vmm.sock` set to `root:hyperos 0660`, verifying `SO_PEERCRED` against UID `0` or primary/supplementary membership in the `hyperos` group and closing unauthorized sockets immediately without a response.
- Both `desktop-shell` and `vm-control-ui` run as an unprivileged GUI user in the `hyperos` group (plus `seat` for `seatd`).
- `/var/lib/hyperos` is `root:hyperos 0710` (traverse-only) and `/var/lib/hyperos/captures/` is `root:hyperos 0750`, while `base-images/` and `instances/` remain `root:root 0700`.
- When connection fails or is rejected, the Top Context Rail displays a visible red `VMM Offline` badge and an actionable banner distinguishing filesystem permission denial (`hyperos` group missing), immediate `SO_PEERCRED` peer close, `vmm-daemon` not running, and RPC timeouts (without blindly retrying mutations).
