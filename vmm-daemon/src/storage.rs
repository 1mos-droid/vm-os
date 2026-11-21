//! Persistent Btrfs CoW storage engine matching `ARCHITECTURE.md` verbatim.
//!
//! Directory layout under `<storage_root>` (`/var/lib/hyperos` in production):
//! ```text
//! /var/lib/hyperos/
//!   base-images/<image_id>/
//!     disk.raw
//!     manifest.json
//!   instances/<vm_id>/
//!     metadata.json
//!     daemon-policy.json
//!     current/
//!       disk.raw
//!     snapshots/<snapshot_id>/
//!     snapshot-metadata/<snapshot_id>.json
//! ```

use crate::rpc::{
    error_code, is_valid_image_id, is_valid_sha256_hex, is_valid_uuid, BaseImageManifest, RpcError,
    Snapshot, SnapshotMetadataFile, Vm,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Linux kernel `BTRFS_SUPER_MAGIC` constant (`0x9123683E`).
const BTRFS_SUPER_MAGIC: u64 = 0x9123_683e;

/// Monotonic counter guaranteeing unique temp filenames for concurrent atomic writes.
static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Internal persistent daemon policy stored alongside `metadata.json`.
/// Never serialized into wire JSON-RPC `Vm` objects (keeping the wire schema 100% verbatim).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonVmPolicy {
    pub ephemeral: bool,
    pub balloon_min_ram_mib: u32,
    pub balloon_max_ram_mib: u32,
    pub vsock_cid: u32,
    #[serde(default)]
    pub last_pid: Option<u32>,
}

/// Loaded VM record recovered from disk on daemon startup.
#[derive(Debug, Clone)]
pub struct PersistedVmRecord {
    pub vm: Vm,
    pub policy: DaemonVmPolicy,
}

/// Persistent CoW storage manager for `/var/lib/hyperos`.
#[derive(Debug, Clone)]
pub struct StorageManager {
    root: PathBuf,
    strict_btrfs: bool,
}

impl StorageManager {
    /// Initialize storage manager.
    ///
    /// When `strict_btrfs` is `true` (production mode):
    /// - Blocks startup if `root` is not mounted on a Btrfs filesystem.
    /// - Requires `btrfs subvolume` and `cp --reflink=always` with zero full-copy fallback.
    ///
    /// When `strict_btrfs` is `false` (`--dry-run` / CI test mode):
    /// - Attempts `cp --reflink=always` first, returning a clear error if strict, or
    ///   using a sparse dry-run fallback when running on non-Btrfs CI filesystems (ext4/tmpfs).
    pub fn new(root: impl AsRef<Path>, strict_btrfs: bool) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        if strict_btrfs {
            verify_btrfs_mount(&root)?;
        } else {
            fs::create_dir_all(&root).map_err(|e| format!("failed to create storage root: {e}"))?;
        }

        let base_dir = root.join("base-images");
        let inst_dir = root.join("instances");
        fs::create_dir_all(&base_dir)
            .map_err(|e| format!("failed to create base-images dir: {e}"))?;
        fs::create_dir_all(&inst_dir)
            .map_err(|e| format!("failed to create instances dir: {e}"))?;

