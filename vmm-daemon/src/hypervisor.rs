//! Native `crosvm` process supervisor and control-socket controller + `--dry-run` mock backend.

use crate::rpc::{error_code, RpcError};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::oneshot;

/// Default `crosvm` binary path on HyperOS (`ARCHITECTURE.md`).
pub const DEFAULT_CROSVM_BIN: &str = "/usr/bin/crosvm";

/// Default guest kernel path on HyperOS.
pub const DEFAULT_GUEST_KERNEL: &str = "/boot/bzImage";

/// Reason a supervised `crosvm` process exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExitReason {
    StoppedByDaemon,
    UnexpectedCrash,
}

/// Complete launch specification for `crosvm run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrosvmRunSpec {
    pub vm_id: String,
    pub vcpu_count: u32,
    pub ram_mib: u32,
    pub disk_path: PathBuf,
    pub control_socket: PathBuf,
    pub jail_pivot_root: PathBuf,
    pub vsock_cid: u32,
    pub tap_name: Option<String>,
    pub shared_dir: Option<PathBuf>,
    pub kernel_path: PathBuf,
    pub enable_gpu: bool,
}

impl CrosvmRunSpec {
    /// Generate exact `crosvm run` CLI arguments (no `--disable-sandbox`, preserving
    /// `crosvm`'s jail/seccomp isolation as required by `ARCHITECTURE.md`).
    pub fn to_crosvm_args(&self) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            "--cpus".to_string(),
            self.vcpu_count.to_string(),
            "--mem".to_string(),
            self.ram_mib.to_string(),
            "--block".to_string(),
            format!("path={},root", self.disk_path.display()),
            "--balloon".to_string(),
            "--vsock".to_string(),
            format!("cid={}", self.vsock_cid),
            "--socket".to_string(),
            self.control_socket.display().to_string(),
            "--jail-config".to_string(),
            format!("pivot-root={}", self.jail_pivot_root.display()),
        ];

        if self.enable_gpu {
            args.push("--gpu".to_string());
            args.push("backend=2d".to_string());
        }

        if let Some(ref tap) = self.tap_name {
            args.push("--net".to_string());
            args.push(format!("tap-name={tap}"));
        }

        if let Some(ref shared) = self.shared_dir {
            args.push("--shared-dir".to_string());
            args.push(format!("{}:hyperos-share:type=fs", shared.display()));
        }

        args.push("--params".to_string());
        args.push("root=/dev/vda1 rw console=ttyS0,115200".to_string());
        args.push(self.kernel_path.display().to_string());
        args
    }
}

/// Build `crosvm suspend <control_socket>` arguments.
pub fn crosvm_suspend_args(control_socket: &Path) -> Vec<String> {
    vec!["suspend".to_string(), control_socket.display().to_string()]
}

/// Build `crosvm resume <control_socket>` arguments.
pub fn crosvm_resume_args(control_socket: &Path) -> Vec<String> {
    vec!["resume".to_string(), control_socket.display().to_string()]
}

/// Build `crosvm balloon <balloon_bytes> <control_socket>` arguments.
pub fn crosvm_balloon_args(balloon_bytes: u64, control_socket: &Path) -> Vec<String> {
    vec![
        "balloon".to_string(),
        balloon_bytes.to_string(),
        control_socket.display().to_string(),
    ]
}

/// Build `crosvm snapshot take <snapshot_path> <control_socket>` arguments.
pub fn crosvm_snapshot_args(snapshot_path: &Path, control_socket: &Path) -> Vec<String> {
    vec![
        "snapshot".to_string(),
        "take".to_string(),
        snapshot_path.display().to_string(),
        control_socket.display().to_string(),
    ]
}

/// Build `crosvm stop <control_socket>` arguments.
pub fn crosvm_stop_args(control_socket: &Path) -> Vec<String> {
    vec!["stop".to_string(), control_socket.display().to_string()]
}

