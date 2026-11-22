//! End-to-end integration tests for `vmm-daemon` over real `AF_UNIX` sockets.

use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;
use vmm_daemon::auth::PeerPolicy;
use vmm_daemon::hypervisor::{MockHypervisor, MockHypervisorEvent};
use vmm_daemon::manager::{HostResources, VmManager};
use vmm_daemon::network::MockNetworkController;
use vmm_daemon::rpc::{error_code, BaseImageManifest, MAX_FRAME_BYTES};
use vmm_daemon::server::BoundRpcServer;
use vmm_daemon::storage::{sha256_file, StorageManager};

struct TestHarness {
    base_dir: PathBuf,
    storage_dir: PathBuf,
    runtime_dir: PathBuf,
    socket_path: PathBuf,
    manager: Arc<VmManager>,
    hypervisor: Arc<MockHypervisor>,
    network: Arc<MockNetworkController>,
    shutdown_tx: watch::Sender<bool>,
    server_task: tokio::task::JoinHandle<Result<(), String>>,
}

impl TestHarness {
    async fn start(host: HostResources) -> Self {
        let base_dir =
            std::env::temp_dir().join(format!("hyperos-vmm-integ-{}", uuid::Uuid::new_v4()));
        let storage_dir = base_dir.join("storage");
        let runtime_dir = base_dir.join("run");
        let socket_path = runtime_dir.join("vmm.sock");

        fs::create_dir_all(&storage_dir).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        seed_base_image(
            &storage_dir,
            "alpine-3-21-virtio",
            b"alpine-sparse-raw-disk",
        );
        seed_base_image(
            &storage_dir,
            "debian-12-slim-virtio",
            b"debian-sparse-raw-disk",
        );

        let storage = StorageManager::new(&storage_dir, false).unwrap();
        let network = Arc::new(MockNetworkController::new());
        let hypervisor = Arc::new(MockHypervisor::new());
        let manager = VmManager::new(
            storage,
            network.clone(),
            hypervisor.clone(),
            &runtime_dir,
            "/boot/bzImage",
            host,
            true,
        )
        .unwrap();

        let uid = unsafe { libc::geteuid() };
        let server = BoundRpcServer::bind(&socket_path, PeerPolicy::uid_only(uid)).unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mgr_clone = Arc::clone(&manager);
        let server_task = tokio::spawn(async move { server.serve(mgr_clone, shutdown_rx).await });

        Self {
            base_dir,
            storage_dir,
            runtime_dir,
            socket_path,
            manager,
            hypervisor,
            network,
            shutdown_tx,
            server_task,
        }
    }

    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.server_task.await;
        self.manager.shutdown_all().await;
        let _ = fs::remove_dir_all(&self.base_dir);
    }
}

