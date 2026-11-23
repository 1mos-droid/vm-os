//! Core `VmManager` coordinating storage, network sandboxing, `crosvm` supervision,
//! RAM admission control, startup reconciliation, disposable-VM cleanup, snapshot restore,
//! packet capture, runtime assurance monitoring, and SIGTERM shutdown.

use crate::assurance::{self, HyperosSample};
use crate::capture::{CaptureHandle, CAPTURE_LIMIT_BYTES};
use crate::hypervisor::{
    ChildExitReason, CrosvmRunSpec, HypervisorController, DEFAULT_GUEST_KERNEL,
};
use crate::network::{NetworkAllocation, NetworkController};
use crate::rpc::{
    balloon_bytes, error_code, is_valid_image_id, is_valid_uuid, is_valid_vm_name,
    AssuranceMonitor, AssuranceStatusResult, Capture, CaptureState, CreateVmParams, EmptyParams,
    ListCapturesResult, ListSnapshotsResult, ListVmsResult, NetworkMode, RpcError, Snapshot, Vm,
    VmState, HOST_RAM_RESERVE_MIB, MAX_VCPU_COUNT, MAX_VM_RAM_MIB, MIN_VCPU_COUNT, MIN_VM_RAM_MIB,
};
use crate::storage::{DaemonVmPolicy, StorageManager};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};

/// Detected or configured host capacity for admission control.
#[derive(Debug, Clone, Copy)]
pub struct HostResources {
    pub mem_total_mib: u64,
    pub online_cpus: u32,
}

impl HostResources {
    /// Detect live host `MemTotal` from `/proc/meminfo` and online CPUs.
    pub fn detect() -> Self {
        let mem_total_mib = fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| {
                text.lines().find_map(|line| {
                    let mut parts = line.split_whitespace();
                    if parts.next() == Some("MemTotal:") {
                        parts.next()?.parse::<u64>().ok().map(|kib| kib / 1024)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(4096);

        let online_cpus = std::thread::available_parallelism()
            .map(|n| (n.get() as u32).clamp(MIN_VCPU_COUNT, MAX_VCPU_COUNT))
            .unwrap_or(1);

        Self {
            mem_total_mib,
            online_cpus,
        }
    }

    pub fn admission_budget_mib(&self) -> u64 {
        self.mem_total_mib.saturating_sub(HOST_RAM_RESERVE_MIB)
    }
}

struct ManagedVm {
    vm: Vm,
    policy: DaemonVmPolicy,
    network: Option<NetworkAllocation>,
    runtime_dir: Option<PathBuf>,
}

/// Central VM control plane shared across concurrent JSON-RPC connections.
pub struct VmManager {
    storage: StorageManager,
    network: Arc<dyn NetworkController>,
    hypervisor: Arc<dyn HypervisorController>,
    runtime_root: PathBuf,
    kernel_path: PathBuf,
    host: HostResources,
    /// Global serialization for storage allocations and admission accounting.
    storage_lock: Mutex<()>,
    /// Per-VM mutation locks preventing concurrent state races on the same VM.
    vm_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Authoritative in-memory VM registry (always sorted by canonical lowercase `vm_id`).
    vms: RwLock<BTreeMap<String, ManagedVm>>,
    /// Latest capture per VM. A handle may have finished on its own (`limit_reached`).
    captures: Mutex<HashMap<String, CaptureHandle>>,
    /// Requirements the assurance monitor has reported violated. Latched for
    /// the life of the process: the daemon fails closed until restarted.
    violations: std::sync::Mutex<Vec<&'static str>>,
}

impl VmManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        storage: StorageManager,
        network: Arc<dyn NetworkController>,
        hypervisor: Arc<dyn HypervisorController>,
        runtime_root: impl AsRef<Path>,
        kernel_path: impl AsRef<Path>,
        host: HostResources,
        dry_run: bool,
    ) -> Result<Arc<Self>, String> {
        let runtime_root = runtime_root.as_ref().to_path_buf();
        // The runtime root's own mode belongs to the socket server (0700, or 0750 for
        // the control group); per-VM runtime state stays root-only.
        fs::create_dir_all(runtime_root.join("vms"))
            .map_err(|e| format!("failed to create runtime vms dir: {e}"))?;
        let _ = fs::set_permissions(runtime_root.join("vms"), fs::Permissions::from_mode(0o700));

        let reconciled = Self::reconcile_on_startup(&storage, &runtime_root, dry_run)?;
        storage.reconcile_captures()?;

        let mgr = Arc::new(Self {
            storage,
            network,
            hypervisor,
            runtime_root,
            kernel_path: kernel_path.as_ref().to_path_buf(),
            host,
            storage_lock: Mutex::new(()),
            vm_locks: Mutex::new(HashMap::new()),
            vms: RwLock::new(reconciled),
            captures: Mutex::new(HashMap::new()),
            violations: std::sync::Mutex::new(Vec::new()),
        });

        Ok(mgr)
    }

    pub fn default_kernel_path() -> PathBuf {
        PathBuf::from(DEFAULT_GUEST_KERNEL)
    }

    pub fn runtime_vm_dir(&self, vm_id: &str) -> PathBuf {
        self.runtime_root.join("vms").join(vm_id)
    }

    pub fn control_socket_path(&self, vm_id: &str) -> PathBuf {
        self.runtime_vm_dir(vm_id).join("crosvm.sock")
    }

    /// Verify `/proc/<pid>/comm` (or cmdline) belongs to `crosvm` before signaling an orphaned PID
    /// during startup reconciliation so PID reuse across host reboots cannot hit unrelated processes.
    fn process_looks_like_crosvm(pid: u32) -> bool {
        if let Ok(comm) = fs::read_to_string(format!("/proc/{pid}/comm")) {
            if comm.trim() == "crosvm" {
                return true;
            }
        }
        if let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) {
            if let Some(arg0) = cmdline.split(|&b| b == 0).next() {
                if let Ok(s) = std::str::from_utf8(arg0) {
                    return Path::new(s)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n == "crosvm")
                        .unwrap_or(false);
                }
            }
        }
        false
    }

