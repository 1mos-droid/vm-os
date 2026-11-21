//! In-memory mock VMM backend (`--mock`) enforcing all `ARCHITECTURE.md` rules.

use crate::client::{ClientError, VmmBackend};
use crate::rpc::{
    encode_frame, error_code, is_valid_image_id, is_valid_uuid, is_valid_vm_name, CreateVmParams,
    ListVmsResult, NetworkMode, RpcError, RpcMethodCall, RpcMethodEnvelope, RpcResponse, Snapshot,
    Vm, VmState, HOST_RAM_RESERVE_MIB, MAX_FRAME_BYTES, MAX_VCPU_COUNT, MAX_VM_RAM_MIB,
    MIN_VCPU_COUNT, MIN_VM_RAM_MIB,
};
use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// In-memory mock implementation of the HyperOS `vmm-daemon` RPC contract.
///
/// Used when `vm-control-ui --mock` is launched without a live `vmm-daemon`,
/// and in unit/integration tests.
pub struct MockVmmBackend {
    host_mem_total_mib: u64,
    host_online_cpus: u32,
    known_images: HashSet<String>,
    vms: BTreeMap<String, Vm>,
    snapshots: Vec<Snapshot>,
}

impl MockVmmBackend {
    /// Create a mock backend pre-populated with realistic HyperOS sample VMs.
    pub fn with_sample_vms() -> Self {
        let mut backend = Self::empty(4096, 4);

        let alpine_id = "2b1c48a0-9e3f-4c11-9a12-104088ab0001".to_string();
        backend.vms.insert(
            alpine_id.clone(),
            Vm {
                vm_id: alpine_id,
                name: "alpine-sandbox".to_string(),
                base_image_id: "alpine-3-21-virtio".to_string(),
                vcpu_count: 1,
                ram_mib: 1024,
                balloon_target_mib: 512,
                network_mode: NetworkMode::Isolated,
                state: VmState::Running,
            },
        );

        let debian_id = "7d5a9f12-4421-4b90-8c34-99f012cd0002".to_string();
        backend.vms.insert(
            debian_id.clone(),
            Vm {
                vm_id: debian_id,
                name: "debian-dev".to_string(),
                base_image_id: "debian-12-slim-virtio".to_string(),
                vcpu_count: 2,
                ram_mib: 1536,
                balloon_target_mib: 1536,
                network_mode: NetworkMode::HostOnly,
                state: VmState::Stopped,
            },
        );

        backend
    }

    /// Create an empty mock backend with explicit host RAM (MiB) and CPU count.
    pub fn empty(host_mem_total_mib: u64, host_online_cpus: u32) -> Self {
        let known_images = ["alpine-3-21-virtio", "debian-12-slim-virtio", "linux-demo"]
            .into_iter()
            .map(String::from)
            .collect();

        Self {
            host_mem_total_mib,
            host_online_cpus: host_online_cpus.clamp(MIN_VCPU_COUNT, MAX_VCPU_COUNT),
            known_images,
            vms: BTreeMap::new(),
            snapshots: Vec::new(),
        }
    }

    pub fn register_image(&mut self, image_id: impl Into<String>) {
        self.known_images.insert(image_id.into());
    }

    pub fn snapshots(&self) -> &[Snapshot] {
        &self.snapshots
    }

    /// Admission capacity per `ARCHITECTURE.md`:
    /// `MemTotal - 768 MiB`; swap is not admission capacity.
    fn admission_budget_mib(&self) -> u64 {
        self.host_mem_total_mib.saturating_sub(HOST_RAM_RESERVE_MIB)
    }

    /// Total maximum RAM of currently running or paused guests.
    fn active_guest_max_ram_mib(&self) -> u64 {
        self.vms
            .values()
            .filter(|vm| matches!(vm.state, VmState::Running | VmState::Paused))
            .map(|vm| u64::from(vm.ram_mib))
            .sum()
    }