/// Handle returned when a VM process is spawned.
pub struct SpawnedVm {
    pub pid: Option<u32>,
    pub exit_rx: oneshot::Receiver<ChildExitReason>,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Async hypervisor abstraction implemented by both `CrosvmController` and `MockHypervisor`.
pub trait HypervisorController: Send + Sync {
    fn spawn_vm(&self, spec: CrosvmRunSpec) -> BoxFuture<'_, Result<SpawnedVm, RpcError>>;
    fn suspend_vm(&self, vm_id: &str, control_socket: &Path)
        -> BoxFuture<'_, Result<(), RpcError>>;
    fn resume_vm(&self, vm_id: &str, control_socket: &Path) -> BoxFuture<'_, Result<(), RpcError>>;
    fn set_balloon(
        &self,
        vm_id: &str,
        control_socket: &Path,
        balloon_bytes: u64,
    ) -> BoxFuture<'_, Result<(), RpcError>>;
    fn snapshot_runtime(
        &self,
        vm_id: &str,
        control_socket: &Path,
        snapshot_path: &Path,
    ) -> BoxFuture<'_, Result<(), RpcError>>;
    fn stop_vm(&self, vm_id: &str, control_socket: &Path) -> BoxFuture<'_, Result<(), RpcError>>;
}

/// Real `crosvm` hypervisor controller requiring `/dev/kvm` and `/usr/bin/crosvm`.
pub struct CrosvmController {
    crosvm_bin: PathBuf,
    children: std::sync::Arc<Mutex<HashMap<String, ChildTracker>>>,
}

struct ChildTracker {
    pid: Option<u32>,
    stopping: bool,
}

impl CrosvmController {
    pub fn new(crosvm_bin: impl Into<PathBuf>) -> Self {
        Self {
            crosvm_bin: crosvm_bin.into(),
            children: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn default_hyperos() -> Self {
        Self::new(DEFAULT_CROSVM_BIN)
    }

    fn verify_kvm_and_binary(&self) -> Result<(), RpcError> {
        if !Path::new("/dev/kvm").exists() {
            return Err(RpcError::new(
                error_code::UNAVAILABLE_CAPABILITY,
                "/dev/kvm unavailable; software emulation is disabled",
            ));
        }
        if !self.crosvm_bin.is_file() {
            return Err(RpcError::new(
                error_code::UNAVAILABLE_CAPABILITY,
                "crosvm hypervisor binary unavailable",
            ));
        }
        Ok(())
    }

    async fn run_control_command(
        &self,
        args: Vec<String>,
        capability_err: &str,
    ) -> Result<(), RpcError> {
        let output = Command::new(&self.crosvm_bin)
            .args(&args)
            .output()
            .await
            .map_err(|_| RpcError::new(error_code::UNAVAILABLE_CAPABILITY, capability_err))?;
        if !output.status.success() {
            return Err(RpcError::new(error_code::IO_FAILURE, capability_err));
        }
        Ok(())
    }
}

impl HypervisorController for CrosvmController {
    fn spawn_vm(&self, spec: CrosvmRunSpec) -> BoxFuture<'_, Result<SpawnedVm, RpcError>> {
        Box::pin(async move {
            self.verify_kvm_and_binary()?;
            if !spec.kernel_path.is_file() {
                return Err(RpcError::new(
                    error_code::UNAVAILABLE_CAPABILITY,
                    "guest kernel image unavailable",
                ));
            }
            std::fs::create_dir_all(&spec.jail_pivot_root).map_err(|_| {
                RpcError::new(
                    error_code::IO_FAILURE,
                    "failed to prepare crosvm jail directory",
                )
            })?;

            let args = spec.to_crosvm_args();
            let mut child = Command::new(&self.crosvm_bin)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|_| {
                    RpcError::new(error_code::IO_FAILURE, "failed to launch crosvm process")
                })?;

            let pid = child.id();
            let vm_id = spec.vm_id.clone();
            {
                let mut guard = self.children.lock().unwrap();
                guard.insert(
                    vm_id.clone(),
                    ChildTracker {
                        pid,
                        stopping: false,
                    },
                );
            }

            let (exit_tx, exit_rx) = oneshot::channel();
            let children = self.children.clone();
            tokio::spawn(async move {
                let _ = child.wait().await;
                let was_stopping = {
                    let mut guard = children.lock().unwrap();
                    guard.remove(&vm_id).map(|t| t.stopping).unwrap_or(false)
                };
                let reason = if was_stopping {
                    ChildExitReason::StoppedByDaemon
                } else {
                    ChildExitReason::UnexpectedCrash
                };
                let _ = exit_tx.send(reason);
            });

            Ok(SpawnedVm { pid, exit_rx })
        })
    }

