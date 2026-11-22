//! Lightweight JSON-RPC 2.0 bridge between `desktop-shell` and `/run/hyperos/vmm.sock`
//! (matching `ARCHITECTURE.md` verbatim) plus `--mock` backend, `Reclaim RAM`, and one-click Spaces.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const DEFAULT_VMM_SOCKET: &str = "/run/hyperos/vmm.sock";
pub const MAX_FRAME_BYTES: usize = 65_536;
pub const CAPTURE_MAX_BYTES: u64 = 256 * 1_048_576;

/// Produce a clear, actionable message when `UnixStream::connect` fails.
pub fn classify_connect_error(socket_path: &Path, err: &io::Error) -> String {
    let path = socket_path.display();
    match err.kind() {
        io::ErrorKind::PermissionDenied => format!(
            "Permission denied connecting to {path}. Ensure your user belongs to the `hyperos` group (socket is root:hyperos 0660 under /run/hyperos 0750), or run with --mock."
        ),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => format!(
            "Cannot reach vmm-daemon at {path} ({err}). Check `sv status /run/service/vmm-daemon` or run with --mock."
        ),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => format!(
            "Timed out connecting to vmm-daemon at {path}. Check `sv status /run/service/vmm-daemon`."
        ),
        _ => format!("Cannot reach {path}: {err}"),
    }
}

/// Produce a clear message when `vmm-daemon` closes the socket without sending a complete frame.
pub fn classify_peer_closed_error(socket_path: &Path, received_any_response: bool) -> String {
    let path = socket_path.display();
    if received_any_response {
        format!("vmm-daemon closed {path} unexpectedly (daemon restarted or frame limit exceeded).")
    } else {
        format!(
            "vmm-daemon closed {path} immediately without responding — access denied by SO_PEERCRED (your user must be UID 0 or a member of the `hyperos` group), or the daemon stopped during handshake."
        )
    }
}

/// Classify mid-stream I/O failures (timeouts, SO_PEERCRED resets, permission errors).
pub fn classify_io_error(
    socket_path: &Path,
    operation: &str,
    err: &io::Error,
    received_any_response: bool,
) -> String {
    let path = socket_path.display();
    match err.kind() {
        io::ErrorKind::PermissionDenied => classify_connect_error(socket_path, err),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => format!(
            "Timed out waiting for vmm-daemon on {path} during {operation}. Do not blindly retry mutations; refresh state via list_vms."
        ),
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::UnexpectedEof => {
            classify_peer_closed_error(socket_path, received_any_response)
        }
        _ => format!("{operation} on {path} failed: {err}"),
    }
}

/// Read a single LF-terminated frame up to `MAX_FRAME_BYTES` without unbounded allocation.
fn read_bounded_frame<R: BufRead>(reader: &mut R, socket_path: &Path) -> Result<Vec<u8>, String> {
    let mut buf = Vec::with_capacity(512);
    loop {
        let available = reader
            .fill_buf()
            .map_err(|e| classify_io_error(socket_path, "socket read", &e, false))?;
        if available.is_empty() {
            return Err(classify_peer_closed_error(socket_path, false));
        }
        if let Some(nl_pos) = available.iter().position(|&b| b == b'\n') {
            let take = nl_pos + 1;
            if buf.len() + take > MAX_FRAME_BYTES {
                return Err(format!(
                    "response frame exceeds maximum {MAX_FRAME_BYTES} bytes"
                ));
            }
            buf.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(buf);
        }
        let len = available.len();
        if buf.len() + len >= MAX_FRAME_BYTES {
            return Err(format!(
                "response frame exceeds maximum {MAX_FRAME_BYTES} bytes without LF"
            ));
        }
        buf.extend_from_slice(available);
        reader.consume(len);
    }
}

/// Exact `ARCHITECTURE.md` Contract v2 VM wire schema (9 required fields, no unknown keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSummary {
    pub vm_id: String,
    pub name: String,
    pub base_image_id: String,
    pub vcpu_count: u32,
    pub ram_mib: u32,
    pub balloon_target_mib: u32,
    pub network_mode: String,
    pub state: String,
    pub disposable: bool,
}

/// Exact `ARCHITECTURE.md` Snapshot wire schema (3 required fields, no unknown keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub vm_id: String,
    pub created_at: String,
}

/// Exact `ARCHITECTURE.md` Contract v2 Capture wire schema (5 required fields, no unknown keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureSummary {
    pub capture_id: String,
    pub vm_id: String,
    pub started_at: String,
    pub state: String,
    pub bytes: u64,
}