    /// Process a single raw JSON-RPC 2.0 frame and return a serialized LF-terminated response.
    /// Useful for end-to-end framing tests against `UnixSocketClient`.
    pub fn handle_raw_frame(&mut self, frame: &[u8]) -> Vec<u8> {
        if frame.len() > MAX_FRAME_BYTES || !frame.ends_with(b"\n") {
            let err = RpcResponse::<serde_json::Value>::Error {
                id: None,
                error: RpcError {
                    code: error_code::INVALID_REQUEST,
                    message: "frame exceeds 65536 bytes or missing LF".to_string(),
                },
            };
            return encode_frame(&err).expect("error frame fits");
        }

        let text = match std::str::from_utf8(&frame[..frame.len() - 1]) {
            Ok(s) => s,
            Err(_) => {
                let err = RpcResponse::<serde_json::Value>::Error {
                    id: None,
                    error: RpcError {
                        code: error_code::PARSE_ERROR,
                        message: "invalid UTF-8".to_string(),
                    },
                };
                return encode_frame(&err).expect("error frame fits");
            }
        };

        let envelope: RpcMethodEnvelope = match serde_json::from_str(text) {
            Ok(env) => env,
            Err(e) => {
                let err = RpcResponse::<serde_json::Value>::Error {
                    id: None,
                    error: RpcError {
                        code: error_code::INVALID_PARAMS,
                        message: format!("invalid JSON-RPC request: {e}"),
                    },
                };
                return encode_frame(&err).expect("error frame fits");
            }
        };

        let id = envelope.id.clone();
        let response: RpcResponse<serde_json::Value> = match envelope.call {
            RpcMethodCall::ListVms(_) => match self.list_vms() {
                Ok(vms) => RpcResponse::Success {
                    id,
                    result: serde_json::to_value(ListVmsResult { vms }).unwrap(),
                },
                Err(ClientError::Rpc(error)) => RpcResponse::Error {
                    id: Some(id),
                    error,
                },
                Err(ClientError::Transport(message)) => RpcResponse::Error {
                    id: Some(id),
                    error: RpcError {
                        code: error_code::INTERNAL_ERROR,
                        message,
                    },
                },
            },
            RpcMethodCall::CreateVm(params) => to_json_response(id, self.create_vm(params)),
            RpcMethodCall::StartVm(p) => to_json_response(id, self.start_vm(&p.vm_id)),
            RpcMethodCall::StopVm(p) => to_json_response(id, self.stop_vm(&p.vm_id)),
            RpcMethodCall::PauseVm(p) => to_json_response(id, self.pause_vm(&p.vm_id)),
            RpcMethodCall::SnapshotVm(p) => to_json_response(id, self.snapshot_vm(&p.vm_id)),
            RpcMethodCall::SetBalloonRam(p) => {
                to_json_response(id, self.set_balloon_ram(&p.vm_id, p.ram_mib))
            }
            RpcMethodCall::SetNetworkMode(p) => {
                to_json_response(id, self.set_network_mode(&p.vm_id, p.network_mode))
            }
        };

        encode_frame(&response).expect("mock response frame fits")
    }
}

fn to_json_response<T: serde::Serialize>(
    id: String,
    res: Result<T, ClientError>,
) -> RpcResponse<serde_json::Value> {
    match res {
        Ok(value) => RpcResponse::Success {
            id,
            result: serde_json::to_value(value).unwrap(),
        },
        Err(ClientError::Rpc(error)) => RpcResponse::Error {
            id: Some(id),
            error,
        },
        Err(ClientError::Transport(message)) => RpcResponse::Error {
            id: Some(id),
            error: RpcError {
                code: error_code::INTERNAL_ERROR,
                message,
            },
        },
    }
}

fn rpc_err(code: i64, message: impl Into<String>) -> ClientError {
    ClientError::Rpc(RpcError {
        code,
        message: message.into(),
    })
}

/// Format current UTC wall-clock time as RFC3339 seconds (`YYYY-MM-DDTHH:MM:SSZ`).
fn utc_rfc3339_seconds() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let hour = sod / 3600;
    let minute = (sod % 3600) / 60;
    let second = sod % 60;

    // Civil date from Unix days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

impl VmmBackend for MockVmmBackend {
    fn backend_label(&self) -> String {
        "Mock VMM (in-memory)".to_string()
    }

    fn is_connected(&self) -> bool {
        true
    }

    fn list_vms(&mut self) -> Result<Vec<Vm>, ClientError> {
        // BTreeMap guarantees ascending sort by canonical lowercase `vm_id`.
        Ok(self.vms.values().cloned().collect())
    }

