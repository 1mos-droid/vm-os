# HyperOS Zero-Script VM Studio (`vm-control-ui/`)

A lightweight, jargon-free native Rust + Slint desktop manager built for non-technical HyperOS users (pure-Rust `winit` + CPU software renderer for ultra-low RAM usage).

Communicates with `vmm-daemon` over `/run/hyperos/vmm.sock` using the exact JSON-RPC 2.0 contract defined in [`ARCHITECTURE.md`](../ARCHITECTURE.md).

## Four Zero-Script Views

1. **Spaces (Dashboard + Visual Checkpoints):**
   - Live Host Memory (`/proc/meminfo`) and ZRAM (`/sys/block/zram0/mm_stat`) gauges + one-click **`Reclaim RAM`** button (deflates idle VM balloons via `set_balloon_ram`).
   - Color-coded Space cards with plain-English Network & Display badges, one-click Open/Pause/Close (with mandatory hard power-off data-loss confirmation modal), and **Visual Checkpoints** (`Save Checkpoint` wired to `snapshot_vm` + `Rewind` checkpoint cards).
2. **Create Space (Plain-English Flow):**
   - Visual selectors for **Network Access** (*Offline Vault* = `isolated`, *Safe Web* = `nat`, *Home Network* = `host-only`), **Memory Profile** (*Light* / *Standard* / *Heavy* mapped to `virtio-balloon`), **Display Mode** (*Seamless App Window* vs *Full Environment*), and **Self-Destruct on Close** toggle — wired strictly to the 5-field `ARCHITECTURE.md` `create_vm` RPC.
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
```