fn seed_base_image(storage_root: &Path, image_id: &str, data: &[u8]) {
    let dir = storage_root.join("base-images").join(image_id);
    fs::create_dir_all(&dir).unwrap();
    let disk = dir.join("disk.raw");
    fs::write(&disk, data).unwrap();
    let sha = sha256_file(&disk).unwrap();
    let manifest = BaseImageManifest {
        image_id: image_id.to_string(),
        sha256: sha,
        format: "raw".to_string(),
    };
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

async fn rpc_call(stream: &mut UnixStream, req: &Value) -> Value {
    let mut line = serde_json::to_string(req).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    let n = reader.read_line(&mut resp_line).await.unwrap();
    assert!(n > 0, "server closed connection unexpectedly");
    serde_json::from_str(&resp_line).unwrap()
}

#[tokio::test]
async fn end_to_end_lifecycle_balloon_pause_snapshot_network_and_crosvm_args() {
    let harness = TestHarness::start(HostResources {
        mem_total_mib: 4096,
        online_cpus: 4,
    })
    .await;

    // Verify restrictive socket and parent dir permissions
    let sock_mode = fs::metadata(&harness.socket_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let dir_mode = fs::metadata(&harness.runtime_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(sock_mode, 0o600);
    assert_eq!(dir_mode, 0o700);

    let mut client = UnixStream::connect(&harness.socket_path).await.unwrap();

    // 1. list_vms (empty)
    let list0 = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"list-0","method":"list_vms","params":{}}),
    )
    .await;
    assert_eq!(list0["result"]["vms"].as_array().unwrap().len(), 0);

    // 2. create_vm
    let created = rpc_call(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": "create-1",
            "method": "create_vm",
            "params": {
                "name": "alpine-box",
                "base_image_id": "alpine-3-21-virtio",
                "vcpu_count": 2,
                "ram_mib": 1024,
                "network_mode": "isolated",
                "disposable": false
            }
        }),
    )
    .await;
    let vm_obj = created["result"].as_object().unwrap();
    // Strictly 9 keys matching ARCHITECTURE.md verbatim
    assert_eq!(vm_obj.len(), 9);
    assert_eq!(vm_obj["disposable"], false);
    let vm_id = vm_obj["vm_id"].as_str().unwrap().to_string();
    assert_eq!(vm_obj["state"], "stopped");
    assert_eq!(vm_obj["balloon_target_mib"], 1024);
    assert_eq!(vm_obj["network_mode"], "isolated");

    // Verify CoW disk and persistent metadata on disk
    assert!(harness
        .storage_dir
        .join("instances")
        .join(&vm_id)
        .join("current/disk.raw")
        .is_file());
    assert!(harness
        .storage_dir
        .join("instances")
        .join(&vm_id)
        .join("metadata.json")
        .is_file());

    // 3. set_network_mode while stopped -> nat
    let net_upd = rpc_call(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": "net-1",
            "method": "set_network_mode",
            "params": {"vm_id": vm_id, "network_mode": "nat"}
        }),
    )
    .await;
    assert_eq!(net_upd["result"]["network_mode"], "nat");

    // 4. start_vm
    let started = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"start-1","method":"start_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(started["result"]["state"], "running");
    assert_eq!(started["result"]["balloon_target_mib"], 1024);
    assert_eq!(harness.network.active_allocations().len(), 1);

    // Verify crosvm CLI args include virtio-blk, virtio-net, virtio-balloon, virtio-vsock, virtio-gpu
    let events = harness.hypervisor.events();
    let spawn_args = match &events[0] {
        MockHypervisorEvent::Spawn { args, .. } => args,
        other => panic!("expected Spawn event, got {other:?}"),
    };
    assert_eq!(spawn_args[0], "run");
    assert!(spawn_args.contains(&"--balloon".to_string()));
    assert!(spawn_args.contains(&"--vsock".to_string()));
    assert!(spawn_args.contains(&"--gpu".to_string()));
    assert!(spawn_args.contains(&"--net".to_string()));
    assert!(spawn_args
        .iter()
        .any(|a| a.starts_with("path=") && a.ends_with(",root")));
    assert!(!spawn_args.contains(&"--disable-sandbox".to_string()));

    // 5. set_balloon_ram -> 512 MiB => balloon_bytes = (1024 - 512) * 1048576 = 536870912
    let ballooned = rpc_call(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": "balloon-1",
            "method": "set_balloon_ram",
            "params": {"vm_id": vm_id, "ram_mib": 512}
        }),
    )
    .await;
    assert_eq!(ballooned["result"]["balloon_target_mib"], 512);
    assert!(harness.hypervisor.events().iter().any(|ev| matches!(
        ev,
        MockHypervisorEvent::Balloon {
            balloon_bytes: 536_870_912,
            ..
        }
    )));

    // 6. snapshot_vm while running must fail with -32002
    let bad_snap = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"snap-bad","method":"snapshot_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(bad_snap["error"]["code"], error_code::INVALID_STATE);

    // 7. pause_vm -> resume via start_vm (preserves balloon_target_mib = 512)
    let paused = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"pause-1","method":"pause_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(paused["result"]["state"], "paused");

    let resumed = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"resume-1","method":"start_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(resumed["result"]["state"], "running");
    assert_eq!(resumed["result"]["balloon_target_mib"], 512);

    // 8. stop_vm (hard power-off + network teardown)
    let stopped = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"stop-1","method":"stop_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(stopped["result"]["state"], "stopped");
    assert!(harness.network.active_allocations().is_empty());

    // 9. snapshot_vm while stopped -> succeeds and writes immutable snapshot metadata
    let snapped = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"snap-1","method":"snapshot_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    let snap_obj = snapped["result"].as_object().unwrap();
    assert_eq!(snap_obj.len(), 3);
    let snap_id = snap_obj["snapshot_id"].as_str().unwrap();
    assert_eq!(snap_obj["vm_id"].as_str().unwrap(), vm_id);
    assert!(snap_obj["created_at"].as_str().unwrap().ends_with('Z'));
    assert!(harness
        .storage_dir
        .join("instances")
        .join(&vm_id)
        .join("snapshots")
        .join(snap_id)
        .is_dir());
    assert!(harness
        .storage_dir
        .join("instances")
        .join(&vm_id)
        .join("snapshot-metadata")
        .join(format!("{snap_id}.json"))
        .is_file());

    // 10. Cold restart resets balloon_target_mib back to ram_mib (1024)
    let restarted = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"start-2","method":"start_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(restarted["result"]["balloon_target_mib"], 1024);

    harness.shutdown().await;
}