/// Format capture byte counter in human-readable IEC units.
pub fn format_capture_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1_048_576 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    }
}

/// Plain-English network badge making explicit that `nat` is internet-only (LAN blocked).
pub fn network_badge_label(mode: &str) -> &'static str {
    match mode {
        "nat" => "NAT · Internet Only",
        "isolated" => "Isolated · Offline",
        "host-only" => "Host-Only · Local",
        _ => "Unknown",
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListVmsResult {
    vms: Vec<VmSummary>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListSnapshotsResult {
    snapshots: Vec<SnapshotSummary>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListCapturesResult {
    captures: Vec<CaptureSummary>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyResult {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResponse<T> {
    jsonrpc: String,
    #[allow(dead_code)]
    id: Option<String>,
    result: Option<T>,
    error: Option<RpcError>,
}

/// Curated one-click environment presets launched from the `Super`-key Action Deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentPreset {
    DisposableBrowser,
    WorkPod,
    OfflineVault,
}

impl EnvironmentPreset {
    pub const ALL: [Self; 3] = [Self::DisposableBrowser, Self::WorkPod, Self::OfflineVault];

    pub fn id(self) -> &'static str {
        match self {
            Self::DisposableBrowser => "disposable-browser",
            Self::WorkPod => "work-pod",
            Self::OfflineVault => "offline-vault",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::DisposableBrowser => "Disposable Browser",
            Self::WorkPod => "Work Pod",
            Self::OfflineVault => "Offline Vault",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::DisposableBrowser => "NAT · Internet Only · Resets disk on close",
            Self::WorkPod => "NAT · Internet Only · Daily apps & snapshots",
            Self::OfflineVault => "Isolated · Offline · Air-gapped private files",
        }
    }

    pub fn base_image_id(self) -> &'static str {
        match self {
            Self::DisposableBrowser => "alpine-3-21-virtio",
            Self::WorkPod => "debian-12-slim-virtio",
            Self::OfflineVault => "alpine-3-21-virtio",
        }
    }

    pub fn network_mode(self) -> &'static str {
        match self {
            Self::DisposableBrowser => "nat",
            Self::WorkPod => "nat",
            Self::OfflineVault => "isolated",
        }
    }

    pub fn vcpu_count(self) -> u32 {
        match self {
            Self::DisposableBrowser => 2,
            Self::WorkPod => 2,
            Self::OfflineVault => 1,
        }
    }

    pub fn max_ram_mib(self) -> u32 {
        match self {
            Self::DisposableBrowser => 1024,
            Self::WorkPod => 1536,
            Self::OfflineVault => 512,
        }
    }

    pub fn idle_balloon_target_mib(self) -> u32 {
        match self {
            Self::DisposableBrowser => 256,
            Self::WorkPod => 512,
            Self::OfflineVault => 256,
        }
    }

    pub fn is_disposable(self) -> bool {
        matches!(self, Self::DisposableBrowser)
    }

    pub fn accent_hex(self) -> &'static str {
        match self {
            Self::DisposableBrowser => "#06b6d4",
            Self::WorkPod => "#10b981",
            Self::OfflineVault => "#f59e0b",
        }
    }
}

/// Result summary from clicking `Reclaim RAM` on the Top Context Rail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimReport {
    pub deflated_vms: usize,
    pub reclaimed_mib: u32,
}

/// Shell bridge abstraction (live `/run/hyperos/vmm.sock` or in-memory `--mock`).
pub trait ShellVmmBridge {
    fn list_vms(&mut self) -> Result<Vec<VmSummary>, String>;
    fn create_vm(
        &mut self,
        name: &str,
        base_image_id: &str,
        vcpu_count: u32,
        ram_mib: u32,
        network_mode: &str,
        disposable: bool,
    ) -> Result<VmSummary, String>;
    fn start_vm(&mut self, vm_id: &str) -> Result<VmSummary, String>;
    fn stop_vm(&mut self, vm_id: &str) -> Result<VmSummary, String>;
    fn set_balloon_ram(&mut self, vm_id: &str, ram_mib: u32) -> Result<VmSummary, String>;
    fn snapshot_vm(&mut self, vm_id: &str) -> Result<SnapshotSummary, String>;
    fn list_snapshots(&mut self, vm_id: &str) -> Result<Vec<SnapshotSummary>, String>;
    fn restore_snapshot(&mut self, vm_id: &str, snapshot_id: &str) -> Result<VmSummary, String>;
    fn delete_snapshot(&mut self, vm_id: &str, snapshot_id: &str) -> Result<(), String>;
    fn start_capture(&mut self, vm_id: &str) -> Result<CaptureSummary, String>;
    fn stop_capture(&mut self, vm_id: &str) -> Result<CaptureSummary, String>;
    fn list_captures(&mut self, vm_id: &str) -> Result<Vec<CaptureSummary>, String>;
    fn delete_capture(&mut self, vm_id: &str, capture_id: &str) -> Result<(), String>;
    fn tick_mock_captures(&mut self) {}
}