    fn create_vm(&mut self, params: CreateVmParams) -> Result<Vm, ClientError> {
        if !is_valid_vm_name(&params.name) {
            return Err(rpc_err(
                error_code::INVALID_PARAMS,
                "name must be 1-64 Unicode code points without control characters",
            ));
        }
        if !is_valid_image_id(&params.base_image_id) {
            return Err(rpc_err(
                error_code::INVALID_PARAMS,
                "base_image_id must be 1-64 ASCII letters, digits, '_' or '-'",
            ));
        }
        if !self.known_images.contains(&params.base_image_id) {
            return Err(rpc_err(
                error_code::NOT_FOUND,
                format!("base image '{}' not found", params.base_image_id),
            ));
        }
        if params.vcpu_count < MIN_VCPU_COUNT
            || params.vcpu_count > MAX_VCPU_COUNT
            || params.vcpu_count > self.host_online_cpus
        {
            return Err(rpc_err(
                error_code::INSUFFICIENT_RESOURCES,
                format!(
                    "vcpu_count {} exceeds allowed range 1..={}",
                    params.vcpu_count, self.host_online_cpus
                ),
            ));
        }
        if params.ram_mib < MIN_VM_RAM_MIB || params.ram_mib > MAX_VM_RAM_MIB {
            return Err(rpc_err(
                error_code::INVALID_PARAMS,
                format!(
                    "ram_mib {} must be within {}..={} MiB",
                    params.ram_mib, MIN_VM_RAM_MIB, MAX_VM_RAM_MIB
                ),
            ));
        }
        let projected = self.active_guest_max_ram_mib() + u64::from(params.ram_mib);
        if projected > self.admission_budget_mib() {
            return Err(rpc_err(
                error_code::INSUFFICIENT_RESOURCES,
                format!(
                    "insufficient RAM admission capacity: requested {} MiB + active {} MiB > budget {} MiB",
                    params.ram_mib,
                    self.active_guest_max_ram_mib(),
                    self.admission_budget_mib()
                ),
            ));
        }

        let vm_id = uuid::Uuid::new_v4().hyphenated().to_string();
        let vm = Vm {
            vm_id: vm_id.clone(),
            name: params.name,
            base_image_id: params.base_image_id,
            vcpu_count: params.vcpu_count,
            ram_mib: params.ram_mib,
            balloon_target_mib: params.ram_mib,
            network_mode: params.network_mode,
            state: VmState::Stopped,
        };
        self.vms.insert(vm_id, vm.clone());
        Ok(vm)
    }

    fn start_vm(&mut self, vm_id: &str) -> Result<Vm, ClientError> {
        if !is_valid_uuid(vm_id) {
            return Err(rpc_err(error_code::INVALID_PARAMS, "invalid vm_id UUID"));
        }
        let current = self
            .vms
            .get(vm_id)
            .cloned()
            .ok_or_else(|| rpc_err(error_code::NOT_FOUND, "VM not found"))?;

        match current.state {
            VmState::Running => Ok(current),
            VmState::Paused => {
                // Resume preserves current balloon target.
                let vm = self.vms.get_mut(vm_id).unwrap();
                vm.state = VmState::Running;
                Ok(vm.clone())
            }
            VmState::Stopped | VmState::Failed => {
                let projected = self.active_guest_max_ram_mib() + u64::from(current.ram_mib);
                if projected > self.admission_budget_mib() {
                    return Err(rpc_err(
                        error_code::INSUFFICIENT_RESOURCES,
                        format!(
                            "insufficient RAM to start VM: {} MiB needed exceeds remaining {} MiB budget",
                            current.ram_mib,
                            self.admission_budget_mib()
                                .saturating_sub(self.active_guest_max_ram_mib())
                        ),
                    ));
                }
                let vm = self.vms.get_mut(vm_id).unwrap();
                vm.state = VmState::Running;
                // Per ARCHITECTURE.md: Balloon target resets to maximum at each cold start.
                vm.balloon_target_mib = vm.ram_mib;
                Ok(vm.clone())
            }
        }
    }

    fn stop_vm(&mut self, vm_id: &str) -> Result<Vm, ClientError> {
        if !is_valid_uuid(vm_id) {
            return Err(rpc_err(error_code::INVALID_PARAMS, "invalid vm_id UUID"));
        }
        let vm = self
            .vms
            .get_mut(vm_id)
            .ok_or_else(|| rpc_err(error_code::NOT_FOUND, "VM not found"))?;
        vm.state = VmState::Stopped;
        Ok(vm.clone())
    }

    fn pause_vm(&mut self, vm_id: &str) -> Result<Vm, ClientError> {
        if !is_valid_uuid(vm_id) {
            return Err(rpc_err(error_code::INVALID_PARAMS, "invalid vm_id UUID"));
        }
        let vm = self
            .vms
            .get_mut(vm_id)
            .ok_or_else(|| rpc_err(error_code::NOT_FOUND, "VM not found"))?;
        match vm.state {
            VmState::Running => {
                vm.state = VmState::Paused;
                Ok(vm.clone())
            }
            VmState::Paused => Ok(vm.clone()),
            VmState::Stopped | VmState::Failed => Err(rpc_err(
                error_code::INVALID_STATE,
                "VM must be running to pause",
            )),
        }
    }

    fn snapshot_vm(&mut self, vm_id: &str) -> Result<Snapshot, ClientError> {
        if !is_valid_uuid(vm_id) {
            return Err(rpc_err(error_code::INVALID_PARAMS, "invalid vm_id UUID"));
        }
        let vm = self
            .vms
            .get(vm_id)
            .ok_or_else(|| rpc_err(error_code::NOT_FOUND, "VM not found"))?;
        if vm.state != VmState::Stopped {
            return Err(rpc_err(
                error_code::INVALID_STATE,
                "snapshot_vm is allowed only while the VM is stopped",
            ));
        }
        let snapshot = Snapshot {
            snapshot_id: uuid::Uuid::new_v4().hyphenated().to_string(),
            vm_id: vm.vm_id.clone(),
            created_at: utc_rfc3339_seconds(),
        };
        self.snapshots.push(snapshot.clone());
        Ok(snapshot)
    }

