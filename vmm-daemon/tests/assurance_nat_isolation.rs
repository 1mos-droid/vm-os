//! HYP001 end to end: a NAT Space's firewall table is tampered with, the
//! daemon stops the Space and fails closed. Own process (monitor state is global).

use std::fs;
use std::path::Path;
use std::sync::Arc;
use vmm_daemon::hypervisor::MockHypervisor;
use vmm_daemon::manager::{HostResources, VmManager};
use vmm_daemon::network::MockNetworkController;
use vmm_daemon::rpc::{error_code, BaseImageManifest, CreateVmParams, NetworkMode, VmState};
use vmm_daemon::storage::{sha256_file, StorageManager};

fn seed_base_image(storage_root: &Path, image_id: &str) {
    let dir = storage_root.join("base-images").join(image_id);
    fs::create_dir_all(&dir).unwrap();
    let disk = dir.join("disk.raw");
    fs::write(&disk, b"base-disk").unwrap();
    let manifest = BaseImageManifest {
        image_id: image_id.to_string(),
        sha256: sha256_file(&disk).unwrap(),
        format: "raw".to_string(),
    };
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

fn params(name: &str, network_mode: NetworkMode) -> CreateVmParams {
    CreateVmParams {
        name: name.to_string(),
        base_image_id: "alpine".to_string(),
        vcpu_count: 1,
        ram_mib: 512,
        network_mode,
        disposable: false,
    }
}

#[tokio::test]
async fn tampered_nat_firewall_stops_the_space_and_fails_closed() {
    let base = std::env::temp_dir().join(format!("hyperos-assure-{}", uuid::Uuid::new_v4()));
    let storage_dir = base.join("storage");
    fs::create_dir_all(&storage_dir).unwrap();
    seed_base_image(&storage_dir, "alpine");

    let network = Arc::new(MockNetworkController::new());
    let manager = VmManager::new(
        StorageManager::new(&storage_dir, false).unwrap(),
        network.clone(),
        Arc::new(MockHypervisor::new()),
        base.join("run"),
        "/boot/bzImage",
        HostResources {
            mem_total_mib: 4096,
            online_cpus: 2,
        },
        true,
    )
    .unwrap();

    let nat = manager
        .create_vm(params("browser", NetworkMode::Nat))
        .await
        .unwrap();
    let offline = manager
        .create_vm(params("vault", NetworkMode::Isolated))
        .await
        .unwrap();
    manager.start_vm(&nat.vm_id).await.unwrap();
    manager.start_vm(&offline.vm_id).await.unwrap();

    manager.check_invariants().await;
    assert!(manager
        .assurance_status()
        .monitors
        .iter()
        .all(|m| m.state == "ok"));

    // Someone flushes the NAT Space's nftables table.
    network.tamper_isolation(&nat.vm_id);
    manager.check_invariants().await;

    let status = manager.assurance_status();
    let hyp001 = status.monitors.iter().find(|m| m.id == "HYP001").unwrap();
    assert_eq!(hyp001.state, "violated");
    assert!(status
        .monitors
        .iter()
        .filter(|m| m.id != "HYP001")
        .all(|m| m.state == "ok"));

    // The unprotected NAT Space was stopped; the isolated one keeps running.
    let vms = manager.list_vms().await.unwrap().vms;
    let state = |id: &str| vms.iter().find(|v| v.vm_id == id).unwrap().state;
    assert_eq!(state(&nat.vm_id), VmState::Stopped);
    assert_eq!(state(&offline.vm_id), VmState::Running);

    // Fail closed: no new Spaces, starts or captures until the daemon restarts.
    let err = manager.start_vm(&nat.vm_id).await.unwrap_err();
    assert_eq!(err.code, error_code::INVARIANT_VIOLATED);
    assert!(err.message.contains("HYP001"));
    let err = manager
        .create_vm(params("again", NetworkMode::Nat))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::INVARIANT_VIOLATED);
    let err = manager.start_capture(&offline.vm_id).await.unwrap_err();
    assert_eq!(err.code, error_code::INVARIANT_VIOLATED);

    // Stopping is still allowed.
    assert_eq!(
        manager.stop_vm(&offline.vm_id).await.unwrap().state,
        VmState::Stopped
    );

    let _ = fs::remove_dir_all(&base);
}