/// Deflate balloons of running VMs (skipping the currently focused VM if desired)
/// to immediately reclaim unused RAM back to the HyperOS host.
pub fn reclaim_idle_ram(
    bridge: &mut dyn ShellVmmBridge,
    focused_vm_id: Option<&str>,
) -> Result<ReclaimReport, String> {
    let vms = bridge.list_vms()?;
    let mut deflated_vms = 0usize;
    let mut reclaimed_mib = 0u32;

    for vm in vms {
        if vm.state != "running" {
            continue;
        }
        // Deflate idle (non-focused) VMs aggressively to 256 MiB; gently trim focused VM if >512 MiB.
        let is_focused = focused_vm_id == Some(vm.vm_id.as_str());
        let desired = if is_focused {
            (vm.balloon_target_mib.saturating_sub(128)).max(512.min(vm.ram_mib))
        } else {
            256u32.clamp(128, vm.ram_mib)
        };

        if desired < vm.balloon_target_mib {
            let delta = vm.balloon_target_mib - desired;
            bridge.set_balloon_ram(&vm.vm_id, desired)?;
            deflated_vms += 1;
            reclaimed_mib += delta;
        }
    }

    Ok(ReclaimReport {
        deflated_vms,
        reclaimed_mib,
    })
}

/// Launch a curated one-click environment preset (`create_vm` -> `start_vm`).
pub fn launch_environment_preset(
    bridge: &mut dyn ShellVmmBridge,
    preset: EnvironmentPreset,
    disposable_override: Option<bool>,
) -> Result<VmSummary, String> {
    let disposable = disposable_override.unwrap_or_else(|| preset.is_disposable());
    let created = bridge.create_vm(
        preset.title(),
        preset.base_image_id(),
        preset.vcpu_count(),
        preset.max_ram_mib(),
        preset.network_mode(),
        disposable,
    )?;
    bridge.start_vm(&created.vm_id)
}

/// Live AF_UNIX JSON-RPC 2.0 client speaking `ARCHITECTURE.md` to `/run/hyperos/vmm.sock`.
pub struct UnixVmmBridge {
    socket_path: PathBuf,
    next_id: AtomicU64,
}

impl UnixVmmBridge {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            next_id: AtomicU64::new(1),
        }
    }

    fn call<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T, String> {
        let id = format!("shell-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut frame = serde_json::to_vec(&req).map_err(|e| format!("encode error: {e}"))?;
        frame.push(b'\n');
        if frame.len() > MAX_FRAME_BYTES {
            return Err("request exceeds 65536-byte frame limit".to_string());
        }

        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| classify_connect_error(&self.socket_path, &e))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;

        stream
            .write_all(&frame)
            .and_then(|_| stream.flush())
            .map_err(|e| classify_io_error(&self.socket_path, "socket write", &e, false))?;

        let mut reader = BufReader::new(stream);
        let resp_bytes = read_bounded_frame(&mut reader, &self.socket_path)?;
        let resp_line = std::str::from_utf8(resp_bytes.strip_suffix(b"\n").unwrap_or(&resp_bytes))
            .map_err(|e| format!("response frame is not valid UTF-8: {e}"))?;

        let env: RawResponse<T> =
            serde_json::from_str(resp_line).map_err(|e| format!("decode error: {e}"))?;
        if env.jsonrpc != "2.0" {
            return Err("invalid jsonrpc version".to_string());
        }
        match (env.result, env.error) {
            (Some(_), Some(_)) => {
                Err("protocol violation: response has both result and error".to_string())
            }
            (None, None) => Err("missing result or error in JSON-RPC response".to_string()),
            (None, Some(err)) => Err(format!("RPC {}: {}", err.code, err.message)),
            (Some(result), None) => {
                if env.id.as_deref() != Some(id.as_str()) {
                    return Err("mismatched JSON-RPC response id".to_string());
                }
                Ok(result)
            }
        }
    }
}