#[tokio::test]
async fn ephemeral_vm_purges_cow_overlay_on_stop_and_on_unexpected_crosvm_crash() {
    let harness = TestHarness::start(HostResources {
        mem_total_mib: 4096,
        online_cpus: 4,
    })
    .await;
    let mut client = UnixStream::connect(&harness.socket_path).await.unwrap();

    let created = rpc_call(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": "ephem-create",
            "method": "create_vm",
            "params": {
                "name": "ephemeral-sandbox",
                "base_image_id": "alpine-3-21-virtio",
                "vcpu_count": 1,
                "ram_mib": 512,
                "network_mode": "host-only",
                "disposable": true
            }
        }),
    )
    .await;
    let vm_id = created["result"]["vm_id"].as_str().unwrap().to_string();
    assert_eq!(created["result"]["disposable"], true);

    // Disposable VMs cannot be snapshotted.
    let snap = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"ephem-snap","method":"snapshot_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(snap["error"]["code"], error_code::INVALID_STATE);

    let overlay_path = harness
        .storage_dir
        .join("instances")
        .join(&vm_id)
        .join("current/disk.raw");
    let runtime_vm_dir = harness.runtime_dir.join("vms").join(&vm_id);

    // Start -> Stop should immediately delete CoW overlay and runtime artifacts
    let _ = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"ephem-start-1","method":"start_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert!(overlay_path.is_file());
    assert!(runtime_vm_dir.is_dir());

    let stopped = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"ephem-stop-1","method":"stop_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(stopped["result"]["state"], "stopped");
    assert!(
        !overlay_path.exists(),
        "ephemeral CoW overlay must be deleted on stop_vm"
    );
    assert!(
        !runtime_vm_dir.exists(),
        "runtime artifacts must be deleted on stop_vm"
    );

    // Start again -> transparently re-clones CoW overlay -> simulate crosvm crash!
    let restarted = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"ephem-start-2","method":"start_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(restarted["result"]["state"], "running");
    assert!(overlay_path.is_file());
    assert!(harness.hypervisor.simulate_crash(&vm_id));

    // Wait briefly for async crash supervisor to reconcile state and purge ephemeral overlay
    tokio::time::sleep(Duration::from_millis(80)).await;

    let listed = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"ephem-list","method":"list_vms","params":{}}),
    )
    .await;
    let vm = &listed["result"]["vms"][0];
    assert_eq!(vm["state"], "failed");
    assert!(
        !overlay_path.exists(),
        "ephemeral CoW overlay must be deleted on crash"
    );
    assert!(
        !runtime_vm_dir.exists(),
        "runtime artifacts must be deleted on crash"
    );
    assert!(harness.network.active_allocations().is_empty());

    harness.shutdown().await;
}