    /// Reconcile persisted VM metadata on daemon startup before accepting client mutations.
    /// Per `ARCHITECTURE.md`: never report a persisted `running`/`paused` state without a live VM.
    fn reconcile_on_startup(
        storage: &StorageManager,
        runtime_root: &Path,
        dry_run: bool,
    ) -> Result<BTreeMap<String, ManagedVm>, String> {
        let persisted = storage.load_all_vms()?;
        let mut map = BTreeMap::new();
        let mut used_cids: std::collections::HashSet<u32> = std::collections::HashSet::new();

        for mut rec in persisted {
            let mut policy_dirty = false;

            // Ensure every VM holds a unique valid vsock CID (>= 3).
            if rec.policy.vsock_cid < 3 || !used_cids.insert(rec.policy.vsock_cid) {
                let fresh_cid = (3u32..=65_535)
                    .find(|cid| !used_cids.contains(cid))
                    .unwrap_or(3);
                used_cids.insert(fresh_cid);
                rec.policy.vsock_cid = fresh_cid;
                policy_dirty = true;
            }

            if matches!(rec.vm.state, VmState::Running | VmState::Paused) {
                if !dry_run {
                    if let Some(pid) = rec.policy.last_pid {
                        // Only signal orphaned processes verified to be crosvm.
                        if Self::process_looks_like_crosvm(pid) {
                            unsafe {
                                libc::kill(pid as libc::pid_t, libc::SIGTERM);
                            }
                        }
                    }
                }
                rec.vm.state = VmState::Stopped;
                rec.policy.last_pid = None;
                policy_dirty = true;
                let rt_dir = runtime_root.join("vms").join(&rec.vm.vm_id);
                if rt_dir.exists() {
                    let _ = fs::remove_dir_all(&rt_dir);
                }
                if rec.vm.disposable {
                    let _ = storage.delete_vm_overlay(&rec.vm.vm_id);
                }
                storage
                    .save_vm_metadata(&rec.vm)
                    .map_err(|e| format!("failed to persist reconciled VM metadata: {e}"))?;
            }

            if policy_dirty {
                storage
                    .save_vm_policy(&rec.vm.vm_id, &rec.policy)
                    .map_err(|e| format!("failed to persist reconciled VM policy: {e}"))?;
            }

            map.insert(
                rec.vm.vm_id.clone(),
                ManagedVm {
                    vm: rec.vm,
                    policy: rec.policy,
                    network: None,
                    runtime_dir: None,
                },
            );
        }

        Ok(map)
    }