impl ShellVmmBridge for UnixVmmBridge {
    fn list_vms(&mut self) -> Result<Vec<VmSummary>, String> {
        let res: ListVmsResult = self.call("list_vms", serde_json::json!({}))?;
        Ok(res.vms)
    }

    fn create_vm(
        &mut self,
        name: &str,
        base_image_id: &str,
        vcpu_count: u32,
        ram_mib: u32,
        network_mode: &str,
        disposable: bool,
    ) -> Result<VmSummary, String> {
        self.call(
            "create_vm",
            serde_json::json!({
                "name": name,
                "base_image_id": base_image_id,
                "vcpu_count": vcpu_count,
                "ram_mib": ram_mib,
                "network_mode": network_mode,
                "disposable": disposable,
            }),
        )
    }

    fn start_vm(&mut self, vm_id: &str) -> Result<VmSummary, String> {
        self.call("start_vm", serde_json::json!({ "vm_id": vm_id }))
    }

    fn stop_vm(&mut self, vm_id: &str) -> Result<VmSummary, String> {
        self.call("stop_vm", serde_json::json!({ "vm_id": vm_id }))
    }

    fn set_balloon_ram(&mut self, vm_id: &str, ram_mib: u32) -> Result<VmSummary, String> {
        self.call(
            "set_balloon_ram",
            serde_json::json!({ "vm_id": vm_id, "ram_mib": ram_mib }),
        )
    }

    fn snapshot_vm(&mut self, vm_id: &str) -> Result<SnapshotSummary, String> {
        self.call("snapshot_vm", serde_json::json!({ "vm_id": vm_id }))
    }

    fn list_snapshots(&mut self, vm_id: &str) -> Result<Vec<SnapshotSummary>, String> {
        let res: ListSnapshotsResult =
            self.call("list_snapshots", serde_json::json!({ "vm_id": vm_id }))?;
        Ok(res.snapshots)
    }

    fn restore_snapshot(&mut self, vm_id: &str, snapshot_id: &str) -> Result<VmSummary, String> {
        self.call(
            "restore_snapshot",
            serde_json::json!({ "vm_id": vm_id, "snapshot_id": snapshot_id }),
        )
    }

    fn delete_snapshot(&mut self, vm_id: &str, snapshot_id: &str) -> Result<(), String> {
        let _: EmptyResult = self.call(
            "delete_snapshot",
            serde_json::json!({ "vm_id": vm_id, "snapshot_id": snapshot_id }),
        )?;
        Ok(())
    }

    fn start_capture(&mut self, vm_id: &str) -> Result<CaptureSummary, String> {
        self.call("start_capture", serde_json::json!({ "vm_id": vm_id }))
    }

    fn stop_capture(&mut self, vm_id: &str) -> Result<CaptureSummary, String> {
        self.call("stop_capture", serde_json::json!({ "vm_id": vm_id }))
    }

    fn list_captures(&mut self, vm_id: &str) -> Result<Vec<CaptureSummary>, String> {
        let res: ListCapturesResult =
            self.call("list_captures", serde_json::json!({ "vm_id": vm_id }))?;
        Ok(res.captures)
    }

    fn delete_capture(&mut self, vm_id: &str, capture_id: &str) -> Result<(), String> {
        let _: EmptyResult = self.call(
            "delete_capture",
            serde_json::json!({ "vm_id": vm_id, "capture_id": capture_id }),
        )?;
        Ok(())
    }
}

/// In-memory `--mock` VMM bridge pre-populated with realistic HyperOS Spaces.
pub struct MockShellVmmBridge {
    vms: BTreeMap<String, VmSummary>,
    snapshots: Vec<SnapshotSummary>,
    captures: Vec<CaptureSummary>,
}