    fn set_balloon_ram(&mut self, vm_id: &str, ram_mib: u32) -> Result<Vm, ClientError> {
        if !is_valid_uuid(vm_id) {
            return Err(rpc_err(error_code::INVALID_PARAMS, "invalid vm_id UUID"));
        }
        let vm = self
            .vms
            .get_mut(vm_id)
            .ok_or_else(|| rpc_err(error_code::NOT_FOUND, "VM not found"))?;
        if vm.state != VmState::Running {
            return Err(rpc_err(
                error_code::INVALID_STATE,
                "set_balloon_ram requires a running VM",
            ));
        }
        if ram_mib < MIN_VM_RAM_MIB || ram_mib > vm.ram_mib {
            return Err(rpc_err(
                error_code::INVALID_PARAMS,
                format!(
                    "balloon target {} MiB must be within {}..={} MiB",
                    ram_mib, MIN_VM_RAM_MIB, vm.ram_mib
                ),
            ));
        }
        vm.balloon_target_mib = ram_mib;
        Ok(vm.clone())
    }

    fn set_network_mode(
        &mut self,
        vm_id: &str,
        network_mode: NetworkMode,
    ) -> Result<Vm, ClientError> {
        if !is_valid_uuid(vm_id) {
            return Err(rpc_err(error_code::INVALID_PARAMS, "invalid vm_id UUID"));
        }
        let vm = self
            .vms
            .get_mut(vm_id)
            .ok_or_else(|| rpc_err(error_code::NOT_FOUND, "VM not found"))?;
        if vm.state != VmState::Stopped {
            return Err(rpc_err(
                error_code::INVALID_STATE,
                "set_network_mode requires a stopped VM",
            ));
        }
        vm.network_mode = network_mode;
        Ok(vm.clone())
    }

    fn register_template_image(&mut self, image_id: &str) {
        self.known_images.insert(image_id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_enforces_state_machine_and_admission_control() {
        // 4096 MiB host => 4096 - 768 = 3328 MiB budget.
        let mut mock = MockVmmBackend::empty(4096, 4);

        let vm1 = mock
            .create_vm(CreateVmParams {
                name: "vm-one".to_string(),
                base_image_id: "linux-demo".to_string(),
                vcpu_count: 2,
                ram_mib: 2048,
                network_mode: NetworkMode::Isolated,
            })
            .unwrap();
        assert_eq!(vm1.state, VmState::Stopped);

        // Cannot pause or balloon a stopped VM.
        assert_eq!(
            mock.pause_vm(&vm1.vm_id).unwrap_err(),
            rpc_err(error_code::INVALID_STATE, "VM must be running to pause")
        );
        assert!(matches!(
            mock.set_balloon_ram(&vm1.vm_id, 512),
            Err(ClientError::Rpc(RpcError {
                code: error_code::INVALID_STATE,
                ..
            }))
        ));

        // Start, balloon, then stop and restart resets balloon_target_mib to ram_mib.
        mock.start_vm(&vm1.vm_id).unwrap();
        let b = mock.set_balloon_ram(&vm1.vm_id, 1024).unwrap();
        assert_eq!(b.balloon_target_mib, 1024);

        // Cannot snapshot or change network mode while running.
        assert!(matches!(
            mock.snapshot_vm(&vm1.vm_id),
            Err(ClientError::Rpc(RpcError {
                code: error_code::INVALID_STATE,
                ..
            }))
        ));
        assert!(matches!(
            mock.set_network_mode(&vm1.vm_id, NetworkMode::Nat),
            Err(ClientError::Rpc(RpcError {
                code: error_code::INVALID_STATE,
                ..
            }))
        ));

        // Second VM asking for 1536 MiB while 2048 MiB active exceeds 3328 MiB budget.
        assert!(matches!(
            mock.create_vm(CreateVmParams {
                name: "too-big".to_string(),
                base_image_id: "linux-demo".to_string(),
                vcpu_count: 1,
                ram_mib: 1536,
                network_mode: NetworkMode::Isolated,
            }),
            Err(ClientError::Rpc(RpcError {
                code: error_code::INSUFFICIENT_RESOURCES,
                ..
            }))
        ));

        mock.stop_vm(&vm1.vm_id).unwrap();
        let restarted = mock.start_vm(&vm1.vm_id).unwrap();
        assert_eq!(restarted.balloon_target_mib, 2048);
    }
}
