//! Lightweight JSON-RPC 2.0 bridge between `desktop-shell` and `/run/hyperos/vmm.sock`
//! (matching `ARCHITECTURE.md` verbatim) plus `--mock` backend, `Reclaim RAM`, and one-click Spaces.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const DEFAULT_VMM_SOCKET: &str = "/run/hyperos/vmm.sock";
pub const MAX_FRAME_BYTES: usize = 65_536;

/// Exact `ARCHITECTURE.md` VM wire schema (8 required fields, no unknown keys).
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListVmsResult {
    vms: Vec<VmSummary>,
}

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
            Self::DisposableBrowser => "Safe Web · Self-destructs when closed",
            Self::WorkPod => "Safe Web · Everyday apps & documents",
            Self::OfflineVault => "Zero Network · Air-gapped private files",
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

    pub fn is_ephemeral(self) -> bool {
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
    ) -> Result<VmSummary, String>;
    fn start_vm(&mut self, vm_id: &str) -> Result<VmSummary, String>;
    fn set_balloon_ram(&mut self, vm_id: &str, ram_mib: u32) -> Result<VmSummary, String>;
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
) -> Result<VmSummary, String> {
    let created = bridge.create_vm(
        preset.title(),
        preset.base_image_id(),
        preset.vcpu_count(),
        preset.max_ram_mib(),
        preset.network_mode(),
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
            .map_err(|e| format!("cannot reach {}: {e}", self.socket_path.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;

        stream.write_all(&frame).map_err(|e| e.to_string())?;
        stream.flush().map_err(|e| e.to_string())?;

        let mut reader = BufReader::new(stream);
        let mut resp_line = String::new();
        let n = reader
            .read_line(&mut resp_line)
            .map_err(|e| e.to_string())?;
        if n == 0 || n > MAX_FRAME_BYTES {
            return Err("invalid or oversized response frame".to_string());
        }

        let env: RawResponse<T> =
            serde_json::from_str(&resp_line).map_err(|e| format!("decode error: {e}"))?;
        if env.jsonrpc != "2.0" {
            return Err("invalid jsonrpc version".to_string());
        }
        if let Some(err) = env.error {
            return Err(format!("RPC {}: {}", err.code, err.message));
        }
        env.result
            .ok_or_else(|| "missing result in JSON-RPC response".to_string())
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
    ) -> Result<VmSummary, String> {
        self.call(
            "create_vm",
            serde_json::json!({
                "name": name,
                "base_image_id": base_image_id,
                "vcpu_count": vcpu_count,
                "ram_mib": ram_mib,
                "network_mode": network_mode,
            }),
        )
    }

    fn start_vm(&mut self, vm_id: &str) -> Result<VmSummary, String> {
        self.call("start_vm", serde_json::json!({ "vm_id": vm_id }))
    }

    fn set_balloon_ram(&mut self, vm_id: &str, ram_mib: u32) -> Result<VmSummary, String> {
        self.call(
            "set_balloon_ram",
            serde_json::json!({ "vm_id": vm_id, "ram_mib": ram_mib }),
        )
    }
}

/// In-memory `--mock` VMM bridge pre-populated with realistic HyperOS Spaces.
pub struct MockShellVmmBridge {
    vms: BTreeMap<String, VmSummary>,
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
            },
            VmSummary {
                vm_id: "10000000-0000-4000-8000-000000000003".into(),
                name: "Offline Vault".into(),
                base_image_id: "alpine-3-21-virtio".into(),
                vcpu_count: 1,
                ram_mib: 512,
                balloon_target_mib: 384,
                network_mode: "isolated".into(),
                state: "running".into(),
            },
        ];
        for vm in samples {
            vms.insert(vm.vm_id.clone(), vm);
        }
        Self { vms }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reclaim_idle_ram_deflates_idle_balloons_and_reports_savings() {
        let mut bridge = MockShellVmmBridge::with_sample_spaces();
        let focused = "10000000-0000-4000-8000-000000000001";
        let report = reclaim_idle_ram(&mut bridge, Some(focused)).unwrap();
        // Work Pod (focused): 1024 -> 896 (128 reclaimed)
        // Disposable Browser: 768 -> 256 (512 reclaimed)
        // Offline Vault: 384 -> 256 (128 reclaimed)
        assert_eq!(report.deflated_vms, 3);
        assert_eq!(report.reclaimed_mib, 768);
    }

    #[test]
    fn one_click_preset_creates_and_starts_space() {
        let mut bridge = MockShellVmmBridge::with_sample_spaces();
        let vm = launch_environment_preset(&mut bridge, EnvironmentPreset::OfflineVault).unwrap();
        assert_eq!(vm.name, "Offline Vault");
        assert_eq!(vm.network_mode, "isolated");
        assert_eq!(vm.state, "running");
        assert_eq!(bridge.list_vms().unwrap().len(), 4);
    }
}
