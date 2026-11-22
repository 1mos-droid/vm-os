//! Async Tokio AF_UNIX JSON-RPC 2.0 server with `SO_PEERCRED` authorization,
//! serialized startup lock, stale-socket cleanup, and strict 65,536-byte framing.

use crate::auth::{process_groups, PeerPolicy};
use crate::manager::VmManager;
use crate::rpc::{
    encode_error_frame, encode_success_frame, error_code, parse_request_line, RpcCall, RpcError,
    MAX_FRAME_BYTES,
};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{watch, Mutex};

/// Exclusive startup lock + bound Unix listener ensuring only one daemon owns the socket.
pub struct BoundRpcServer {
    socket_path: PathBuf,
    listener: UnixListener,
    policy: PeerPolicy,
    /// Held open for the lifetime of the server to keep the exclusive `flock`.
    _lock_file: File,
}

impl BoundRpcServer {
    /// Bind the Unix domain socket at `socket_path`.
    ///
    /// Without a control group the socket is `0600` in a `0700` parent. With one
    /// (`policy.gid`), both are group-owned by it: socket `0660`, parent `0750`.
    ///
    /// Serializes startup with `flock(LOCK_EX | LOCK_NB)` and only unlinks an existing
    /// socket file after proving it is a real Unix socket and no live daemon accepts connections.
    pub fn bind(socket_path: impl AsRef<Path>, policy: PeerPolicy) -> Result<Self, String> {
        let (dir_mode, sock_mode) = if policy.gid.is_some() {
            (0o750, 0o660)
        } else {
            (0o700, 0o600)
        };
        let socket_path = socket_path.as_ref().to_path_buf();
        let parent = socket_path
            .parent()
            .ok_or_else(|| "socket path must have a parent directory".to_string())?;

        if let Ok(meta) = fs::symlink_metadata(parent) {
            if meta.file_type().is_symlink() {
                return Err("socket parent directory must not be a symlink".to_string());
            }
        }
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create socket parent directory: {e}"))?;
        if let Some(gid) = policy.gid {
            std::os::unix::fs::chown(parent, None, Some(gid))
                .map_err(|e| format!("failed to set socket parent directory group: {e}"))?;
        }
        fs::set_permissions(parent, fs::Permissions::from_mode(dir_mode))
            .map_err(|e| format!("failed to set socket parent directory mode {dir_mode:o}: {e}"))?;

        let lock_path = parent.join("vmm-daemon.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| format!("failed to open daemon startup lock: {e}"))?;
        let _ = fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600));

        let flock_rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if flock_rc != 0 {
            return Err("another vmm-daemon instance already holds the startup lock".to_string());
        }