#[tokio::test]
async fn recovers_persisted_state_across_daemon_restart_without_stale_running_state() {
    let base_dir =
        std::env::temp_dir().join(format!("hyperos-vmm-restart-{}", uuid::Uuid::new_v4()));
    let storage_dir = base_dir.join("storage");
    let runtime_dir = base_dir.join("run");
    fs::create_dir_all(&storage_dir).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    seed_base_image(&storage_dir, "alpine-3-21-virtio", b"alpine-disk");

    // Instance 1: create and start a VM, then simulate abrupt daemon restart (without calling stop_vm)
    let vm_id = {
        let storage = StorageManager::new(&storage_dir, false).unwrap();
        let mgr = VmManager::new(
            storage,
            Arc::new(MockNetworkController::new()),
            Arc::new(MockHypervisor::new()),
            &runtime_dir,
            "/boot/bzImage",
            HostResources {
                mem_total_mib: 4096,
                online_cpus: 4,
            },
            true,
        )
        .unwrap();

        let vm = mgr
            .create_vm(vmm_daemon::rpc::CreateVmParams {
                name: "persistent-vm".to_string(),
                base_image_id: "alpine-3-21-virtio".to_string(),
                vcpu_count: 2,
                ram_mib: 1024,
                network_mode: vmm_daemon::rpc::NetworkMode::Isolated,
                disposable: false,
            })
            .await
            .unwrap();
        mgr.start_vm(&vm.vm_id).await.unwrap();
        vm.vm_id
    };

    // Instance 2: fresh VmManager must reconcile persisted `running` -> `stopped`
    let storage2 = StorageManager::new(&storage_dir, false).unwrap();
    let mgr2 = VmManager::new(
        storage2,
        Arc::new(MockNetworkController::new()),
        Arc::new(MockHypervisor::new()),
        &runtime_dir,
        "/boot/bzImage",
        HostResources {
            mem_total_mib: 4096,
            online_cpus: 4,
        },
        true,
    )
    .unwrap();

    let list = mgr2.list_vms().await.unwrap();
    assert_eq!(list.vms.len(), 1);
    assert_eq!(list.vms[0].vm_id, vm_id);
    assert_eq!(list.vms[0].name, "persistent-vm");
    assert_eq!(list.vms[0].state, vmm_daemon::rpc::VmState::Stopped);

    let _ = fs::remove_dir_all(&base_dir);
}

