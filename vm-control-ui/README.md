# HyperOS Zero-Script VM Studio (`vm-control-ui/`)

A lightweight, jargon-free native Rust + Slint desktop manager built for non-technical HyperOS users (pure-Rust `winit` + CPU software renderer for ultra-low RAM usage).

Communicates with `vmm-daemon` over `/run/hyperos/vmm.sock` using the exact JSON-RPC 2.0 contract defined in [`ARCHITECTURE.md`](../ARCHITECTURE.md).

## Four Zero-Script Views

1. **Spaces (Dashboard + Visual Checkpoints):**
   - Live Host Memory (`/proc/meminfo`) and ZRAM (`/sys/block/zram0/mm_stat`) gauges + one-click **`Reclaim RAM`** button (deflates idle VM balloons via `set_balloon_ram`).
   - Color-coded Space cards with plain-English Network & Display badges, one-click Open/Pause/Close (with mandatory hard power-off data-loss confirmation modal), and **Visual Checkpoints** (`Save Checkpoint` wired to `snapshot_vm` + `Rewind` checkpoint cards).
2. **Create Space (Plain-English Flow):**
   - Visual selectors for **Network Access** (*Offline Vault* = `isolated`, *Safe Web* = `nat`, *Home Network* = `host-only`), **Memory Profile** (*Light* / *Standard* / *Heavy* mapped to `virtio-balloon`), **Display Mode** (*Seamless App Window* vs *Full Environment*), and **Self-Destruct on Close** toggle — wired strictly to `ARCHITECTURE.md`'s `create_vm` RPC.
3. **Template Store:**
   - Curated guest template cards with one-click download & SHA-256 verification progress bars and instant handoff to Create Space.
4. **Guided Settings:**
   - Visual panels for **Wi-Fi & Airplane Isolation**, **Sound & PipeWire audio forwarding**, **Display Scaling** (`100% / 125% / 150%`), and live **Btrfs CoW Storage Savings** (`cp --reflink=always`).

## Build, Test, and Run

```sh
cd vm-control-ui

# Run all JSON-RPC 2.0, framing, mock VMM, Studio, and headless Slint UI tests
cargo test --all-targets

# Launch standalone in --mock mode (no vmm-daemon required)
cargo run -- --mock

# Launch against a live vmm-daemon Unix socket
cargo run -- --socket /run/hyperos/vmm.sock

# Build Wayland-only client (omits X11 dependencies for HyperOS target images)
cargo build --release --no-default-features --features wayland
```

## Packaging for HyperOS (`x86_64-unknown-linux-musl`)

Buildroot packaging (`kernel-buildroot/package/vm-control-ui/`, owned by Codex B4) should compile the Wayland-only feature set **dynamically linked against musl** (`-crt-static` — required because `wayland-sys`, `xkbcommon-dl`, and `fontconfig` load shared libraries at runtime on musl):

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
    --features wayland \
    --target x86_64-unknown-linux-musl
```

### Runtime Dependencies Required in `rootfs.erofs`

| Library / Asset | Buildroot Package | Purpose |
| --- | --- | --- |
| `libwayland-client.so.0` + `libwayland-cursor.so.0` | `BR2_PACKAGE_WAYLAND` | Wayland client surface & cursor connection to `desktop-shell` |
| `libxkbcommon.so.0` + `xkeyboard-config` | `BR2_PACKAGE_LIBXKBCOMMON`, `BR2_PACKAGE_XKEYBOARD_CONFIG` | Keyboard layout & keymap translation under Wayland |
| `libfontconfig.so.1` + `libfreetype.so.6` + `libexpat.so.1` | `BR2_PACKAGE_FONTCONFIG`, `BR2_PACKAGE_FREETYPE`, `BR2_PACKAGE_EXPAT` | Slint software text rendering |
| At least one TTF font (e.g. DejaVu Sans) | `BR2_PACKAGE_DEJAVU` | Default UI font discovered via Fontconfig |

**Explicitly NOT required:** X11/Xorg/XCB (`--no-default-features --features wayland` drops `x11`), Mesa, OpenGL, or Vulkan (Slint uses the pure-Rust CPU software renderer).

### Sockets, Paths, and Environment Variables

| Path / Variable | Access Required | Notes |
| --- | --- | --- |
| `XDG_RUNTIME_DIR` + `WAYLAND_DISPLAY` | Wayland socket exposed by `desktop-shell` | Required when launching inside the compositor session |
| `/run/hyperos/vmm.sock` | `root:hyperos 0660` (dir `0750`) | `vmm-daemon` JSON-RPC 2.0 control socket (user in `hyperos` group) |
| `/proc/meminfo` & `/sys/block/zram0/*` | World-readable procfs/sysfs | Live host memory & ZRAM gauges |
| `/var/lib/hyperos/base-images/` | Optional read; `PermissionDenied` handled gracefully | Falls back cleanly when base images dir is `root:root 0700` |