        // Only unlink an existing socket after proving it is a socket (never a symlink) and no live daemon owns it.
        match fs::symlink_metadata(&socket_path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err("target Unix socket path must not be a symlink".to_string());
                }
                if !meta.file_type().is_socket() {
                    return Err(
                        "existing path at target Unix socket is not a Unix domain socket"
                            .to_string(),
                    );
                }
                match std::os::unix::net::UnixStream::connect(&socket_path) {
                    Ok(_) => {
                        return Err(
                            "an active daemon is already listening on the target Unix socket"
                                .to_string(),
                        );
                    }
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                        ) =>
                    {
                        fs::remove_file(&socket_path)
                            .map_err(|e| format!("failed to remove stale Unix socket: {e}"))?;
                    }
                    Err(err) => {
                        return Err(format!(
                            "failed to probe existing Unix socket for liveness: {err}"
                        ));
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!("failed to inspect target Unix socket path: {err}"));
            }
        }

        let listener = UnixListener::bind(&socket_path)
            .map_err(|e| format!("failed to bind Unix socket: {e}"))?;
        if let Some(gid) = policy.gid {
            std::os::unix::fs::chown(&socket_path, None, Some(gid))
                .map_err(|e| format!("failed to set Unix socket group: {e}"))?;
        }
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(sock_mode))
            .map_err(|e| format!("failed to set Unix socket mode {sock_mode:o}: {e}"))?;

        Ok(Self {
            socket_path,
            listener,
            policy,
            _lock_file: lock_file,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run the async accept loop until `shutdown_rx` signals `true`.
    pub async fn serve(
        self,
        manager: Arc<VmManager>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> Result<(), String> {
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            if !authorize_peer(&stream, &self.policy) {
                                drop(stream);
                                continue;
                            }
                            let mgr = Arc::clone(&manager);
                            tokio::spawn(async move {
                                handle_client(stream, mgr).await;
                            });
                        }
                        Err(err) => {
                            eprintln!("vmm-daemon: accept error: {err}");
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

impl Drop for BoundRpcServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// Enforce `SO_PEERCRED` authorization (`ARCHITECTURE.md`, socket authorization).
/// Supplementary groups are read from `/proc/<pid>/status` only when the UID and
/// primary GID alone do not decide.
fn authorize_peer(stream: &UnixStream, policy: &PeerPolicy) -> bool {
    let Ok(cred) = stream.peer_cred() else {
        return false;
    };
    if policy.allows(cred.uid(), cred.gid(), &[]) {
        return true;
    }
    match (policy.gid, cred.pid()) {
        (Some(_), Some(pid)) => policy.allows(cred.uid(), cred.gid(), &process_groups(pid)),
        _ => false,
    }
}

enum FrameRead {
    Frame(Vec<u8>),
    Eof,
    Oversized,
    IoErr,
}

/// Read a single LF-terminated frame strictly bounded to `MAX_FRAME_BYTES` (65536 bytes).
async fn read_bounded_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> FrameRead {
    let mut buf = Vec::with_capacity(512);
    loop {
        let available = match reader.fill_buf().await {
            Ok(bytes) => bytes,
            Err(_) => return FrameRead::IoErr,
        };
        if available.is_empty() {
            return if buf.is_empty() {
                FrameRead::Eof
            } else {
                FrameRead::Oversized
            };
        }

        if let Some(nl_pos) = available.iter().position(|&b| b == b'\n') {
            let take = nl_pos + 1;
            if buf.len() + take > MAX_FRAME_BYTES {
                return FrameRead::Oversized;
            }
            buf.extend_from_slice(&available[..take]);
            reader.consume(take);
            return FrameRead::Frame(buf);
        }

        let len = available.len();
        if buf.len() + len >= MAX_FRAME_BYTES {
            return FrameRead::Oversized;
        }
        buf.extend_from_slice(available);
        reader.consume(len);
    }
}

async fn handle_client(stream: UnixStream, manager: Arc<VmManager>) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let writer = Arc::new(Mutex::new(write_half));
    let outstanding_ids: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    loop {
        let frame = match read_bounded_frame(&mut reader).await {
            FrameRead::Frame(f) => f,
            FrameRead::Eof | FrameRead::Oversized | FrameRead::IoErr => {
                // Per ARCHITECTURE.md: close oversized or broken connections without parsing further.
                break;
            }
        };

        let line_bytes = &frame[..frame.len() - 1];
        let line_str = match std::str::from_utf8(line_bytes) {
            Ok(s) => s,
            Err(_) => {
                let err_frame = encode_error_frame(
                    None,
                    &RpcError::new(error_code::PARSE_ERROR, "invalid UTF-8 frame"),
                );
                if write_frame(&writer, &err_frame).await.is_err() {
                    break;
                }
                continue;
            }
        };

        let parsed = match parse_request_line(line_str) {
            Ok(req) => req,
            Err(parse_err) => {
                let err_frame = encode_error_frame(parse_err.id.as_deref(), &parse_err.error);
                if write_frame(&writer, &err_frame).await.is_err() {
                    break;
                }
                continue;
            }
        };

        // Enforce unique ID among outstanding requests on this connection.
        let inserted = {
            let mut ids = outstanding_ids.lock().await;
            ids.insert(parsed.id.clone())
        };
        if !inserted {
            let err_frame = encode_error_frame(
                Some(&parsed.id),
                &RpcError::new(
                    error_code::INVALID_REQUEST,
                    "duplicate request id among outstanding requests",
                ),
            );
            if write_frame(&writer, &err_frame).await.is_err() {
                break;
            }
            continue;
        }

        let mgr = Arc::clone(&manager);
        let writer_clone = Arc::clone(&writer);
        let ids_clone = Arc::clone(&outstanding_ids);
        tokio::spawn(async move {
            let response_bytes = dispatch_call(&mgr, &parsed.id, parsed.call).await;
            let _ = write_frame(&writer_clone, &response_bytes).await;
            let mut ids = ids_clone.lock().await;
            ids.remove(&parsed.id);
        });
    }
}

async fn write_frame(
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    bytes: &[u8],
) -> io::Result<()> {
    let mut w = writer.lock().await;
    w.write_all(bytes).await?;
    w.flush().await
}

async fn dispatch_call(manager: &Arc<VmManager>, id: &str, call: RpcCall) -> Vec<u8> {
    match call {
        RpcCall::CreateVm(params) => match manager.create_vm(params).await {
            Ok(vm) => encode_or_error(id, &vm),
            Err(err) => encode_error_frame(Some(id), &err),
        },
        RpcCall::StartVm(p) => match manager.start_vm(&p.vm_id).await {
            Ok(vm) => encode_or_error(id, &vm),
            Err(err) => encode_error_frame(Some(id), &err),
        },
        RpcCall::StopVm(p) => match manager.stop_vm(&p.vm_id).await {
            Ok(vm) => encode_or_error(id, &vm),
            Err(err) => encode_error_frame(Some(id), &err),
        },
        RpcCall::PauseVm(p) => match manager.pause_vm(&p.vm_id).await {
            Ok(vm) => encode_or_error(id, &vm),
            Err(err) => encode_error_frame(Some(id), &err),
        },
        RpcCall::SnapshotVm(p) => match manager.snapshot_vm(&p.vm_id).await {
            Ok(snap) => encode_or_error(id, &snap),
            Err(err) => encode_error_frame(Some(id), &err),
        },
        RpcCall::SetBalloonRam(p) => match manager.set_balloon_ram(&p.vm_id, p.ram_mib).await {
            Ok(vm) => encode_or_error(id, &vm),
            Err(err) => encode_error_frame(Some(id), &err),
        },
        RpcCall::SetNetworkMode(p) => {
            match manager.set_network_mode(&p.vm_id, p.network_mode).await {
                Ok(vm) => encode_or_error(id, &vm),
                Err(err) => encode_error_frame(Some(id), &err),
            }
        }
        RpcCall::ListVms(_) => respond(id, manager.list_vms().await),
        RpcCall::ListSnapshots(p) => respond(id, manager.list_snapshots(&p.vm_id).await),
        RpcCall::RestoreSnapshot(p) => {
            respond(id, manager.restore_snapshot(&p.vm_id, &p.snapshot_id).await)
        }
        RpcCall::DeleteSnapshot(p) => {
            respond(id, manager.delete_snapshot(&p.vm_id, &p.snapshot_id).await)
        }
        RpcCall::StartCapture(p) => respond(id, manager.start_capture(&p.vm_id).await),
        RpcCall::StopCapture(p) => respond(id, manager.stop_capture(&p.vm_id).await),
        RpcCall::ListCaptures(p) => respond(id, manager.list_captures(&p.vm_id).await),
        RpcCall::DeleteCapture(p) => {
            respond(id, manager.delete_capture(&p.vm_id, &p.capture_id).await)
        }
    }
}

fn respond<T: serde::Serialize>(id: &str, result: Result<T, RpcError>) -> Vec<u8> {
    match result {
        Ok(value) => encode_or_error(id, &value),
        Err(err) => encode_error_frame(Some(id), &err),
    }
}

fn encode_or_error<T: serde::Serialize>(id: &str, value: &T) -> Vec<u8> {
    match encode_success_frame(id, value) {
        Ok(bytes) => bytes,
        Err(err) => encode_error_frame(Some(id), &err),
    }
}