#[tokio::test]
async fn enforces_4gib_admission_control_concurrent_clients_and_protocol_errors() {
    // 4096 MiB host => 4096 - 768 = 3328 MiB budget
    let harness = TestHarness::start(HostResources {
        mem_total_mib: 4096,
        online_cpus: 4,
    })
    .await;

    let mut c1 = UnixStream::connect(&harness.socket_path).await.unwrap();
    let mut c2 = UnixStream::connect(&harness.socket_path).await.unwrap();

    // Concurrent creates from two separate Unix socket clients
    let req1 = json!({
        "jsonrpc": "2.0",
        "id": "c1",
        "method": "create_vm",
        "params": {
            "name": "vm-a",
            "base_image_id": "alpine-3-21-virtio",
            "vcpu_count": 2,
            "ram_mib": 2048,
            "network_mode": "isolated",
            "disposable": false
        }
    });
    let req2 = json!({
        "jsonrpc": "2.0",
        "id": "c2",
        "method": "create_vm",
        "params": {
            "name": "vm-b",
            "base_image_id": "debian-12-slim-virtio",
            "vcpu_count": 2,
            "ram_mib": 1536,
            "network_mode": "isolated",
            "disposable": false
        }
    });
    let (r1, r2) = tokio::join!(rpc_call(&mut c1, &req1), rpc_call(&mut c2, &req2));
    let id_a = r1["result"]["vm_id"].as_str().unwrap().to_string();
    let id_b = r2["result"]["vm_id"].as_str().unwrap().to_string();

    // Start vm-a (2048 MiB active). Starting vm-b (2048 + 1536 = 3584 > 3328) must fail with -32003.
    let start_a = rpc_call(
        &mut c1,
        &json!({"jsonrpc":"2.0","id":"sa","method":"start_vm","params":{"vm_id":id_a}}),
    )
    .await;
    assert_eq!(start_a["result"]["state"], "running");

    let start_b = rpc_call(
        &mut c2,
        &json!({"jsonrpc":"2.0","id":"sb","method":"start_vm","params":{"vm_id":id_b}}),
    )
    .await;
    assert_eq!(start_b["error"]["code"], error_code::INSUFFICIENT_RESOURCES);

    // Reject unknown field on create_vm (-32602)
    let bad_key = rpc_call(
        &mut c1,
        &json!({
            "jsonrpc": "2.0",
            "id": "bk",
            "method": "create_vm",
            "params": {
                "name": "bad",
                "base_image_id": "alpine-3-21-virtio",
                "vcpu_count": 1,
                "ram_mib": 512,
                "network_mode": "isolated",
                "disposable": false,
                "ephemeral": true
            }
        }),
    )
    .await;
    assert_eq!(bad_key["error"]["code"], error_code::INVALID_PARAMS);

    // Oversized frame (> 65536 bytes) must close connection without parsing further
    let mut oversized_client = UnixStream::connect(&harness.socket_path).await.unwrap();
    let mut huge = vec![b' '; MAX_FRAME_BYTES + 16];
    huge.push(b'\n');
    let _ = oversized_client.write_all(&huge).await;
    let mut buf = [0u8; 64];
    let read_res = tokio::io::AsyncReadExt::read(&mut oversized_client, &mut buf).await;
    assert!(matches!(read_res, Ok(0) | Err(_)));

    // Live daemon lock must block a second server binding the same socket
    let uid = unsafe { libc::geteuid() };
    assert!(BoundRpcServer::bind(&harness.socket_path, PeerPolicy::uid_only(uid)).is_err());

    harness.shutdown().await;
}

