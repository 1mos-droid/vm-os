//! HyperOS privileged VMM control-plane daemon (`vmm-daemon`).
//!
//! - Production invocation (`vmm-daemon`, zero arguments): binds `/run/hyperos/vmm.sock`
//!   with umask `077`, `SO_PEERCRED` UID 0 plus members of the `hyperos` group (socket
//!   `root:hyperos 0660`; root-only `0600` if the group is missing), strict Btrfs CoW storage at
//!   `/var/lib/hyperos`, `/dev/kvm`-backed `/usr/bin/crosvm`, and clean `SIGTERM` shutdown.
//! - Testing / CI invocation (`vmm-daemon --dry-run [--socket PATH] [--storage-dir DIR]`):
//!   uses the in-memory mock hypervisor and mock network sandbox controller so the full
//!   JSON-RPC server and CoW lifecycle can be exercised without root, `/dev/kvm`, or `crosvm`.

use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use vmm_daemon::auth::{PeerPolicy, CONTROL_GROUP};
use vmm_daemon::hypervisor::{CrosvmController, HypervisorController, MockHypervisor};
use vmm_daemon::manager::{HostResources, VmManager};
use vmm_daemon::network::{LinuxNetworkController, MockNetworkController, NetworkController};
use vmm_daemon::rpc::{DEFAULT_RUNTIME_DIR, DEFAULT_SOCKET_PATH, DEFAULT_STORAGE_ROOT};
use vmm_daemon::server::BoundRpcServer;
use vmm_daemon::storage::StorageManager;

struct DaemonConfig {
    dry_run: bool,
    socket_path: PathBuf,
    storage_root: PathBuf,
    runtime_root: PathBuf,
}

fn parse_cli() -> Result<Option<DaemonConfig>, String> {
    let mut dry_run = false;
    let mut socket_override: Option<PathBuf> = None;
    let mut storage_override: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--socket" => {
                let path = args
                    .next()
                    .ok_or_else(|| "--socket requires a path argument".to_string())?;
                socket_override = Some(PathBuf::from(path));
            }
            "--storage-dir" => {
                let path = args
                    .next()
                    .ok_or_else(|| "--storage-dir requires a directory argument".to_string())?;
                storage_override = Some(PathBuf::from(path));
            }
            "-h" | "--help" => {
                println!(
                    "HyperOS VMM Daemon (vmm-daemon)\n\
                     \n\
                     USAGE:\n\
                       vmm-daemon                                  Run in production mode (ARCHITECTURE.md)\n\
                       vmm-daemon --dry-run [OPTIONS]              Run with mock hypervisor & network for CI/testing\n\
                     \n\
                     DRY-RUN OPTIONS:\n\
                       --socket <PATH>        Override Unix socket path (default: {DEFAULT_SOCKET_PATH})\n\
                       --storage-dir <DIR>    Override storage root (default: {DEFAULT_STORAGE_ROOT})\n\
                       -h, --help             Show this help message"
                );
                return Ok(None);
            }
            other => {
                return Err(format!(
                    "unexpected argument '{other}'; production daemon accepts no arguments (use --dry-run for testing)"
                ));
            }
        }
    }

    // Per ARCHITECTURE.md: production daemon accepts no UI-provided host paths.
    if !dry_run && (socket_override.is_some() || storage_override.is_some()) {
        return Err(
            "--socket and --storage-dir overrides are permitted only with --dry-run".to_string(),
        );
    }

    let socket_path = socket_override.unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH));
    let storage_root = storage_override.unwrap_or_else(|| PathBuf::from(DEFAULT_STORAGE_ROOT));
    let runtime_root = if dry_run {
        socket_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_RUNTIME_DIR))
    } else {
        PathBuf::from(DEFAULT_RUNTIME_DIR)
    };

    Ok(Some(DaemonConfig {
        dry_run,
        socket_path,
        storage_root,
        runtime_root,
    }))
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("vmm-daemon: fatal: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let Some(cfg) = parse_cli()? else {
        return Ok(());
    };

    // Apply restrictive umask 077 per ARCHITECTURE.md.
    unsafe {
        libc::umask(0o077);
    }

    let policy = if cfg.dry_run {
        PeerPolicy::uid_only(unsafe { libc::geteuid() })
    } else {
        let euid = unsafe { libc::geteuid() };
        if euid != 0 {
            return Err(
                "production vmm-daemon must run as root (UID 0); use --dry-run for unprivileged testing"
                    .to_string(),
            );
        }
        let policy = PeerPolicy::production();
        if policy.gid.is_none() {
            eprintln!("vmm-daemon: group '{CONTROL_GROUP}' not found; control socket is root-only");
        }
        policy
    };

    let storage =
        StorageManager::new(&cfg.storage_root, !cfg.dry_run)?.with_access_group(policy.gid)?;
    let network: Arc<dyn NetworkController> = if cfg.dry_run {
        Arc::new(MockNetworkController::new())
    } else {
        Arc::new(LinuxNetworkController::new())
    };
    let hypervisor: Arc<dyn HypervisorController> = if cfg.dry_run {
        Arc::new(MockHypervisor::new())
    } else {
        Arc::new(CrosvmController::default_hyperos())
    };

    let manager = VmManager::new(
        storage,
        network,
        hypervisor,
        &cfg.runtime_root,
        VmManager::default_kernel_path(),
        HostResources::detect(),
        cfg.dry_run,
    )?;

    let server = BoundRpcServer::bind(&cfg.socket_path, policy)?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| format!("failed to install SIGTERM handler: {e}"))?;
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| format!("failed to install SIGINT handler: {e}"))?;

    tokio::spawn(async move {
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
        let _ = shutdown_tx.send(true);
    });

    // Sample the runtime assurance monitor every 2 s, so out-of-band changes
    // (e.g. a flushed firewall table) are caught without waiting for a request.
    let monitor_mgr = Arc::clone(&manager);
    let mut monitor_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = tick.tick() => monitor_mgr.check_invariants().await,
                _ = monitor_shutdown.changed() => break,
            }
        }
    });

    server.serve(Arc::clone(&manager), shutdown_rx).await?;
    // Per ARCHITECTURE.md: handle SIGTERM by stopping children and persisting metadata before exit.
    manager.shutdown_all().await;
    Ok(())
}