    fn suspend_vm(
        &self,
        _vm_id: &str,
        control_socket: &Path,
    ) -> BoxFuture<'_, Result<(), RpcError>> {
        let args = crosvm_suspend_args(control_socket);
        Box::pin(async move {
            self.run_control_command(args, "crosvm suspend failed")
                .await
        })
    }

    fn resume_vm(
        &self,
        _vm_id: &str,
        control_socket: &Path,
    ) -> BoxFuture<'_, Result<(), RpcError>> {
        let args = crosvm_resume_args(control_socket);
        Box::pin(async move { self.run_control_command(args, "crosvm resume failed").await })
    }

    fn set_balloon(
        &self,
        _vm_id: &str,
        control_socket: &Path,
        balloon_bytes: u64,
    ) -> BoxFuture<'_, Result<(), RpcError>> {
        let args = crosvm_balloon_args(balloon_bytes, control_socket);
        Box::pin(async move {
            self.run_control_command(args, "crosvm balloon adjustment failed")
                .await
        })
    }

    fn snapshot_runtime(
        &self,
        _vm_id: &str,
        control_socket: &Path,
        snapshot_path: &Path,
    ) -> BoxFuture<'_, Result<(), RpcError>> {
        let args = crosvm_snapshot_args(snapshot_path, control_socket);
        Box::pin(async move {
            self.run_control_command(args, "crosvm snapshot failed")
                .await
        })
    }

    fn stop_vm(&self, vm_id: &str, control_socket: &Path) -> BoxFuture<'_, Result<(), RpcError>> {
        let vm_id = vm_id.to_string();
        let control_socket = control_socket.to_path_buf();
        Box::pin(async move {
            let pid = {
                let mut guard = self.children.lock().unwrap();
                if let Some(tracker) = guard.get_mut(&vm_id) {
                    tracker.stopping = true;
                    tracker.pid
                } else {
                    None
                }
            };

            if control_socket.exists() {
                let _ = self
                    .run_control_command(crosvm_stop_args(&control_socket), "crosvm stop")
                    .await;
            }

            if let Some(pid) = pid {
                // Wait briefly for graceful crosvm stop, then enforce SIGTERM / SIGKILL.
                for _ in 0..10 {
                    let still_tracked = self.children.lock().unwrap().contains_key(&vm_id);
                    if !still_tracked {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
                for _ in 0..10 {
                    let still_tracked = self.children.lock().unwrap().contains_key(&vm_id);
                    if !still_tracked {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
                for _ in 0..10 {
                    let still_tracked = self.children.lock().unwrap().contains_key(&vm_id);
                    if !still_tracked {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            Ok(())
        })
    }
}

/// Recorded control action in `MockHypervisor` for test assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockHypervisorEvent {
    Spawn {
        vm_id: String,
        args: Vec<String>,
    },
    Suspend {
        vm_id: String,
        args: Vec<String>,
    },
    Resume {
        vm_id: String,
        args: Vec<String>,
    },
    Balloon {
        vm_id: String,
        balloon_bytes: u64,
        args: Vec<String>,
    },
    Snapshot {
        vm_id: String,
        args: Vec<String>,
    },
    Stop {
        vm_id: String,
        args: Vec<String>,
    },
}

/// `--dry-run` / CI mock hypervisor recording every `crosvm` invocation and supporting crash injection.
#[derive(Default)]
pub struct MockHypervisor {
    events: Mutex<Vec<MockHypervisorEvent>>,
    running: Mutex<HashMap<String, oneshot::Sender<ChildExitReason>>>,
    next_pid: Mutex<u32>,
}

impl MockHypervisor {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            running: Mutex::new(HashMap::new()),
            next_pid: Mutex::new(40_000),
        }
    }

    pub fn events(&self) -> Vec<MockHypervisorEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn is_running(&self, vm_id: &str) -> bool {
        self.running.lock().unwrap().contains_key(vm_id)
    }

    /// Simulate an unexpected `crosvm` process crash for testing supervision and ephemeral cleanup.
    pub fn simulate_crash(&self, vm_id: &str) -> bool {
        let tx = self.running.lock().unwrap().remove(vm_id);
        if let Some(tx) = tx {
            let _ = tx.send(ChildExitReason::UnexpectedCrash);
            true
        } else {
            false
        }
    }
}

impl HypervisorController for MockHypervisor {
    fn spawn_vm(&self, spec: CrosvmRunSpec) -> BoxFuture<'_, Result<SpawnedVm, RpcError>> {
        Box::pin(async move {
            let args = spec.to_crosvm_args();
            let pid = {
                let mut p = self.next_pid.lock().unwrap();
                *p += 1;
                *p
            };
            let (tx, rx) = oneshot::channel();
            self.running.lock().unwrap().insert(spec.vm_id.clone(), tx);
            self.events
                .lock()
                .unwrap()
                .push(MockHypervisorEvent::Spawn {
                    vm_id: spec.vm_id,
                    args,
                });
            Ok(SpawnedVm {
                pid: Some(pid),
                exit_rx: rx,
            })
        })
    }

    fn suspend_vm(
        &self,
        vm_id: &str,
        control_socket: &Path,
    ) -> BoxFuture<'_, Result<(), RpcError>> {
        let vm_id = vm_id.to_string();
        let args = crosvm_suspend_args(control_socket);
        Box::pin(async move {
            if !self.running.lock().unwrap().contains_key(&vm_id) {
                return Err(RpcError::new(
                    error_code::INVALID_STATE,
                    "crosvm process is not running",
                ));
            }
            self.events
                .lock()
                .unwrap()
                .push(MockHypervisorEvent::Suspend { vm_id, args });
            Ok(())
        })
    }

    fn resume_vm(&self, vm_id: &str, control_socket: &Path) -> BoxFuture<'_, Result<(), RpcError>> {
        let vm_id = vm_id.to_string();
        let args = crosvm_resume_args(control_socket);
        Box::pin(async move {
            if !self.running.lock().unwrap().contains_key(&vm_id) {
                return Err(RpcError::new(
                    error_code::INVALID_STATE,
                    "crosvm process is not running",
                ));
            }
            self.events
                .lock()
                .unwrap()
                .push(MockHypervisorEvent::Resume { vm_id, args });
            Ok(())
        })
    }

    fn set_balloon(
        &self,
        vm_id: &str,
        control_socket: &Path,
        balloon_bytes: u64,
    ) -> BoxFuture<'_, Result<(), RpcError>> {
        let vm_id = vm_id.to_string();
        let args = crosvm_balloon_args(balloon_bytes, control_socket);
        Box::pin(async move {
            if !self.running.lock().unwrap().contains_key(&vm_id) {
                return Err(RpcError::new(
                    error_code::INVALID_STATE,
                    "crosvm process is not running",
                ));
            }
            self.events
                .lock()
                .unwrap()
                .push(MockHypervisorEvent::Balloon {
                    vm_id,
                    balloon_bytes,
                    args,
                });
            Ok(())
        })
    }

    fn snapshot_runtime(
        &self,
        vm_id: &str,
        control_socket: &Path,
        snapshot_path: &Path,
    ) -> BoxFuture<'_, Result<(), RpcError>> {
        let vm_id = vm_id.to_string();
        let args = crosvm_snapshot_args(snapshot_path, control_socket);
        Box::pin(async move {
            self.events
                .lock()
                .unwrap()
                .push(MockHypervisorEvent::Snapshot { vm_id, args });
            Ok(())
        })
    }

    fn stop_vm(&self, vm_id: &str, control_socket: &Path) -> BoxFuture<'_, Result<(), RpcError>> {
        let vm_id = vm_id.to_string();
        let args = crosvm_stop_args(control_socket);
        Box::pin(async move {
            if let Some(tx) = self.running.lock().unwrap().remove(&vm_id) {
                let _ = tx.send(ChildExitReason::StoppedByDaemon);
            }
            self.events
                .lock()
                .unwrap()
                .push(MockHypervisorEvent::Stop { vm_id, args });
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_expected_crosvm_cli_flags_for_isolated_and_networked_vms() {
        let spec_iso = CrosvmRunSpec {
            vm_id: "b6f51eed-8dc7-47f6-a8e7-81b6ae783a21".to_string(),
            vcpu_count: 2,
            ram_mib: 1024,
            disk_path: PathBuf::from("/var/lib/hyperos/instances/b6f51eed/current/disk.raw"),
            control_socket: PathBuf::from("/run/hyperos/vms/b6f51eed/crosvm.sock"),
            jail_pivot_root: PathBuf::from("/run/hyperos/vms/b6f51eed/jail"),
            vsock_cid: 3,
            tap_name: None,
            shared_dir: None,
            kernel_path: PathBuf::from("/boot/bzImage"),
            enable_gpu: true,
        };
        let args_iso = spec_iso.to_crosvm_args();
        assert_eq!(args_iso[0], "run");
        assert!(args_iso.contains(&"--balloon".to_string()));
        assert!(args_iso.contains(&"--vsock".to_string()));
        assert!(args_iso.contains(&"cid=3".to_string()));
        assert!(args_iso.contains(&"--gpu".to_string()));
        assert!(args_iso.contains(&"backend=2d".to_string()));
        assert!(!args_iso.contains(&"--net".to_string()));
        assert!(!args_iso.contains(&"--disable-sandbox".to_string()));

        let mut spec_net = spec_iso;
        spec_net.tap_name = Some("hvmtapb6f51eed".to_string());
        let args_net = spec_net.to_crosvm_args();
        assert!(args_net.contains(&"--net".to_string()));
        assert!(args_net.contains(&"tap-name=hvmtapb6f51eed".to_string()));

        assert_eq!(
            crosvm_balloon_args(536_870_912, Path::new("/run/hyperos/c.sock")),
            vec!["balloon", "536870912", "/run/hyperos/c.sock"]
        );
        assert_eq!(
            crosvm_suspend_args(Path::new("/run/hyperos/c.sock")),
            vec!["suspend", "/run/hyperos/c.sock"]
        );
        assert_eq!(
            crosvm_resume_args(Path::new("/run/hyperos/c.sock")),
            vec!["resume", "/run/hyperos/c.sock"]
        );
        assert_eq!(
            crosvm_snapshot_args(
                Path::new("/var/lib/hyperos/snap"),
                Path::new("/run/hyperos/c.sock")
            ),
            vec![
                "snapshot",
                "take",
                "/var/lib/hyperos/snap",
                "/run/hyperos/c.sock"
            ]
        );
    }

    #[tokio::test]
    async fn mock_hypervisor_records_snapshot_runtime_control_call() {
        let hv = MockHypervisor::new();
        hv.snapshot_runtime(
            "b6f51eed-8dc7-47f6-a8e7-81b6ae783a21",
            Path::new("/run/hyperos/vms/b6f51eed/crosvm.sock"),
            Path::new("/var/lib/hyperos/instances/b6f51eed/snapshots/snap1"),
        )
        .await
        .unwrap();

        let events = hv.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            MockHypervisorEvent::Snapshot { args, .. }
                if args == &vec![
                    "snapshot",
                    "take",
                    "/var/lib/hyperos/instances/b6f51eed/snapshots/snap1",
                    "/run/hyperos/vms/b6f51eed/crosvm.sock",
                ]
        ));
    }
}
