# HyperOS VM Control UI (`vm-control-ui/`)

Lightweight native Rust desktop GUI application built with [Slint](https://slint.rs) (pure-Rust `winit` + CPU software renderer for minimal RAM usage on 4 GiB HyperOS hosts).

Communicates with `vmm-daemon` over `/run/hyperos/vmm.sock` using the exact JSON-RPC 2.0 wire contract defined in [`ARCHITECTURE.md`](../ARCHITECTURE.md).

## Features

- **Dashboard:**
  - Live host RAM meter parsed from `/proc/meminfo` (`used = MemTotal - MemAvailable`, plus `MemTotal - 768 MiB` VM admission budget).
  - Live ZRAM meter parsed from `/sys/block/zram0/mm_stat`, `disksize`, and `comp_algorithm` (with graceful simulated fallback in `--mock` mode).
  - VM list (`list_vms`, sorted by `vm_id`) with lifecycle actions: `start_vm`, `pause_vm`, `snapshot_vm` (cold snapshot while stopped), and `stop_vm` (with mandatory hard power-off data-loss confirmation dialog).
  - Live `set_balloon_ram` controls for running VMs and `set_network_mode` controls for stopped VMs.
- **Create Controlled VM Wizard:**
  - Guest template picker reading `/var/lib/hyperos/base-images/<image_id>/manifest.json` (with built-in templates in `--mock` mode).
  - Dynamic memory ballooning sliders: **Max RAM** (`128–3072 MiB`, wired to `create_vm.ram_mib`) and **Min RAM** (`128..=Max RAM MiB`, tracked as the desired post-boot `set_balloon_ram` target).
  - vCPU count selector (`1..=min(64, host online CPUs)`).
  - Network sandbox mode selector (`isolated`, `nat`, `host-only`).
  - Ephemeral session toggle (client-tracked overlay disposal policy; `create_vm` sends strictly the 5 `ARCHITECTURE.md` v1 contract fields with no unknown keys).
- **In-Memory Mock Mode (`--mock`):**
  - Launches and exercises the full UI without `vmm-daemon` running.

## Build, Test, and Run

```sh
cd vm-control-ui

# Run all JSON-RPC 2.0, framing, mock VMM, telemetry, and template unit tests
cargo test

# Launch against the in-memory mock backend (no vmm-daemon required)
cargo run -- --mock

# Launch against a live vmm-daemon socket (defaults to /run/hyperos/vmm.sock)
cargo run -- --socket /run/hyperos/vmm.sock
```