    async fn vm_lock(&self, vm_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.vm_locks.lock().await;
        locks
            .entry(vm_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn active_guest_max_ram_mib(&self) -> u64 {
        let map = self.vms.read().await;
        map.values()
            .filter(|m| matches!(m.vm.state, VmState::Running | VmState::Paused))
            .map(|m| u64::from(m.vm.ram_mib))
            .sum()
    }

    fn next_vsock_cid(map: &BTreeMap<String, ManagedVm>) -> u32 {
        // CIDs 0, 1, 2 are reserved by Linux vsock (hypervisor, local, host).
        let used: std::collections::HashSet<u32> =
            map.values().map(|m| m.policy.vsock_cid).collect();
        (3u32..=65_535).find(|cid| !used.contains(cid)).unwrap_or(3)
    }

    pub async fn list_vms(&self) -> Result<ListVmsResult, RpcError> {
        let map = self.vms.read().await;
        let vms = map.values().map(|m| m.vm.clone()).collect();
        Ok(ListVmsResult { vms })
    }

    pub async fn create_vm(&self, params: CreateVmParams) -> Result<Vm, RpcError> {
        self.assurance_gate()?;
        if !is_valid_vm_name(&params.name) {
            return Err(RpcError::new(
                error_code::INVALID_PARAMS,
                "name must be 1-64 Unicode code points without control characters",
            ));
        }
        if !is_valid_image_id(&params.base_image_id) {
            return Err(RpcError::new(
                error_code::INVALID_PARAMS,
                "invalid base_image_id format",
            ));
        }
        if params.vcpu_count < MIN_VCPU_COUNT || params.vcpu_count > MAX_VCPU_COUNT {
            return Err(RpcError::new(
                error_code::INVALID_PARAMS,
                "vcpu_count must be between 1 and 64",
            ));
        }
        if params.vcpu_count > self.host.online_cpus {
            return Err(RpcError::new(
                error_code::INSUFFICIENT_RESOURCES,
                "vcpu_count exceeds online host CPUs",
            ));
        }
        if params.ram_mib < MIN_VM_RAM_MIB || params.ram_mib > MAX_VM_RAM_MIB {
            return Err(RpcError::new(
                error_code::INVALID_PARAMS,
                "ram_mib must be between 128 and 3072 MiB",
            ));
        }

        // Serialize storage allocation and RAM admission globally.
        let _storage_guard = self.storage_lock.lock().await;

        let active_ram = self.active_guest_max_ram_mib().await;
        let budget = self.host.admission_budget_mib();
        if active_ram + u64::from(params.ram_mib) > budget {
            return Err(RpcError::new(
                error_code::INSUFFICIENT_RESOURCES,
                "insufficient host RAM admission capacity",
            ));
        }

        let vm_id = uuid::Uuid::new_v4().hyphenated().to_string();
        let vsock_cid = {
            let map = self.vms.read().await;
            Self::next_vsock_cid(&map)
        };

        let vm = Vm {
            vm_id: vm_id.clone(),
            name: params.name,
            base_image_id: params.base_image_id,
            vcpu_count: params.vcpu_count,
            ram_mib: params.ram_mib,
            balloon_target_mib: params.ram_mib,
            network_mode: params.network_mode,
            state: VmState::Stopped,
            disposable: params.disposable,
        };
        let policy = DaemonVmPolicy {
            balloon_min_ram_mib: MIN_VM_RAM_MIB.min(params.ram_mib),
            balloon_max_ram_mib: params.ram_mib,
            vsock_cid,
            last_pid: None,
        };

        self.storage.provision_vm_instance(&vm, &policy)?;

        let mut map = self.vms.write().await;
        map.insert(
            vm_id,
            ManagedVm {
                vm: vm.clone(),
                policy,
                network: None,
                runtime_dir: None,
            },
        );
        Ok(vm)
    }

    pub async fn start_vm(self: &Arc<Self>, vm_id: &str) -> Result<Vm, RpcError> {
        if !is_valid_uuid(vm_id) {
            return Err(RpcError::new(error_code::INVALID_PARAMS, "invalid vm_id"));
        }
        self.assurance_gate()?;

        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;

        let (current_vm, current_policy) = {
            let map = self.vms.read().await;
            let entry = map
                .get(vm_id)
                .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?;
            (entry.vm.clone(), entry.policy.clone())
        };

        match current_vm.state {
            VmState::Running => Ok(current_vm),
            VmState::Paused => {
                let control_sock = self.control_socket_path(vm_id);
                self.hypervisor.resume_vm(vm_id, &control_sock).await?;

                let mut map = self.vms.write().await;
                let entry = map
                    .get_mut(vm_id)
                    .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?;
                let mut next_vm = entry.vm.clone();
                next_vm.state = VmState::Running;
                if let Err(err) = self.storage.save_vm_metadata(&next_vm) {
                    let _ = self.hypervisor.suspend_vm(vm_id, &control_sock).await;
                    return Err(err);
                }
                entry.vm = next_vm;
                Ok(entry.vm.clone())
            }
            VmState::Stopped | VmState::Failed => {
                let _storage_guard = self.storage_lock.lock().await;
                let active_ram = self.active_guest_max_ram_mib().await;
                if active_ram + u64::from(current_vm.ram_mib) > self.host.admission_budget_mib() {
                    return Err(RpcError::new(
                        error_code::INSUFFICIENT_RESOURCES,
                        "insufficient host RAM admission capacity to start VM",
                    ));
                }

                // Ensure CoW disk exists (re-clones a fresh overlay for a disposable VM).
                let disk_path = self.storage.ensure_vm_overlay(&current_vm)?;

                let rt_dir = self.runtime_vm_dir(vm_id);
                let jail_dir = rt_dir.join("jail");
                fs::create_dir_all(&jail_dir).map_err(|_| {
                    RpcError::new(
                        error_code::IO_FAILURE,
                        "failed to create VM runtime directory",
                    )
                })?;
                let _ = fs::set_permissions(&rt_dir, fs::Permissions::from_mode(0o700));
                let _ = fs::set_permissions(&jail_dir, fs::Permissions::from_mode(0o700));

                let net_alloc = match self.network.setup(vm_id, current_vm.network_mode) {
                    Ok(alloc) => alloc,
                    Err(err) => {
                        let _ = fs::remove_dir_all(&rt_dir);
                        return Err(err);
                    }
                };

                let control_socket = self.control_socket_path(vm_id);
                let spec = CrosvmRunSpec {
                    vm_id: vm_id.to_string(),
                    vcpu_count: current_vm.vcpu_count,
                    ram_mib: current_vm.ram_mib,
                    disk_path,
                    control_socket: control_socket.clone(),
                    jail_pivot_root: jail_dir,
                    vsock_cid: current_policy.vsock_cid,
                    tap_name: net_alloc.as_ref().map(|a| a.tap_name.clone()),
                    shared_dir: None,
                    kernel_path: self.kernel_path.clone(),
                    enable_gpu: true,
                };

                let spawned = match self.hypervisor.spawn_vm(spec).await {
                    Ok(s) => s,
                    Err(err) => {
                        if let Some(ref alloc) = net_alloc {
                            self.network.teardown(alloc);
                        }
                        let _ = fs::remove_dir_all(&rt_dir);
                        return Err(err);
                    }
                };

                let commit_res = {
                    let mut map = self.vms.write().await;
                    let entry = map
                        .get_mut(vm_id)
                        .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?;
                    let mut next_vm = entry.vm.clone();
                    next_vm.state = VmState::Running;
                    // Per ARCHITECTURE.md: Balloon target resets to maximum at each cold start.
                    next_vm.balloon_target_mib = next_vm.ram_mib;
                    let mut next_policy = entry.policy.clone();
                    next_policy.last_pid = spawned.pid;

                    match self
                        .storage
                        .save_vm_metadata(&next_vm)
                        .and_then(|_| self.storage.save_vm_policy(vm_id, &next_policy))
                    {
                        Ok(()) => {
                            entry.vm = next_vm.clone();
                            entry.policy = next_policy;
                            entry.network = net_alloc.clone();
                            entry.runtime_dir = Some(rt_dir.clone());
                            Ok(next_vm)
                        }
                        Err(err) => Err(err),
                    }
                };

                let updated_vm = match commit_res {
                    Ok(vm) => vm,
                    Err(err) => {
                        let _ = self.hypervisor.stop_vm(vm_id, &control_socket).await;
                        if let Some(ref alloc) = net_alloc {
                            self.network.teardown(alloc);
                        }
                        let _ = fs::remove_dir_all(&rt_dir);
                        return Err(err);
                    }
                };

                // Spawn async child-exit supervisor to detect unexpected crosvm crashes.
                let mgr_weak = Arc::downgrade(self);
                let watched_id = vm_id.to_string();
                tokio::spawn(async move {
                    if let Ok(ChildExitReason::UnexpectedCrash) = spawned.exit_rx.await {
                        if let Some(mgr) = mgr_weak.upgrade() {
                            mgr.handle_unexpected_exit(&watched_id).await;
                        }
                    }
                });

                Ok(updated_vm)
            }
        }
    }

    async fn handle_unexpected_exit(&self, vm_id: &str) {
        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;
        self.end_capture(vm_id).await;

        let mut map = self.vms.write().await;
        let Some(entry) = map.get_mut(vm_id) else {
            return;
        };
        if entry.vm.state == VmState::Stopped {
            return;
        }

        if let Some(alloc) = entry.network.take() {
            self.network.teardown(&alloc);
        }
        if let Some(rt_dir) = entry.runtime_dir.take() {
            let _ = fs::remove_dir_all(&rt_dir);
        }
        if entry.vm.disposable {
            let _ = self.storage.delete_vm_overlay(vm_id);
        }
        entry.vm.state = VmState::Failed;
        entry.policy.last_pid = None;
        let _ = self.storage.save_vm_metadata(&entry.vm);
        let _ = self.storage.save_vm_policy(vm_id, &entry.policy);
    }

    pub async fn pause_vm(&self, vm_id: &str) -> Result<Vm, RpcError> {
        if !is_valid_uuid(vm_id) {
            return Err(RpcError::new(error_code::INVALID_PARAMS, "invalid vm_id"));
        }

        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;

        let current_vm = {
            let map = self.vms.read().await;
            map.get(vm_id)
                .map(|e| e.vm.clone())
                .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?
        };

        match current_vm.state {
            VmState::Paused => Ok(current_vm),
            VmState::Running => {
                let control_sock = self.control_socket_path(vm_id);
                self.hypervisor.suspend_vm(vm_id, &control_sock).await?;

                let mut map = self.vms.write().await;
                let entry = map
                    .get_mut(vm_id)
                    .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?;
                let mut next_vm = entry.vm.clone();
                next_vm.state = VmState::Paused;
                if let Err(err) = self.storage.save_vm_metadata(&next_vm) {
                    let _ = self.hypervisor.resume_vm(vm_id, &control_sock).await;
                    return Err(err);
                }
                entry.vm = next_vm;
                Ok(entry.vm.clone())
            }
            VmState::Stopped | VmState::Failed => Err(RpcError::new(
                error_code::INVALID_STATE,
                "pause_vm requires a running VM",
            )),
        }
    }

    pub async fn stop_vm(&self, vm_id: &str) -> Result<Vm, RpcError> {
        if !is_valid_uuid(vm_id) {
            return Err(RpcError::new(error_code::INVALID_PARAMS, "invalid vm_id"));
        }

        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;

        let current_state = {
            let map = self.vms.read().await;
            map.get(vm_id)
                .map(|e| e.vm.state)
                .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?
        };

        if matches!(current_state, VmState::Running | VmState::Paused) {
            let control_sock = self.control_socket_path(vm_id);
            self.hypervisor.stop_vm(vm_id, &control_sock).await?;
        }
        self.end_capture(vm_id).await;

        let mut map = self.vms.write().await;
        let entry = map
            .get_mut(vm_id)
            .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?;

        if let Some(alloc) = entry.network.take() {
            self.network.teardown(&alloc);
        }
        if let Some(rt_dir) = entry.runtime_dir.take() {
            let _ = fs::remove_dir_all(&rt_dir);
        } else {
            let rt_dir = self.runtime_vm_dir(vm_id);
            if rt_dir.exists() {
                let _ = fs::remove_dir_all(&rt_dir);
            }
        }

        // A disposable VM's writable disk is discarded on every stop.
        if entry.vm.disposable {
            self.storage.delete_vm_overlay(vm_id)?;
        }

        entry.vm.state = VmState::Stopped;
        entry.policy.last_pid = None;
        self.storage.save_vm_metadata(&entry.vm)?;
        self.storage.save_vm_policy(vm_id, &entry.policy)?;
        Ok(entry.vm.clone())
    }

    pub async fn snapshot_vm(&self, vm_id: &str) -> Result<Snapshot, RpcError> {
        if !is_valid_uuid(vm_id) {
            return Err(RpcError::new(error_code::INVALID_PARAMS, "invalid vm_id"));
        }

        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;
        let _storage_guard = self.storage_lock.lock().await;

        let current_vm = {
            let map = self.vms.read().await;
            map.get(vm_id)
                .map(|e| e.vm.clone())
                .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?
        };

        if current_vm.state != VmState::Stopped {
            return Err(RpcError::new(
                error_code::INVALID_STATE,
                "snapshot_vm is allowed only while the VM is stopped",
            ));
        }
        if current_vm.disposable {
            return Err(RpcError::new(
                error_code::INVALID_STATE,
                "disposable VMs cannot be snapshotted",
            ));
        }

        let snapshot = Snapshot {
            snapshot_id: uuid::Uuid::new_v4().hyphenated().to_string(),
            vm_id: current_vm.vm_id.clone(),
            created_at: utc_rfc3339_seconds(),
        };
        self.storage.create_cold_snapshot(&current_vm, &snapshot)?;
        Ok(snapshot)
    }

    pub async fn set_balloon_ram(&self, vm_id: &str, ram_mib: u32) -> Result<Vm, RpcError> {
        if !is_valid_uuid(vm_id) {
            return Err(RpcError::new(error_code::INVALID_PARAMS, "invalid vm_id"));
        }

        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;

        let current_vm = {
            let map = self.vms.read().await;
            map.get(vm_id)
                .map(|e| e.vm.clone())
                .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?
        };

        if current_vm.state != VmState::Running {
            return Err(RpcError::new(
                error_code::INVALID_STATE,
                "set_balloon_ram requires a running VM",
            ));
        }

        let target_balloon_bytes = balloon_bytes(current_vm.ram_mib, ram_mib)?;
        let control_sock = self.control_socket_path(vm_id);
        self.hypervisor
            .set_balloon(vm_id, &control_sock, target_balloon_bytes)
            .await?;

        let mut map = self.vms.write().await;
        let entry = map
            .get_mut(vm_id)
            .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?;
        let mut next_vm = entry.vm.clone();
        next_vm.balloon_target_mib = ram_mib;
        self.storage.save_vm_metadata(&next_vm)?;
        entry.vm = next_vm;
        Ok(entry.vm.clone())
    }

    pub async fn set_network_mode(
        &self,
        vm_id: &str,
        network_mode: NetworkMode,
    ) -> Result<Vm, RpcError> {
        if !is_valid_uuid(vm_id) {
            return Err(RpcError::new(error_code::INVALID_PARAMS, "invalid vm_id"));
        }

        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;

        let mut map = self.vms.write().await;
        let entry = map
            .get_mut(vm_id)
            .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?;

        if entry.vm.state != VmState::Stopped {
            return Err(RpcError::new(
                error_code::INVALID_STATE,
                "set_network_mode requires a stopped VM",
            ));
        }

        let mut next_vm = entry.vm.clone();
        next_vm.network_mode = network_mode;
        self.storage.save_vm_metadata(&next_vm)?;
        entry.vm = next_vm;
        Ok(entry.vm.clone())
    }

    async fn existing_vm(&self, vm_id: &str) -> Result<Vm, RpcError> {
        if !is_valid_uuid(vm_id) {
            return Err(RpcError::new(error_code::INVALID_PARAMS, "invalid vm_id"));
        }
        let map = self.vms.read().await;
        map.get(vm_id)
            .map(|e| e.vm.clone())
            .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))
    }