impl MockShellVmmBridge {
    pub fn with_sample_spaces() -> Self {
        let mut vms = BTreeMap::new();
        let samples = [
            VmSummary {
                vm_id: "10000000-0000-4000-8000-000000000001".into(),
                name: "Work Pod".into(),
                base_image_id: "debian-12-slim-virtio".into(),
                vcpu_count: 2,
                ram_mib: 1536,
                balloon_target_mib: 1024,
                network_mode: "nat".into(),
                state: "running".into(),
                disposable: false,
            },
            VmSummary {
                vm_id: "10000000-0000-4000-8000-000000000002".into(),
                name: "Disposable Browser".into(),
                base_image_id: "alpine-3-21-virtio".into(),
                vcpu_count: 2,
                ram_mib: 1024,
                balloon_target_mib: 768,
                network_mode: "nat".into(),
                state: "running".into(),
                disposable: true,
            },
            VmSummary {
                vm_id: "10000000-0000-4000-8000-000000000003".into(),
                name: "Offline Vault".into(),
                base_image_id: "alpine-3-21-virtio".into(),
                vcpu_count: 1,
                ram_mib: 512,
                balloon_target_mib: 384,
                network_mode: "isolated".into(),
                state: "stopped".into(),
                disposable: false,
            },
        ];
        for vm in samples {
            vms.insert(vm.vm_id.clone(), vm);
        }
        let snapshots = vec![SnapshotSummary {
            snapshot_id: "5c102e90-8c4a-4b11-9222-104088ab1003".into(),
            vm_id: "10000000-0000-4000-8000-000000000003".into(),
            created_at: "2026-11-22T09:30:00Z".into(),
        }];
        let captures = vec![CaptureSummary {
            capture_id: "0c2f6e9a-7b1d-4c3e-9f80-2d5a6b7c8e02".into(),
            vm_id: "10000000-0000-4000-8000-000000000002".into(),
            started_at: "2026-11-22T10:00:00Z".into(),
            state: "stopped".into(),
            bytes: 32_768,
        }];
        Self {
            vms,
            snapshots,
            captures,
        }
    }
}

impl ShellVmmBridge for MockShellVmmBridge {
    fn list_vms(&mut self) -> Result<Vec<VmSummary>, String> {
        Ok(self.vms.values().cloned().collect())
    }

    fn create_vm(
        &mut self,
        name: &str,
        base_image_id: &str,
        vcpu_count: u32,
        ram_mib: u32,
        network_mode: &str,
        disposable: bool,
    ) -> Result<VmSummary, String> {
        let vm_id = uuid::Uuid::new_v4().hyphenated().to_string();
        let vm = VmSummary {
            vm_id: vm_id.clone(),
            name: name.to_string(),
            base_image_id: base_image_id.to_string(),
            vcpu_count,
            ram_mib,
            balloon_target_mib: ram_mib,
            network_mode: network_mode.to_string(),
            state: "stopped".to_string(),
            disposable,
        };
        self.vms.insert(vm_id, vm.clone());
        Ok(vm)
    }

    fn start_vm(&mut self, vm_id: &str) -> Result<VmSummary, String> {
        let vm = self
            .vms
            .get_mut(vm_id)
            .ok_or_else(|| "VM not found".to_string())?;
        vm.state = "running".to_string();
        vm.balloon_target_mib = vm.ram_mib;
        Ok(vm.clone())
    }

    fn stop_vm(&mut self, vm_id: &str) -> Result<VmSummary, String> {
        let vm = self
            .vms
            .get_mut(vm_id)
            .ok_or_else(|| "VM not found".to_string())?;
        vm.state = "stopped".to_string();
        for cap in &mut self.captures {
            if cap.vm_id == vm_id && cap.state == "running" {
                cap.state = "stopped".to_string();
            }
        }
        Ok(vm.clone())
    }

    fn set_balloon_ram(&mut self, vm_id: &str, ram_mib: u32) -> Result<VmSummary, String> {
        let vm = self
            .vms
            .get_mut(vm_id)
            .ok_or_else(|| "VM not found".to_string())?;
        if vm.state != "running" {
            return Err("VM must be running to adjust balloon".to_string());
        }
        if ram_mib < 128 || ram_mib > vm.ram_mib {
            return Err("balloon ram_mib out of bounds".to_string());
        }
        vm.balloon_target_mib = ram_mib;
        Ok(vm.clone())
    }

    fn snapshot_vm(&mut self, vm_id: &str) -> Result<SnapshotSummary, String> {
        let vm = self
            .vms
            .get(vm_id)
            .ok_or_else(|| "VM not found".to_string())?;
        if vm.disposable {
            return Err("RPC -32002: snapshot_vm is disallowed on disposable VMs".to_string());
        }
        if vm.state != "stopped" {
            return Err("RPC -32002: snapshot_vm requires a stopped VM".to_string());
        }
        let snap = SnapshotSummary {
            snapshot_id: uuid::Uuid::new_v4().hyphenated().to_string(),
            vm_id: vm.vm_id.clone(),
            created_at: "2026-11-22T12:00:00Z".to_string(),
        };
        self.snapshots.push(snap.clone());
        Ok(snap)
    }