#[tokio::test]
async fn cleans_up_stale_socket_and_runs_dry_run_binary_with_sigterm() {
    let base_dir = std::env::temp_dir().join(format!("hyperos-vmm-bin-{}", uuid::Uuid::new_v4()));
    let storage_dir = base_dir.join("storage");
    let run_dir = base_dir.join("run");
    let sock_path = run_dir.join("vmm.sock");
    fs::create_dir_all(&storage_dir).unwrap();
    fs::create_dir_all(&run_dir).unwrap();

    // Create a stale Unix socket file left behind by a dead listener
    {
        let _stale = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
    }
    assert!(sock_path.exists());

    let bin = env!("CARGO_BIN_EXE_vmm-daemon");
    let mut child = tokio::process::Command::new(bin)
        .args(["--dry-run", "--socket"])
        .arg(&sock_path)
        .arg("--storage-dir")
        .arg(&storage_dir)
        .spawn()
        .unwrap();

    // Wait for daemon to clean stale socket and accept connections
    let mut stream = None;
    for _ in 0..40 {
        if let Ok(s) = UnixStream::connect(&sock_path).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut stream = stream.expect("vmm-daemon --dry-run did not bind Unix socket in time");

    let resp = rpc_call(
        &mut stream,
        &json!({"jsonrpc":"2.0","id":"smoke-1","method":"list_vms","params":{}}),
    )
    .await;
    assert_eq!(resp["id"], "smoke-1");
    assert_eq!(resp["result"]["vms"].as_array().unwrap().len(), 0);
    drop(stream);

    // Send SIGTERM and verify clean exit and socket removal
    let pid = child.id().unwrap() as libc::pid_t;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("vmm-daemon did not exit promptly on SIGTERM")
        .unwrap();
    assert!(status.success());
    assert!(!sock_path.exists());

    let _ = fs::remove_dir_all(&base_dir);
}

#[tokio::test]
async fn rejects_symlink_socket_and_enforces_state_guards_and_idempotency() {
    let harness = TestHarness::start(HostResources {
        mem_total_mib: 4096,
        online_cpus: 4,
    })
    .await;

    // Binding over a symlink at socket_path must be rejected cleanly.
    let symlink_dir = harness.base_dir.join("symlink-test");
    fs::create_dir_all(&symlink_dir).unwrap();
    let real_file = symlink_dir.join("victim");
    fs::write(&real_file, b"do-not-delete").unwrap();
    let sym_sock = symlink_dir.join("vmm.sock");
    std::os::unix::fs::symlink(&real_file, &sym_sock).unwrap();
    let uid = unsafe { libc::geteuid() };
    assert!(BoundRpcServer::bind(&sym_sock, PeerPolicy::uid_only(uid)).is_err());
    assert!(real_file.exists());

    let mut client = UnixStream::connect(&harness.socket_path).await.unwrap();

    // Create VM -> test stopped state guards and idempotency.
    let created = rpc_call(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": "guard-create",
            "method": "create_vm",
            "params": {
                "name": "guard-vm",
                "base_image_id": "alpine-3-21-virtio",
                "vcpu_count": 1,
                "ram_mib": 512,
                "network_mode": "isolated",
                "disposable": false
            }
        }),
    )
    .await;
    let vm_id = created["result"]["vm_id"].as_str().unwrap().to_string();

    // pause_vm and set_balloon_ram while stopped must fail with -32002.
    let pause_stopped = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-p1","method":"pause_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(pause_stopped["error"]["code"], error_code::INVALID_STATE);

    let balloon_stopped = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-b1","method":"set_balloon_ram","params":{"vm_id":vm_id,"ram_mib":256}}),
    )
    .await;
    assert_eq!(balloon_stopped["error"]["code"], error_code::INVALID_STATE);

    // stop_vm on already-stopped VM is idempotent.
    let stop_idem = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-s1","method":"stop_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(stop_idem["result"]["state"], "stopped");

    // Start VM -> start_vm is idempotent; set_network_mode while running returns -32002.
    let _ = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-st1","method":"start_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    let start_idem = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-st2","method":"start_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(start_idem["result"]["state"], "running");

    let net_running = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-n1","method":"set_network_mode","params":{"vm_id":vm_id,"network_mode":"nat"}}),
    )
    .await;
    assert_eq!(net_running["error"]["code"], error_code::INVALID_STATE);

    // Out-of-bounds balloon target (< 128 or > max ram_mib) returns -32602.
    let balloon_low = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-bl","method":"set_balloon_ram","params":{"vm_id":vm_id,"ram_mib":64}}),
    )
    .await;
    assert_eq!(balloon_low["error"]["code"], error_code::INVALID_PARAMS);

    let balloon_high = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-bh","method":"set_balloon_ram","params":{"vm_id":vm_id,"ram_mib":1024}}),
    )
    .await;
    assert_eq!(balloon_high["error"]["code"], error_code::INVALID_PARAMS);

    // Pause twice -> idempotent; balloon while paused returns -32002.
    let p1 = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-p2","method":"pause_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(p1["result"]["state"], "paused");
    let p2 = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-p3","method":"pause_vm","params":{"vm_id":vm_id}}),
    )
    .await;
    assert_eq!(p2["result"]["state"], "paused");

    let balloon_paused = rpc_call(
        &mut client,
        &json!({"jsonrpc":"2.0","id":"g-bp","method":"set_balloon_ram","params":{"vm_id":vm_id,"ram_mib":256}}),
    )
    .await;
    assert_eq!(balloon_paused["error"]["code"], error_code::INVALID_STATE);

    harness.shutdown().await;
}

