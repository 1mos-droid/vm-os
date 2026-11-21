# HyperOS Workspace-First Desktop Shell (`desktop-shell/`)

An original, ultra-low-RAM Wayland desktop shell built in Rust with **Smithay 0.7** and **Slint** (pure-Rust software renderer), targeting **< 100 MB total RAM usage** on 4 GiB HyperOS machines.

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

## Build, Test, and Run

```sh
cd desktop-shell

# Run compositor, snapping, transition, origin-border, VMM bridge, and headless Slint UI tests
cargo test --all-targets

# Launch the desktop shell with mock VMs and fallback telemetry (no vmm-daemon required)
cargo run -- --mock

# Run the Smithay Wayland compositor headlessly (useful for CI smoke tests)
cargo run -- --mock --headless

# Enable bare-metal DRM/KMS/libinput/seatd backend on HyperOS target builds
cargo build --release --features kms
```