    fn require_uuid(id: &str, what: &str) -> Result<(), RpcError> {
        if is_valid_uuid(id) {
            Ok(())
        } else {
            Err(RpcError::new(
                error_code::INVALID_PARAMS,
                format!("invalid {what}"),
            ))
        }
    }

    pub async fn list_snapshots(&self, vm_id: &str) -> Result<ListSnapshotsResult, RpcError> {
        self.existing_vm(vm_id).await?;
        let snapshots = self.storage.list_snapshots(vm_id)?;
        Ok(ListSnapshotsResult { snapshots })
    }

    pub async fn restore_snapshot(&self, vm_id: &str, snapshot_id: &str) -> Result<Vm, RpcError> {
        Self::require_uuid(snapshot_id, "snapshot_id")?;
        self.existing_vm(vm_id).await?;
        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;
        let _storage_guard = self.storage_lock.lock().await;

        let vm = self.existing_vm(vm_id).await?;
        if vm.state != VmState::Stopped {
            return Err(RpcError::new(
                error_code::INVALID_STATE,
                "restore_snapshot requires a stopped VM",
            ));
        }
        if vm.disposable {
            return Err(RpcError::new(
                error_code::INVALID_STATE,
                "disposable VMs cannot be restored from a snapshot",
            ));
        }
        self.storage.restore_snapshot(vm_id, snapshot_id)?;
        Ok(vm)
    }