fn call(id: &str, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[tokio::test]
async fn snapshot_restore_delete_and_packet_capture_over_socket() {
    let harness = TestHarness::start(HostResources {
        mem_total_mib: 4096,
        online_cpus: 4,
    })
    .await;
    let mut c = UnixStream::connect(&harness.socket_path).await.unwrap();

    let created = rpc_call(
        &mut c,
        &call(
            "c1",
            "create_vm",
            json!({"name":"lab","base_image_id":"alpine-3-21-virtio","vcpu_count":1,
                   "ram_mib":512,"network_mode":"nat","disposable":false}),
        ),
    )
    .await;
    let vm_id = created["result"]["vm_id"].as_str().unwrap().to_string();
    let vm = json!({"vm_id": vm_id});
    let disk = harness
        .storage_dir
        .join("instances")
        .join(&vm_id)
        .join("current/disk.raw");

    // Snapshot a known disk state, dirty it, then restore.
    fs::write(&disk, b"clean-state").unwrap();
    let snap = rpc_call(&mut c, &call("s1", "snapshot_vm", vm.clone())).await;
    let snapshot_id = snap["result"]["snapshot_id"].as_str().unwrap().to_string();
    fs::write(&disk, b"infected-state").unwrap();

    let listed = rpc_call(&mut c, &call("ls1", "list_snapshots", vm.clone())).await;
    assert_eq!(listed["result"]["snapshots"], json!([snap["result"]]));

    let snap_ref = json!({"vm_id": vm_id, "snapshot_id": snapshot_id});
    let restored = rpc_call(&mut c, &call("r1", "restore_snapshot", snap_ref.clone())).await;
    assert_eq!(restored["result"]["vm_id"], vm_id.as_str());
    assert_eq!(restored["result"]["state"], "stopped");
    assert_eq!(fs::read(&disk).unwrap(), b"clean-state");

    let missing = json!({"vm_id": vm_id, "snapshot_id": "00000000-0000-4000-8000-000000000000"});
    let r = rpc_call(&mut c, &call("r2", "restore_snapshot", missing)).await;
    assert_eq!(r["error"]["code"], error_code::NOT_FOUND);
    let bad = json!({"vm_id": vm_id, "snapshot_id": "../current"});
    let r = rpc_call(&mut c, &call("r3", "restore_snapshot", bad)).await;
    assert_eq!(r["error"]["code"], error_code::INVALID_PARAMS);

    // Capture needs a running VM.
    let r = rpc_call(&mut c, &call("cap0", "start_capture", vm.clone())).await;
    assert_eq!(r["error"]["code"], error_code::INVALID_STATE);
    let r = rpc_call(&mut c, &call("cap0s", "stop_capture", vm.clone())).await;
    assert_eq!(r["error"]["code"], error_code::NOT_FOUND);

    let started = rpc_call(&mut c, &call("st1", "start_vm", vm.clone())).await;
    assert_eq!(started["result"]["state"], "running");
    let r = rpc_call(&mut c, &call("r4", "restore_snapshot", snap_ref.clone())).await;
    assert_eq!(r["error"]["code"], error_code::INVALID_STATE);

    let cap = rpc_call(&mut c, &call("cap1", "start_capture", vm.clone())).await;
    let cap_obj = cap["result"].as_object().unwrap();
    assert_eq!(cap_obj.len(), 5);
    assert_eq!(cap_obj["state"], "running");
    let capture_id = cap_obj["capture_id"].as_str().unwrap().to_string();
    let r = rpc_call(&mut c, &call("cap2", "start_capture", vm.clone())).await;
    assert_eq!(r["error"]["code"], error_code::INVALID_STATE);

    assert!(harness.network.inject_frame(&vm_id, vec![0x42; 60]));
    let mut live_bytes = 0;
    for _ in 0..100 {
        let l = rpc_call(&mut c, &call("lc", "list_captures", vm.clone())).await;
        live_bytes = l["result"]["captures"][0]["bytes"].as_u64().unwrap();
        if live_bytes == 24 + 16 + 60 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(live_bytes, 24 + 16 + 60);

    let cap_ref = json!({"vm_id": vm_id, "capture_id": capture_id});
    let r = rpc_call(&mut c, &call("d0", "delete_capture", cap_ref.clone())).await;
    assert_eq!(r["error"]["code"], error_code::INVALID_STATE);

    let stopped = rpc_call(&mut c, &call("cap3", "stop_capture", vm.clone())).await;
    assert_eq!(stopped["result"]["state"], "stopped");
    assert_eq!(stopped["result"]["bytes"], 24 + 16 + 60);
    // Idempotent: no capture running, so the latest is returned again.
    let again = rpc_call(&mut c, &call("cap4", "stop_capture", vm.clone())).await;
    assert_eq!(again["result"], stopped["result"]);

    let pcap = harness
        .storage_dir
        .join("captures")
        .join(&vm_id)
        .join(format!("{capture_id}.pcap"));
    let bytes = fs::read(&pcap).unwrap();
    assert_eq!(bytes.len(), 24 + 16 + 60);
    assert_eq!(&bytes[0..4], &0xa1b2_c3d4u32.to_le_bytes());

    // Stopping the VM ends a running capture.
    let cap = rpc_call(&mut c, &call("cap5", "start_capture", vm.clone())).await;
    let second_id = cap["result"]["capture_id"].as_str().unwrap().to_string();
    let _ = rpc_call(&mut c, &call("sp1", "stop_vm", vm.clone())).await;
    let l = rpc_call(&mut c, &call("lc2", "list_captures", vm.clone())).await;
    let caps = l["result"]["captures"].as_array().unwrap();
    assert_eq!(caps.len(), 2);
    let second = caps
        .iter()
        .find(|c| c["capture_id"] == second_id.as_str())
        .unwrap();
    assert_eq!(second["state"], "stopped");

    let r = rpc_call(&mut c, &call("d1", "delete_capture", cap_ref)).await;
    assert_eq!(r["result"], json!({}));
    assert!(!pcap.exists());

    let r = rpc_call(&mut c, &call("ds", "delete_snapshot", snap_ref)).await;
    assert_eq!(r["result"], json!({}));
    let listed = rpc_call(&mut c, &call("ls2", "list_snapshots", vm.clone())).await;
    assert_eq!(listed["result"]["snapshots"], json!([]));

    // Isolated VMs have nothing to capture.
    let iso = rpc_call(
        &mut c,
        &call(
            "c2",
            "create_vm",
            json!({"name":"iso","base_image_id":"alpine-3-21-virtio","vcpu_count":1,
                   "ram_mib":256,"network_mode":"isolated","disposable":false}),
        ),
    )
    .await;
    let iso_vm = json!({"vm_id": iso["result"]["vm_id"]});
    let _ = rpc_call(&mut c, &call("st2", "start_vm", iso_vm.clone())).await;
    let r = rpc_call(&mut c, &call("cap6", "start_capture", iso_vm)).await;
    assert_eq!(r["error"]["code"], error_code::UNAVAILABLE_CAPABILITY);

    harness.shutdown().await;
}

#[tokio::test]
async fn group_policy_makes_socket_group_accessible() {
    let base_dir = std::env::temp_dir().join(format!("hyperos-grp-{}", uuid::Uuid::new_v4()));
    let run_dir = base_dir.join("run");
    let sock = run_dir.join("vmm.sock");
    // Our own primary GID stands in for `hyperos`, so no privilege is needed.
    let policy = PeerPolicy {
        uid: u32::MAX,
        gid: Some(unsafe { libc::getegid() }),
    };
    let server = BoundRpcServer::bind(&sock, policy).unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&run_dir), 0o750);
    assert_eq!(mode(&sock), 0o660);

    let storage_dir = base_dir.join("storage");
    let storage = StorageManager::new(&storage_dir, false).unwrap();
    let manager = VmManager::new(
        storage,
        Arc::new(MockNetworkController::new()),
        Arc::new(MockHypervisor::new()),
        &run_dir,
        "/boot/bzImage",
        HostResources {
            mem_total_mib: 4096,
            online_cpus: 2,
        },
        true,
    )
    .unwrap();
    // Creating the manager must not tighten the socket directory back to 0700.
    assert_eq!(mode(&run_dir), 0o750);

    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(server.serve(Arc::clone(&manager), rx));
    // UID does not match (u32::MAX); access is granted through the group alone.
    let mut client = UnixStream::connect(&sock).await.unwrap();
    let resp = rpc_call(&mut client, &call("g1", "list_vms", json!({}))).await;
    assert_eq!(resp["result"]["vms"], json!([]));

    let _ = tx.send(true);
    let _ = task.await;
    let _ = fs::remove_dir_all(&base_dir);
}