    fn list_snapshots(&mut self, vm_id: &str) -> Result<Vec<SnapshotSummary>, String> {
        if !self.vms.contains_key(vm_id) {
            return Err("RPC -32001: VM not found".to_string());
        }
        let mut list: Vec<SnapshotSummary> = self
            .snapshots
            .iter()
            .filter(|s| s.vm_id == vm_id)
            .cloned()
            .collect();
        list.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.snapshot_id.cmp(&b.snapshot_id))
        });
        Ok(list)
    }

    fn restore_snapshot(&mut self, vm_id: &str, snapshot_id: &str) -> Result<VmSummary, String> {
        let vm = self
            .vms
            .get(vm_id)
            .cloned()
            .ok_or_else(|| "RPC -32001: VM not found".to_string())?;
        if vm.disposable {
            return Err("RPC -32002: restore_snapshot is disallowed on disposable VMs".to_string());
        }
        if !self
            .snapshots
            .iter()
            .any(|s| s.vm_id == vm_id && s.snapshot_id == snapshot_id)
        {
            return Err("RPC -32001: snapshot not found".to_string());
        }
        if vm.state != "stopped" {
            return Err("RPC -32002: restore_snapshot requires a stopped VM".to_string());
        }
        Ok(vm)
    }

    fn delete_snapshot(&mut self, vm_id: &str, snapshot_id: &str) -> Result<(), String> {
        if !self.vms.contains_key(vm_id) {
            return Err("RPC -32001: VM not found".to_string());
        }
        let before = self.snapshots.len();
        self.snapshots
            .retain(|s| !(s.vm_id == vm_id && s.snapshot_id == snapshot_id));
        if self.snapshots.len() == before {
            return Err("RPC -32001: snapshot not found".to_string());
        }
        Ok(())
    }

    fn start_capture(&mut self, vm_id: &str) -> Result<CaptureSummary, String> {
        let vm = self
            .vms
            .get(vm_id)
            .ok_or_else(|| "RPC -32001: VM not found".to_string())?;
        if vm.state != "running" {
            return Err("RPC -32002: start_capture requires a running VM".to_string());
        }
        if vm.network_mode == "isolated" {
            return Err("RPC -32004: start_capture unavailable for isolated VM".to_string());
        }
        if self
            .captures
            .iter()
            .any(|c| c.vm_id == vm_id && c.state == "running")
        {
            return Err("RPC -32002: capture already running".to_string());
        }
        let cap = CaptureSummary {
            capture_id: uuid::Uuid::new_v4().hyphenated().to_string(),
            vm_id: vm.vm_id.clone(),
            started_at: "2026-11-22T12:00:00Z".to_string(),
            state: "running".to_string(),
            bytes: 0,
        };
        self.captures.push(cap.clone());
        Ok(cap)
    }

    fn stop_capture(&mut self, vm_id: &str) -> Result<CaptureSummary, String> {
        if !self.vms.contains_key(vm_id) {
            return Err("RPC -32001: VM not found".to_string());
        }
        if let Some(active) = self
            .captures
            .iter_mut()
            .find(|c| c.vm_id == vm_id && c.state == "running")
        {
            active.state = "stopped".to_string();
            return Ok(active.clone());
        }
        self.captures
            .iter()
            .filter(|c| c.vm_id == vm_id)
            .max_by(|a, b| {
                a.started_at
                    .cmp(&b.started_at)
                    .then_with(|| a.capture_id.cmp(&b.capture_id))
            })
            .cloned()
            .ok_or_else(|| "RPC -32001: no capture exists for VM".to_string())
    }

    fn list_captures(&mut self, vm_id: &str) -> Result<Vec<CaptureSummary>, String> {
        if !self.vms.contains_key(vm_id) {
            return Err("RPC -32001: VM not found".to_string());
        }
        let mut list: Vec<CaptureSummary> = self
            .captures
            .iter()
            .filter(|c| c.vm_id == vm_id)
            .cloned()
            .collect();
        list.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.capture_id.cmp(&b.capture_id))
        });
        Ok(list)
    }

    fn delete_capture(&mut self, vm_id: &str, capture_id: &str) -> Result<(), String> {
        if !self.vms.contains_key(vm_id) {
            return Err("RPC -32001: VM not found".to_string());
        }
        let Some(pos) = self
            .captures
            .iter()
            .position(|c| c.vm_id == vm_id && c.capture_id == capture_id)
        else {
            return Err("RPC -32001: capture not found".to_string());
        };
        if self.captures[pos].state == "running" {
            return Err("RPC -32002: cannot delete a running capture; stop it first".to_string());
        }
        self.captures.remove(pos);
        Ok(())
    }

    fn tick_mock_captures(&mut self) {
        for cap in &mut self.captures {
            if cap.state == "running" {
                let next = cap.bytes.saturating_add(4096).min(CAPTURE_MAX_BYTES);
                cap.bytes = next;
                if next >= CAPTURE_MAX_BYTES {
                    cap.state = "limit_reached".to_string();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::thread;

    #[test]
    fn classifies_socket_permission_missing_timeout_and_so_peercred_errors() {
        let sock = Path::new("/run/hyperos/vmm.sock");

        let perm = classify_connect_error(
            sock,
            &io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"),
        );
        assert!(perm.contains("hyperos"));
        assert!(perm.contains("0660"));

        let missing = classify_connect_error(
            sock,
            &io::Error::new(io::ErrorKind::NotFound, "no such file"),
        );
        assert!(missing.contains("vmm-daemon"));

        let refused = classify_connect_error(
            sock,
            &io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"),
        );
        assert!(refused.contains("vmm-daemon"));

        let peercred = classify_peer_closed_error(sock, false);
        assert!(peercred.contains("SO_PEERCRED"));
        assert!(peercred.contains("hyperos"));

        let timed_out = classify_io_error(
            sock,
            "socket read",
            &io::Error::new(io::ErrorKind::TimedOut, "timed out"),
            false,
        );
        assert!(timed_out.contains("Timed out"));
    }

    #[test]
    fn unix_vmm_bridge_reports_so_peercred_and_accepts_0660_socket() {
        let sock_path =
            std::env::temp_dir().join(format!("hyperos-shell-sock-{}.sock", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();
        std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o660)).unwrap();

        let server = thread::spawn(move || {
            // Connection 1: simulate SO_PEERCRED immediate close without response.
            let (s1, _) = listener.accept().unwrap();
            drop(s1);

            // Connection 2: respond to list_vms cleanly.
            let (s2, _) = listener.accept().unwrap();
            let mut w = s2.try_clone().unwrap();
            let mut r = BufReader::new(s2);
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = req["id"].as_str().unwrap();
            let resp =
                format!("{{\"jsonrpc\":\"2.0\",\"id\":\"{id}\",\"result\":{{\"vms\":[]}}}}\n");
            w.write_all(resp.as_bytes()).unwrap();
            w.flush().unwrap();
        });

        let mut bridge = UnixVmmBridge::new(&sock_path);
        let err = bridge.list_vms().unwrap_err();
        assert!(err.contains("SO_PEERCRED"), "unexpected error: {err}");
        assert!(err.contains("hyperos"), "unexpected error: {err}");

        let vms = bridge.list_vms().unwrap();
        assert!(vms.is_empty());

        server.join().unwrap();
        let _ = std::fs::remove_file(&sock_path);
    }

    #[test]
    fn reclaim_idle_ram_deflates_idle_balloons_and_reports_savings() {
        let mut bridge = MockShellVmmBridge::with_sample_spaces();
        let focused = "10000000-0000-4000-8000-000000000001";
        let report = reclaim_idle_ram(&mut bridge, Some(focused)).unwrap();
        // Work Pod (focused, running): 1024 -> 896 (128 reclaimed)
        // Disposable Browser (running): 768 -> 256 (512 reclaimed)
        // Offline Vault starts stopped so snapshots are immediately testable.
        assert_eq!(report.deflated_vms, 2);
        assert_eq!(report.reclaimed_mib, 640);
    }

    #[test]
    fn one_click_preset_creates_and_starts_space_and_respects_disposable_override() {
        let mut bridge = MockShellVmmBridge::with_sample_spaces();
        let vm =
            launch_environment_preset(&mut bridge, EnvironmentPreset::OfflineVault, Some(true))
                .unwrap();
        assert_eq!(vm.name, "Offline Vault");
        assert_eq!(vm.network_mode, "isolated");
        assert_eq!(vm.state, "running");
        assert!(vm.disposable);
        assert_eq!(bridge.list_vms().unwrap().len(), 4);

        let browser =
            launch_environment_preset(&mut bridge, EnvironmentPreset::DisposableBrowser, None)
                .unwrap();
        assert!(browser.disposable);
    }

    #[test]
    fn unix_vmm_bridge_serializes_contract_v2_disposable_snapshot_and_capture_rpcs() {
        let sock_path =
            std::env::temp_dir().join(format!("hyperos-shell-v2-{}.sock", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        let vm_id = "10000000-0000-4000-8000-000000000099";
        let snap_id = "5c102e90-8c4a-4b11-9222-104088ab1099";
        let cap_id = "0c2f6e9a-7b1d-4c3e-9f80-2d5a6b7c8e99";

        let server = thread::spawn(move || {
            for _ in 0..8 {
                let (stream, _) = listener.accept().unwrap();
                let mut w = stream.try_clone().unwrap();
                let mut r = BufReader::new(stream);
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                let id = req["id"].as_str().unwrap();
                let method = req["method"].as_str().unwrap();
                let result = match method {
                    "create_vm" => {
                        assert_eq!(req["params"]["disposable"], serde_json::json!(true));
                        serde_json::json!({
                            "vm_id": vm_id,
                            "name": "Web",
                            "base_image_id": "alpine-3-21-virtio",
                            "vcpu_count": 2,
                            "ram_mib": 1024,
                            "balloon_target_mib": 1024,
                            "network_mode": "nat",
                            "state": "stopped",
                            "disposable": true
                        })
                    }
                    "list_snapshots" => serde_json::json!({
                        "snapshots": [{"snapshot_id": snap_id, "vm_id": vm_id, "created_at": "2026-11-22T00:00:00Z"}]
                    }),
                    "restore_snapshot" => {
                        assert_eq!(req["params"]["snapshot_id"], serde_json::json!(snap_id));
                        serde_json::json!({
                            "vm_id": vm_id,
                            "name": "Web",
                            "base_image_id": "alpine-3-21-virtio",
                            "vcpu_count": 2,
                            "ram_mib": 1024,
                            "balloon_target_mib": 1024,
                            "network_mode": "nat",
                            "state": "stopped",
                            "disposable": false
                        })
                    }
                    "delete_snapshot" => serde_json::json!({}),
                    "start_capture" | "stop_capture" => serde_json::json!({
                        "capture_id": cap_id,
                        "vm_id": vm_id,
                        "started_at": "2026-11-22T00:00:00Z",
                        "state": if method == "start_capture" { "running" } else { "stopped" },
                        "bytes": 4096
                    }),
                    "list_captures" => serde_json::json!({
                        "captures": [{
                            "capture_id": cap_id,
                            "vm_id": vm_id,
                            "started_at": "2026-11-22T00:00:00Z",
                            "state": "stopped",
                            "bytes": 4096
                        }]
                    }),
                    "delete_capture" => serde_json::json!({}),
                    other => panic!("unexpected method: {other}"),
                };
                let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
                let mut frame = serde_json::to_vec(&resp).unwrap();
                frame.push(b'\n');
                w.write_all(&frame).unwrap();
                w.flush().unwrap();
            }
        });

        let mut bridge = UnixVmmBridge::new(&sock_path);
        let created = bridge
            .create_vm("Web", "alpine-3-21-virtio", 2, 1024, "nat", true)
            .unwrap();
        assert!(created.disposable);
        assert_eq!(bridge.list_snapshots(vm_id).unwrap().len(), 1);
        assert!(!bridge.restore_snapshot(vm_id, snap_id).unwrap().disposable);
        bridge.delete_snapshot(vm_id, snap_id).unwrap();
        assert_eq!(bridge.start_capture(vm_id).unwrap().state, "running");
        assert_eq!(bridge.stop_capture(vm_id).unwrap().state, "stopped");
        assert_eq!(bridge.list_captures(vm_id).unwrap()[0].bytes, 4096);
        bridge.delete_capture(vm_id, cap_id).unwrap();

        // Missing disposable field in VmSummary must fail strict deserialization
        let missing_disp = r#"{"vm_id":"1","name":"x","base_image_id":"a","vcpu_count":1,"ram_mib":512,"balloon_target_mib":512,"network_mode":"nat","state":"stopped"}"#;
        assert!(serde_json::from_str::<VmSummary>(missing_disp).is_err());

        server.join().unwrap();
        let _ = std::fs::remove_file(&sock_path);
    }
}