    pub async fn delete_snapshot(
        &self,
        vm_id: &str,
        snapshot_id: &str,
    ) -> Result<EmptyParams, RpcError> {
        Self::require_uuid(snapshot_id, "snapshot_id")?;
        self.existing_vm(vm_id).await?;
        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;
        let _storage_guard = self.storage_lock.lock().await;
        self.storage.delete_snapshot(vm_id, snapshot_id)?;
        Ok(EmptyParams {})
    }

    pub async fn start_capture(&self, vm_id: &str) -> Result<Capture, RpcError> {
        self.existing_vm(vm_id).await?;
        self.assurance_gate()?;
        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;

        let (vm, alloc) = {
            let map = self.vms.read().await;
            let entry = map
                .get(vm_id)
                .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "VM not found"))?;
            (entry.vm.clone(), entry.network.clone())
        };
        if vm.network_mode == NetworkMode::Isolated {
            return Err(RpcError::new(
                error_code::UNAVAILABLE_CAPABILITY,
                "isolated VMs have no network interface to capture",
            ));
        }
        if vm.state != VmState::Running {
            return Err(RpcError::new(
                error_code::INVALID_STATE,
                "start_capture requires a running VM",
            ));
        }
        let alloc = alloc.ok_or_else(|| {
            RpcError::new(
                error_code::UNAVAILABLE_CAPABILITY,
                "VM network interface is not available",
            )
        })?;

        let mut captures = self.captures.lock().await;
        if captures.get(vm_id).is_some_and(|h| h.is_running()) {
            return Err(RpcError::new(
                error_code::INVALID_STATE,
                "a capture is already running for this VM",
            ));
        }

        let source = self.network.open_capture(&alloc)?;
        let capture = Capture {
            capture_id: uuid::Uuid::new_v4().hyphenated().to_string(),
            vm_id: vm_id.to_string(),
            started_at: utc_rfc3339_seconds(),
            state: CaptureState::Running,
            bytes: 0,
        };
        let file = self
            .storage
            .create_capture_file(vm_id, &capture.capture_id)?;
        if let Err(err) = self.storage.save_capture(&capture) {
            let _ = fs::remove_file(self.storage.capture_pcap_path(vm_id, &capture.capture_id));
            return Err(err);
        }

        let storage = self.storage.clone();
        let handle = CaptureHandle::spawn(
            source,
            file,
            capture,
            CAPTURE_LIMIT_BYTES,
            Box::new(move |done| {
                if let Err(err) = storage.save_capture(done) {
                    eprintln!("vmm-daemon: failed to persist capture: {err}");
                }
            }),
        );
        let started = handle.snapshot();
        // Replacing a finished (`limit_reached`) handle is fine: it already persisted.
        captures.insert(vm_id.to_string(), handle);
        Ok(started)
    }

    pub async fn stop_capture(&self, vm_id: &str) -> Result<Capture, RpcError> {
        self.existing_vm(vm_id).await?;
        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;
        if let Some(done) = self.end_capture(vm_id).await {
            return Ok(done);
        }
        // Idempotent: report the most recent capture if none is running.
        self.storage
            .list_captures(vm_id)?
            .pop()
            .ok_or_else(|| RpcError::new(error_code::NOT_FOUND, "no capture exists for this VM"))
    }

    pub async fn list_captures(&self, vm_id: &str) -> Result<ListCapturesResult, RpcError> {
        self.existing_vm(vm_id).await?;
        let mut captures = self.storage.list_captures(vm_id)?;
        if let Some(handle) = self.captures.lock().await.get(vm_id) {
            let live = handle.snapshot();
            if let Some(c) = captures
                .iter_mut()
                .find(|c| c.capture_id == live.capture_id)
            {
                *c = live;
            }
        }
        Ok(ListCapturesResult { captures })
    }

    pub async fn delete_capture(
        &self,
        vm_id: &str,
        capture_id: &str,
    ) -> Result<EmptyParams, RpcError> {
        Self::require_uuid(capture_id, "capture_id")?;
        self.existing_vm(vm_id).await?;
        let lock = self.vm_lock(vm_id).await;
        let _guard = lock.lock().await;
        let mut captures = self.captures.lock().await;
        if let Some(handle) = captures.get(vm_id) {
            if handle.snapshot().capture_id == capture_id {
                if handle.is_running() {
                    return Err(RpcError::new(
                        error_code::INVALID_STATE,
                        "stop the capture before deleting it",
                    ));
                }
                captures.remove(vm_id);
            }
        }
        self.storage.delete_capture(vm_id, capture_id)?;
        Ok(EmptyParams {})
    }

    /// Stop a VM's capture, if any, waiting for its final flush. Called with the VM
    /// lock held, before the TAP is torn down.
    async fn end_capture(&self, vm_id: &str) -> Option<Capture> {
        let handle = self.captures.lock().await.remove(vm_id)?;
        tokio::task::spawn_blocking(move || handle.stop())
            .await
            .ok()
    }

    /// Refuse new work once any host invariant has been violated.
    fn assurance_gate(&self) -> Result<(), RpcError> {
        let violated = self.violations.lock().unwrap_or_else(|p| p.into_inner());
        if violated.is_empty() {
            return Ok(());
        }
        Err(RpcError::new(
            error_code::INVARIANT_VIOLATED,
            format!(
                "host invariant {} was violated; investigate, then restart vmm-daemon",
                violated.join(", ")
            ),
        ))
    }

    /// Observe the host, feed the runtime monitor, and respond to violations:
    /// NAT Spaces whose firewall table changed (HYP001) are stopped, and every
    /// violation blocks new Spaces and captures until the daemon restarts.
    pub async fn check_invariants(&self) {
        let mut observation = HyperosSample {
            ram_budget_mib: self.host.admission_budget_mib() as i64,
            reserved_ram_mib: self.active_guest_max_ram_mib().await as i64,
            ..Default::default()
        };
        let mut unprotected = Vec::new();
        {
            let map = self.vms.read().await;
            for entry in map.values() {
                if !matches!(entry.vm.state, VmState::Running | VmState::Paused)
                    || entry.vm.network_mode != NetworkMode::Nat
                {
                    continue;
                }
                observation.nat_spaces_running += 1;
                let intact = entry
                    .network
                    .as_ref()
                    .is_some_and(|alloc| self.network.isolation_intact(alloc));
                if intact {
                    observation.nat_spaces_lan_blocked += 1;
                } else {
                    unprotected.push(entry.vm.vm_id.clone());
                }
            }
        }
        observation.largest_capture_bytes = self
            .captures
            .lock()
            .await
            .values()
            .map(|h| h.snapshot().bytes as i64)
            .max()
            .unwrap_or(0);

        let violated = assurance::sample(&observation);
        if violated.is_empty() {
            return;
        }
        {
            let mut latched = self.violations.lock().unwrap_or_else(|p| p.into_inner());
            for req in &violated {
                if !latched.contains(&req.id) {
                    eprintln!(
                        "vmm-daemon: ASSURANCE VIOLATION {}: \"{}\" ({observation:?})",
                        req.id, req.fretish
                    );
                    latched.push(req.id);
                }
            }
        }
        if violated.iter().any(|r| r.id == "HYP001") {
            for vm_id in unprotected {
                if let Err(err) = self.stop_vm(&vm_id).await {
                    eprintln!("vmm-daemon: failed to stop unprotected Space {vm_id}: {err}");
                }
            }
        }
    }

    /// State of every runtime monitor.
    pub fn assurance_status(&self) -> AssuranceStatusResult {
        let latched = self.violations.lock().unwrap_or_else(|p| p.into_inner());
        AssuranceStatusResult {
            monitors: assurance::REQUIREMENTS
                .iter()
                .map(|r| AssuranceMonitor {
                    id: r.id.to_string(),
                    requirement: r.fretish.to_string(),
                    state: if latched.contains(&r.id) {
                        "violated".to_string()
                    } else {
                        "ok".to_string()
                    },
                })
                .collect(),
        }
    }

    /// Gracefully stop all active VMs and persist metadata before daemon exit (`SIGTERM`).
    pub async fn shutdown_all(&self) {
        let active_ids: Vec<String> = {
            let map = self.vms.read().await;
            map.values()
                .filter(|m| matches!(m.vm.state, VmState::Running | VmState::Paused))
                .map(|m| m.vm.vm_id.clone())
                .collect()
        };

        for vm_id in active_ids {
            let _ = self.stop_vm(&vm_id).await;
        }
    }
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