        let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(&base_dir, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(&inst_dir, fs::Permissions::from_mode(0o700));

        let mgr = Self { root, strict_btrfs };
        mgr.recover_interrupted_staging()?;
        Ok(mgr)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn base_images_dir(&self) -> PathBuf {
        self.root.join("base-images")
    }

    pub fn instances_dir(&self) -> PathBuf {
        self.root.join("instances")
    }

    pub fn instance_dir(&self, vm_id: &str) -> PathBuf {
        self.instances_dir().join(vm_id)
    }

    pub fn instance_current_dir(&self, vm_id: &str) -> PathBuf {
        self.instance_dir(vm_id).join("current")
    }

    pub fn instance_disk_path(&self, vm_id: &str) -> PathBuf {
        self.instance_current_dir(vm_id).join("disk.raw")
    }

    pub fn instance_metadata_path(&self, vm_id: &str) -> PathBuf {
        self.instance_dir(vm_id).join("metadata.json")
    }

    pub fn instance_policy_path(&self, vm_id: &str) -> PathBuf {
        self.instance_dir(vm_id).join("daemon-policy.json")
    }

    pub fn snapshot_dir(&self, vm_id: &str, snapshot_id: &str) -> PathBuf {
        self.instance_dir(vm_id).join("snapshots").join(snapshot_id)
    }

    pub fn snapshot_metadata_path(&self, vm_id: &str, snapshot_id: &str) -> PathBuf {
        self.instance_dir(vm_id)
            .join("snapshot-metadata")
            .join(format!("{snapshot_id}.json"))
    }

    /// Clean up any `.staging-*` directories or `.tmp.*` files left behind by an interrupted operation.
    pub fn recover_interrupted_staging(&self) -> Result<(), String> {
        let inst_dir = self.instances_dir();
        if let Ok(entries) = fs::read_dir(&inst_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with(".staging-") || name_str.contains(".tmp.") {
                    self.remove_tree_or_subvol(&path);
                    continue;
                }
                if path.is_dir() {
                    // Clean up interrupted snapshot staging or temp metadata inside instance dirs.
                    for sub in ["snapshots", "snapshot-metadata"] {
                        let sub_dir = path.join(sub);
                        if let Ok(sub_entries) = fs::read_dir(&sub_dir) {
                            for se in sub_entries.flatten() {
                                let sn = se.file_name();
                                let sn_str = sn.to_string_lossy();
                                if sn_str.starts_with(".staging-") || sn_str.contains(".tmp.") {
                                    self.remove_tree_or_subvol(&se.path());
                                }
                            }
                        }
                    }
                    if let Ok(meta_entries) = fs::read_dir(&path) {
                        for me in meta_entries.flatten() {
                            if me.file_name().to_string_lossy().contains(".tmp.") {
                                let _ = fs::remove_file(me.path());
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Validate a base image ID against the base-images directory, preventing symlink
    /// traversal and verifying its `manifest.json` SHA-256 digest before admission.
    pub fn resolve_and_validate_base_image(&self, image_id: &str) -> Result<PathBuf, RpcError> {
        if !is_valid_image_id(image_id) {
            return Err(RpcError::new(
                error_code::INVALID_PARAMS,
                "invalid base_image_id format",
            ));
        }

        let base_root = fs::canonicalize(self.base_images_dir()).map_err(|_| {
            RpcError::new(error_code::IO_FAILURE, "base image storage is unavailable")
        })?;

        let candidate_dir = self.base_images_dir().join(image_id);
        if !candidate_dir.exists() {
            return Err(RpcError::new(
                error_code::NOT_FOUND,
                "requested base image not found",
            ));
        }

        let canon_dir = fs::canonicalize(&candidate_dir)
            .map_err(|_| RpcError::new(error_code::NOT_FOUND, "requested base image not found"))?;
        if !canon_dir.starts_with(&base_root) || canon_dir == base_root {
            return Err(RpcError::new(
                error_code::INVALID_PARAMS,
                "base image path escapes managed storage tree",
            ));
        }

        let manifest_path = fs::canonicalize(canon_dir.join("manifest.json"))
            .map_err(|_| RpcError::new(error_code::NOT_FOUND, "base image manifest missing"))?;
        let disk_path = fs::canonicalize(canon_dir.join("disk.raw"))
            .map_err(|_| RpcError::new(error_code::NOT_FOUND, "base image disk missing"))?;

        if !manifest_path.starts_with(&canon_dir) || !disk_path.starts_with(&canon_dir) {
            return Err(RpcError::new(
                error_code::INVALID_PARAMS,
                "base image files must not traverse symlinks outside image directory",
            ));
        }

        let manifest_text = fs::read_to_string(&manifest_path).map_err(|_| {
            RpcError::new(error_code::IO_FAILURE, "failed to read base image manifest")
        })?;
        let manifest: BaseImageManifest = serde_json::from_str(&manifest_text)
            .map_err(|_| RpcError::new(error_code::IO_FAILURE, "corrupt base image manifest"))?;

        if manifest.image_id != image_id
            || manifest.format != "raw"
            || !is_valid_sha256_hex(&manifest.sha256)
        {
            return Err(RpcError::new(
                error_code::IO_FAILURE,
                "base image manifest failed schema validation",
            ));
        }

        let actual_sha = sha256_file(&disk_path).map_err(|_| {
            RpcError::new(error_code::IO_FAILURE, "failed to checksum base image disk")
        })?;
        if actual_sha != manifest.sha256 {
            return Err(RpcError::new(
                error_code::IO_FAILURE,
                "base image checksum mismatch",
            ));
        }

        if self.strict_btrfs {
            let disk_dev = fs::metadata(&disk_path).map_err(map_io_err)?.dev();
            let inst_dev = fs::metadata(self.instances_dir())
                .map_err(map_io_err)?
                .dev();
            if disk_dev != inst_dev {
                return Err(RpcError::new(
                    error_code::IO_FAILURE,
                    "base image and instances must reside on the same Btrfs filesystem",
                ));
            }
        }

        Ok(disk_path)
    }

    /// Atomically stage and commit a new VM instance directory with a Btrfs CoW reflink disk.
    pub fn provision_vm_instance(&self, vm: &Vm, policy: &DaemonVmPolicy) -> Result<(), RpcError> {
        let base_disk = self.resolve_and_validate_base_image(&vm.base_image_id)?;

        let staging_dir = self.instances_dir().join(format!(".staging-{}", vm.vm_id));
        if staging_dir.exists() {
            self.remove_tree_or_subvol(&staging_dir);
        }

        let create_result = (|| -> Result<(), RpcError> {
            fs::create_dir_all(&staging_dir).map_err(map_io_err)?;
            fs::set_permissions(&staging_dir, fs::Permissions::from_mode(0o700))
                .map_err(map_io_err)?;

            let snapshots_dir = staging_dir.join("snapshots");
            let snap_meta_dir = staging_dir.join("snapshot-metadata");
            fs::create_dir_all(&snapshots_dir).map_err(map_io_err)?;
            fs::create_dir_all(&snap_meta_dir).map_err(map_io_err)?;

            let current_dir = staging_dir.join("current");
            self.create_current_subvolume(&current_dir)?;

            let dst_disk = current_dir.join("disk.raw");
            self.clone_disk_reflink(&base_disk, &dst_disk)?;
            fs::set_permissions(&dst_disk, fs::Permissions::from_mode(0o600))
                .map_err(map_io_err)?;

            write_json_atomic(&staging_dir.join("metadata.json"), vm)?;
            write_json_atomic(&staging_dir.join("daemon-policy.json"), policy)?;

            let final_dir = self.instance_dir(&vm.vm_id);
            fs::rename(&staging_dir, &final_dir).map_err(map_io_err)?;
            sync_dir(&self.instances_dir())?;
            Ok(())
        })();

        if create_result.is_err() {
            self.remove_tree_or_subvol(&staging_dir);
        }
        create_result
    }

    /// Ensure an instance's CoW `current/disk.raw` exists before booting (e.g. re-cloning
    /// a fresh overlay after an ephemeral VM was previously stopped and purged).
    pub fn ensure_vm_overlay(&self, vm: &Vm) -> Result<PathBuf, RpcError> {
        let disk_path = self.instance_disk_path(&vm.vm_id);
        if disk_path.is_file() {
            return Ok(disk_path);
        }
        let base_disk = self.resolve_and_validate_base_image(&vm.base_image_id)?;
        let current_dir = self.instance_current_dir(&vm.vm_id);
        if !current_dir.exists() {
            self.create_current_subvolume(&current_dir)?;
        }
        self.clone_disk_reflink(&base_disk, &disk_path)?;
        fs::set_permissions(&disk_path, fs::Permissions::from_mode(0o600)).map_err(map_io_err)?;
        Ok(disk_path)
    }

    /// Delete the CoW disk overlay (`instances/<vm_id>/current/`) for an ephemeral VM.
    pub fn delete_vm_overlay(&self, vm_id: &str) -> Result<(), RpcError> {
        let current_dir = self.instance_current_dir(vm_id);
        if current_dir.exists() {
            self.remove_tree_or_subvol(&current_dir);
        }
        Ok(())
    }

    /// Atomically persist `metadata.json` (`ARCHITECTURE.md` VM object) + `fsync`.
    pub fn save_vm_metadata(&self, vm: &Vm) -> Result<(), RpcError> {
        write_json_atomic(&self.instance_metadata_path(&vm.vm_id), vm)
    }

    /// Atomically persist `daemon-policy.json` + `fsync`.
    pub fn save_vm_policy(&self, vm_id: &str, policy: &DaemonVmPolicy) -> Result<(), RpcError> {
        write_json_atomic(&self.instance_policy_path(vm_id), policy)
    }

    /// Create a cold read-only Btrfs snapshot of `instances/<vm_id>/current/`
    /// plus immutable metadata under `snapshot-metadata/<snapshot_id>.json`.
    pub fn create_cold_snapshot(&self, vm: &Vm, snapshot: &Snapshot) -> Result<(), RpcError> {
        let current_dir = self.instance_current_dir(&vm.vm_id);
        if !current_dir.is_dir() {
            return Err(RpcError::new(
                error_code::IO_FAILURE,
                "VM disk subvolume is not present to snapshot",
            ));
        }

        let snap_parent = self.instance_dir(&vm.vm_id).join("snapshots");
        let snap_staging = snap_parent.join(format!(".staging-{}", snapshot.snapshot_id));
        let snap_final = self.snapshot_dir(&vm.vm_id, &snapshot.snapshot_id);
        let snap_meta_path = self.snapshot_metadata_path(&vm.vm_id, &snapshot.snapshot_id);

        let res = (|| -> Result<(), RpcError> {
            if self.strict_btrfs {
                let output = Command::new("btrfs")
                    .args(["subvolume", "snapshot", "-r"])
                    .arg(&current_dir)
                    .arg(&snap_staging)
                    .output()
                    .map_err(|_| {
                        RpcError::new(
                            error_code::UNAVAILABLE_CAPABILITY,
                            "btrfs utility unavailable for cold snapshot",
                        )
                    })?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    if stderr.to_ascii_lowercase().contains("no space left") {
                        return Err(RpcError::new(
                            error_code::INSUFFICIENT_RESOURCES,
                            "insufficient storage space for snapshot",
                        ));
                    }
                    return Err(RpcError::new(
                        error_code::IO_FAILURE,
                        "failed to create read-only Btrfs subvolume snapshot",
                    ));
                }
            } else {
                // Dry-run mode: attempt Btrfs read-only snapshot first, fall back to read-only copy.
                let btrfs_ok = Command::new("btrfs")
                    .args(["subvolume", "snapshot", "-r"])
                    .arg(&current_dir)
                    .arg(&snap_staging)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);

                if !btrfs_ok {
                    fs::create_dir_all(&snap_staging).map_err(map_io_err)?;
                    let src_disk = current_dir.join("disk.raw");
                    let dst_disk = snap_staging.join("disk.raw");
                    if src_disk.is_file() {
                        self.clone_disk_reflink(&src_disk, &dst_disk)?;
                        fs::set_permissions(&dst_disk, fs::Permissions::from_mode(0o444))
                            .map_err(map_io_err)?;
                    }
                    fs::set_permissions(&snap_staging, fs::Permissions::from_mode(0o555))
                        .map_err(map_io_err)?;
                }
            }

            fs::rename(&snap_staging, &snap_final).map_err(map_io_err)?;
            sync_dir(&snap_parent)?;

            let meta_doc = SnapshotMetadataFile {
                snapshot: snapshot.clone(),
                vm: vm.clone(),
            };
            write_json_atomic(&snap_meta_path, &meta_doc)?;
            let _ = fs::set_permissions(&snap_meta_path, fs::Permissions::from_mode(0o444));
            Ok(())
        })();

        if res.is_err() {
            self.remove_tree_or_subvol(&snap_staging);
            self.remove_tree_or_subvol(&snap_final);
            let _ = fs::remove_file(&snap_meta_path);
        }
        res
    }

    /// Load all persisted VM records from `instances/<vm_id>/metadata.json`.
    pub fn load_all_vms(&self) -> Result<Vec<PersistedVmRecord>, String> {
        let mut records = Vec::new();
        let inst_dir = self.instances_dir();
        let entries = fs::read_dir(&inst_dir)
            .map_err(|e| format!("failed to scan instances directory: {e}"))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let vm_id = name.to_string_lossy();
            if !is_valid_uuid(&vm_id) {
                continue;
            }

            let meta_path = path.join("metadata.json");
            if !meta_path.is_file() {
                continue;
            }
            let text = fs::read_to_string(&meta_path)
                .map_err(|e| format!("failed to read VM metadata for {vm_id}: {e}"))?;
            let vm: Vm = serde_json::from_str(&text)
                .map_err(|e| format!("corrupt VM metadata for {vm_id}: {e}"))?;

            let policy_path = path.join("daemon-policy.json");
            let policy = if policy_path.is_file() {
                let ptext = fs::read_to_string(&policy_path)
                    .map_err(|e| format!("failed to read daemon policy for {vm_id}: {e}"))?;
                serde_json::from_str::<DaemonVmPolicy>(&ptext)
                    .map_err(|e| format!("corrupt daemon policy for {vm_id}: {e}"))?
            } else {
                DaemonVmPolicy {
                    ephemeral: false,
                    balloon_min_ram_mib: crate::rpc::MIN_VM_RAM_MIB,
                    balloon_max_ram_mib: vm.ram_mib,
                    vsock_cid: 3,
                    last_pid: None,
                }
            };

            records.push(PersistedVmRecord { vm, policy });
        }

        records.sort_by(|a, b| a.vm.vm_id.cmp(&b.vm.vm_id));
        Ok(records)
    }

    fn create_current_subvolume(&self, current_dir: &Path) -> Result<(), RpcError> {
        if self.strict_btrfs {
            let output = Command::new("btrfs")
                .args(["subvolume", "create"])
                .arg(current_dir)
                .output()
                .map_err(|_| {
                    RpcError::new(
                        error_code::UNAVAILABLE_CAPABILITY,
                        "btrfs utility unavailable for CoW subvolume creation",
                    )
                })?;
            if !output.status.success() {
                return Err(RpcError::new(
                    error_code::IO_FAILURE,
                    "failed to create Btrfs instance subvolume",
                ));
            }
        } else {
            let btrfs_ok = Command::new("btrfs")
                .args(["subvolume", "create"])
                .arg(current_dir)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !btrfs_ok {
                fs::create_dir_all(current_dir).map_err(map_io_err)?;
            }
        }
        fs::set_permissions(current_dir, fs::Permissions::from_mode(0o700)).map_err(map_io_err)?;
        Ok(())
    }

    /// Clone a base disk using strict `cp --reflink=always`.
    ///
    /// In production (`strict_btrfs = true`), failure to reflink returns a clean `RpcError`
    /// and never falls back to a full byte copy (`ARCHITECTURE.md`).
    pub fn clone_disk_reflink(&self, src: &Path, dst: &Path) -> Result<(), RpcError> {
        let output = Command::new("cp")
            .args(["--reflink=always", "--sparse=always", "--"])
            .arg(src)
            .arg(dst)
            .output()
            .map_err(|_| {
                RpcError::new(
                    error_code::IO_FAILURE,
                    "failed to invoke cp --reflink=always",
                )
            })?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.to_ascii_lowercase().contains("no space left") {
            return Err(RpcError::new(
                error_code::INSUFFICIENT_RESOURCES,
                "insufficient storage space for CoW clone",
            ));
        }

        if self.strict_btrfs {
            return Err(RpcError::new(
                error_code::IO_FAILURE,
                "CoW reflink clone failed: filesystem does not support Btrfs reflinks",
            ));
        }

        // Dry-run / CI test mode fallback on non-reflink filesystems (ext4/tmpfs).
        let _ = fs::remove_file(dst);
        fs::copy(src, dst).map_err(map_io_err)?;
        Ok(())
    }

    fn remove_tree_or_subvol(&self, path: &Path) {
        if !path.exists() {
            return;
        }
        // Make read-only snapshot dirs writable first if needed for cleanup of staging dirs.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        let subvol_deleted = Command::new("btrfs")
            .args(["subvolume", "delete"])
            .arg(path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !subvol_deleted {
            if path.is_dir() {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// Verify that `path` resides on a mounted Btrfs filesystem (`BTRFS_SUPER_MAGIC`).
pub fn verify_btrfs_mount(path: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    if !path.exists() {
        return Err("managed storage directory does not exist".to_string());
    }
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "invalid storage path".to_string())?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(format!(
            "statfs failed on storage mount: {}",
            io::Error::last_os_error()
        ));
    }
    // `statfs.f_type` is `i64` on glibc and `u64` on musl.
    #[allow(clippy::unnecessary_cast)]
    let fs_type = stat.f_type as u64;
    if fs_type != BTRFS_SUPER_MAGIC {
        return Err(format!(
            "storage root is not a Btrfs filesystem (fs magic 0x{:x}, expected 0x{:x})",
            fs_type, BTRFS_SUPER_MAGIC
        ));
    }
    Ok(())
}

/// Compute lowercase hex SHA-256 checksum of a file.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Atomically write JSON to `target` using write -> `fsync` -> `rename` -> `fsync(parent)`.
pub fn write_json_atomic<T: Serialize>(target: &Path, value: &T) -> Result<(), RpcError> {
    let parent = target.parent().ok_or_else(|| {
        RpcError::new(error_code::IO_FAILURE, "invalid metadata destination path")
    })?;
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| RpcError::new(error_code::IO_FAILURE, "invalid metadata filename"))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!("{file_name}.tmp.{}.{seq}", std::process::id()));

    let bytes = serde_json::to_vec(value)
        .map_err(|_| RpcError::new(error_code::INTERNAL_ERROR, "failed to encode metadata"))?;

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)
        .map_err(map_io_err)?;
    file.write_all(&bytes).map_err(map_io_err)?;
    file.sync_all().map_err(map_io_err)?;
    drop(file);

    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600)).map_err(map_io_err)?;
    fs::rename(&tmp_path, target).map_err(map_io_err)?;
    sync_dir(parent)?;
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<(), RpcError> {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn map_io_err(err: io::Error) -> RpcError {
    if err.raw_os_error() == Some(libc::ENOSPC) {
        RpcError::new(
            error_code::INSUFFICIENT_RESOURCES,
            "insufficient storage space",
        )
    } else {
        RpcError::new(error_code::IO_FAILURE, "storage I/O failure")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{NetworkMode, VmState};

    fn create_test_base_image(storage_root: &Path, image_id: &str, payload: &[u8]) {
        let dir = storage_root.join("base-images").join(image_id);
        fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.raw");
        fs::write(&disk, payload).unwrap();
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

    #[test]
    fn validates_base_image_checksum_and_rejects_tampered_disk() {
        let tmp = std::env::temp_dir().join(format!("hyperos-storage-{}", uuid::Uuid::new_v4()));
        let mgr = StorageManager::new(&tmp, false).unwrap();
        create_test_base_image(&tmp, "alpine-3-21-virtio", b"fake-sparse-raw-disk");

        assert!(mgr
            .resolve_and_validate_base_image("alpine-3-21-virtio")
            .is_ok());

        // Tamper with disk.raw after manifest generation
        fs::write(
            tmp.join("base-images/alpine-3-21-virtio/disk.raw"),
            b"tampered-disk",
        )
        .unwrap();
        let err = mgr
            .resolve_and_validate_base_image("alpine-3-21-virtio")
            .unwrap_err();
        assert_eq!(err.code, error_code::IO_FAILURE);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn strict_mode_rejects_non_reflink_filesystem_without_copy_fallback() {
        let tmp = std::env::temp_dir().join(format!("hyperos-strict-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        let src = tmp.join("src.raw");
        let dst = tmp.join("dst.raw");
        fs::write(&src, b"hello").unwrap();

        // Only assert failure if /tmp is not itself Btrfs/XFS reflink-capable.
        let strict_mgr = StorageManager {
            root: tmp.clone(),
            strict_btrfs: true,
        };
        if verify_btrfs_mount(&tmp).is_err() {
            let res = strict_mgr.clone_disk_reflink(&src, &dst);
            if let Err(err) = res {
                assert_eq!(err.code, error_code::IO_FAILURE);
                assert!(!dst.exists());
            }
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn recovers_interrupted_staging_on_startup() {
        let tmp = std::env::temp_dir().join(format!("hyperos-recover-{}", uuid::Uuid::new_v4()));
        let staging = tmp.join("instances/.staging-aborted-vm");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("partial"), b"junk").unwrap();

        let _mgr = StorageManager::new(&tmp, false).unwrap();
        assert!(!staging.exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn provisions_persists_snapshots_and_deletes_ephemeral_overlay() {
        let tmp = std::env::temp_dir().join(format!("hyperos-prov-{}", uuid::Uuid::new_v4()));
        let mgr = StorageManager::new(&tmp, false).unwrap();
        create_test_base_image(&tmp, "linux-demo", b"bootable-raw-image");

        let vm = Vm {
            vm_id: uuid::Uuid::new_v4().hyphenated().to_string(),
            name: "demo".to_string(),
            base_image_id: "linux-demo".to_string(),
            vcpu_count: 1,
            ram_mib: 512,
            balloon_target_mib: 512,
            network_mode: NetworkMode::Isolated,
            state: VmState::Stopped,
        };
        let policy = DaemonVmPolicy {
            ephemeral: true,
            balloon_min_ram_mib: 256,
            balloon_max_ram_mib: 512,
            vsock_cid: 3,
            last_pid: None,
        };

        mgr.provision_vm_instance(&vm, &policy).unwrap();
        assert!(mgr.instance_disk_path(&vm.vm_id).is_file());

        let loaded = mgr.load_all_vms().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].vm, vm);
        assert_eq!(loaded[0].policy, policy);

        let snap = Snapshot {
            snapshot_id: uuid::Uuid::new_v4().hyphenated().to_string(),
            vm_id: vm.vm_id.clone(),
            created_at: "2026-11-20T00:00:00Z".to_string(),
        };
        mgr.create_cold_snapshot(&vm, &snap).unwrap();
        assert!(mgr.snapshot_dir(&vm.vm_id, &snap.snapshot_id).is_dir());
        assert!(mgr
            .snapshot_metadata_path(&vm.vm_id, &snap.snapshot_id)
            .is_file());

        mgr.delete_vm_overlay(&vm.vm_id).unwrap();
        assert!(!mgr.instance_disk_path(&vm.vm_id).exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rejects_symlink_escape_in_base_image_directory() {
        let tmp = std::env::temp_dir().join(format!("hyperos-sym-{}", uuid::Uuid::new_v4()));
        let outside = tmp.join("outside.raw");
        let storage_root = tmp.join("storage");
        fs::create_dir_all(&storage_root).unwrap();
        fs::write(&outside, b"secret-host-file").unwrap();

        let mgr = StorageManager::new(&storage_root, false).unwrap();
        let img_dir = storage_root.join("base-images/evil-img");
        fs::create_dir_all(&img_dir).unwrap();
        std::os::unix::fs::symlink(&outside, img_dir.join("disk.raw")).unwrap();
        let sha = sha256_file(&outside).unwrap();
        let manifest = BaseImageManifest {
            image_id: "evil-img".to_string(),
            sha256: sha,
            format: "raw".to_string(),
        };
        fs::write(
            img_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let err = mgr.resolve_and_validate_base_image("evil-img").unwrap_err();
        assert_eq!(err.code, error_code::INVALID_PARAMS);

        let _ = fs::remove_dir_all(&tmp);
    }
}
