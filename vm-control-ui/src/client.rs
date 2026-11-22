//! VMM backend abstraction and Unix domain socket JSON-RPC 2.0 client.

use crate::rpc::{
    decode_response_frame, encode_frame, method, Capture, CaptureIdParams, CreateVmParams,
    EmptyParams, EmptyResult, ListCapturesResult, ListSnapshotsResult, ListVmsResult, NetworkMode,
    RpcError, RpcRequest, RpcResponse, SetBalloonRamParams, SetNetworkModeParams, Snapshot,
    SnapshotIdParams, Vm, VmIdParams, DEFAULT_SOCKET_PATH, MAX_FRAME_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Errors returned by a `VmmBackend` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// Transport / connection / framing error.
    Transport(String),
    /// Structured JSON-RPC 2.0 error returned by the daemon.
    Rpc(RpcError),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(msg) => f.write_str(msg),
            Self::Rpc(err) => write!(f, "{err}"),
        }
    }
}

/// Produce a clear, actionable message when `UnixStream::connect` fails.
pub fn classify_connect_error(socket_path: &Path, err: &io::Error) -> String {
    let path = socket_path.display();
    match err.kind() {
        io::ErrorKind::PermissionDenied => format!(
            "Permission denied connecting to {path}. Ensure your user is a member of the `hyperos` group (`id -nG`; socket mode is root:hyperos 0660 under /run/hyperos 0750), or pass --mock for preview mode."
        ),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => format!(
            "Cannot connect to vmm-daemon at {path} ({err}). vmm-daemon may not be running — check `sv status /run/service/vmm-daemon` or run with --mock."
        ),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => format!(
            "Timed out connecting to vmm-daemon at {path}. Check `sv status /run/service/vmm-daemon`."
        ),
        _ => format!("Failed to connect to {path}: {err}"),
    }
}

/// Produce a clear message when the peer closes the socket connection without a complete frame.
pub fn classify_peer_closed_error(socket_path: &Path, received_any_response: bool) -> String {
    let path = socket_path.display();
    if received_any_response {
        format!(
            "vmm-daemon closed {path} unexpectedly (daemon may have restarted or rejected an oversized frame)."
        )
    } else {
        format!(
            "vmm-daemon closed {path} immediately without responding — access denied by SO_PEERCRED (your user must be UID 0 or a member of the `hyperos` group), or the daemon exited during handshake."
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
            "Timed out waiting for vmm-daemon on {path} during {operation}. Do not blindly retry mutations; click Refresh (list_vms) to verify current VM state."
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

impl std::error::Error for ClientError {}

impl From<RpcError> for ClientError {
    fn from(value: RpcError) -> Self {
        Self::Rpc(value)
    }
}

/// Abstract client interface implemented by both `UnixSocketClient` and `MockVmmBackend`.
pub trait VmmBackend {
    fn backend_label(&self) -> String;
    fn is_connected(&self) -> bool;

    fn list_vms(&mut self) -> Result<Vec<Vm>, ClientError>;
    fn create_vm(&mut self, params: CreateVmParams) -> Result<Vm, ClientError>;
    fn start_vm(&mut self, vm_id: &str) -> Result<Vm, ClientError>;
    fn stop_vm(&mut self, vm_id: &str) -> Result<Vm, ClientError>;
    fn pause_vm(&mut self, vm_id: &str) -> Result<Vm, ClientError>;
    fn snapshot_vm(&mut self, vm_id: &str) -> Result<Snapshot, ClientError>;
    fn list_snapshots(&mut self, vm_id: &str) -> Result<Vec<Snapshot>, ClientError>;
    fn restore_snapshot(&mut self, vm_id: &str, snapshot_id: &str) -> Result<Vm, ClientError>;
    fn delete_snapshot(&mut self, vm_id: &str, snapshot_id: &str) -> Result<(), ClientError>;
    fn set_balloon_ram(&mut self, vm_id: &str, ram_mib: u32) -> Result<Vm, ClientError>;
    fn set_network_mode(
        &mut self,
        vm_id: &str,
        network_mode: NetworkMode,
    ) -> Result<Vm, ClientError>;
    fn start_capture(&mut self, vm_id: &str) -> Result<Capture, ClientError>;
    fn stop_capture(&mut self, vm_id: &str) -> Result<Capture, ClientError>;
    fn list_captures(&mut self, vm_id: &str) -> Result<Vec<Capture>, ClientError>;
    fn delete_capture(&mut self, vm_id: &str, capture_id: &str) -> Result<(), ClientError>;

    /// Notify an in-memory mock backend that a Template Store image has been provisioned.
    fn register_template_image(&mut self, _image_id: &str) {}

    /// Advance simulated capture byte counters in `--mock` mode (no-op on live daemon clients).
    fn tick_mock_captures(&mut self) {}
}

/// Persistent AF_UNIX / SOCK_STREAM JSON-RPC 2.0 client for `/run/hyperos/vmm.sock`.
///
/// Honors `ARCHITECTURE.md`:
/// - One UTF-8 JSON object + LF per frame, max 65536 bytes including LF.
/// - Unique request IDs per connection (`ui-<counter>`).
/// - Matches responses by `id` even when responses arrive out of order.
/// - Reconnects on read-only `list_vms` after disconnect; never blindly retries
///   mutating operations (`create_vm`, `start_vm`, `stop_vm`, `pause_vm`,
///   `snapshot_vm`, `set_balloon_ram`, `set_network_mode`).
pub struct UnixSocketClient {
    socket_path: PathBuf,
    timeout: Duration,
    connection: Option<ConnectionState>,
    next_id: AtomicU64,
}

struct ConnectionState {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// True once at least one valid JSON-RPC frame was received on this connection.
    /// Helps distinguish initial SO_PEERCRED rejection from later daemon restarts.
    has_received_response: bool,
    /// Buffered out-of-order response frames keyed by response `id`.
    pending_responses: HashMap<String, Vec<u8>>,
}

impl UnixSocketClient {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            timeout: Duration::from_secs(10),
            connection: None,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn default_socket() -> Self {
        Self::new(DEFAULT_SOCKET_PATH)
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn next_request_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{n}")
    }

    fn ensure_connected(&mut self) -> Result<&mut ConnectionState, ClientError> {
        if self.connection.is_none() {
            let stream = UnixStream::connect(&self.socket_path).map_err(|e| {
                ClientError::Transport(classify_connect_error(&self.socket_path, &e))
            })?;
            stream
                .set_read_timeout(Some(self.timeout))
                .map_err(|e| ClientError::Transport(format!("failed to set read timeout: {e}")))?;
            stream
                .set_write_timeout(Some(self.timeout))
                .map_err(|e| ClientError::Transport(format!("failed to set write timeout: {e}")))?;
            let writer = stream
                .try_clone()
                .map_err(|e| ClientError::Transport(format!("failed to clone Unix stream: {e}")))?;
            self.connection = Some(ConnectionState {
                reader: BufReader::new(stream),
                writer,
                has_received_response: false,
                pending_responses: HashMap::new(),
            });
        }
        Ok(self.connection.as_mut().expect("connection initialized"))
    }

    fn drop_connection(&mut self) {
        self.connection = None;
    }

    fn call<P, R>(
        &mut self,
        id_prefix: &str,
        method_name: &'static str,
        params: P,
        allow_reconnect_retry: bool,
    ) -> Result<R, ClientError>
    where
        P: Serialize,
        R: for<'de> Deserialize<'de>,
    {
        let req_id = self.next_request_id(id_prefix);
        let request =
            RpcRequest::new(req_id.clone(), method_name, params).map_err(ClientError::Transport)?;
        let frame = encode_frame(&request).map_err(ClientError::Transport)?;
        let had_healthy_conn = self
            .connection
            .as_ref()
            .is_some_and(|conn| conn.has_received_response);

        match self.send_and_receive::<R>(&req_id, &frame) {
            Ok(res) => Ok(res),
            Err(ClientError::Transport(first_err)) if allow_reconnect_retry && had_healthy_conn => {
                // Only retry read-only queries after an established connection drops.
                // Never mask permission/daemon-offline/SO_PEERCRED handshake errors.
                self.drop_connection();
                self.send_and_receive::<R>(&req_id, &frame)
                    .map_err(|_| ClientError::Transport(first_err))
            }
            Err(err) => Err(err),
        }
    }

    fn send_and_receive<R>(&mut self, expected_id: &str, frame: &[u8]) -> Result<R, ClientError>
    where
        R: for<'de> Deserialize<'de>,
    {
        let socket_path = self.socket_path.clone();
        let result = (|| {
            let conn = self.ensure_connected()?;
            let received_any = conn.has_received_response;
            conn.writer
                .write_all(frame)
                .and_then(|_| conn.writer.flush())
                .map_err(|e| {
                    ClientError::Transport(classify_io_error(
                        &socket_path,
                        "socket write",
                        &e,
                        received_any,
                    ))
                })?;

            if let Some(buffered) = conn.pending_responses.remove(expected_id) {
                let decoded =
                    decode_response_frame::<R>(&buffered).map_err(ClientError::Transport)?;
                conn.has_received_response = true;
                return decoded.into_result().map_err(ClientError::Rpc);
            }

            loop {
                let received_any = conn.has_received_response;
                let raw_frame = read_bounded_frame(&mut conn.reader, &socket_path, received_any)?;
                let envelope: RpcResponse<serde_json::Value> =
                    decode_response_frame(&raw_frame).map_err(ClientError::Transport)?;
                conn.has_received_response = true;

                match envelope.id() {
                    Some(id) if id == expected_id => {
                        let typed: RpcResponse<R> =
                            decode_response_frame(&raw_frame).map_err(ClientError::Transport)?;
                        return typed.into_result().map_err(ClientError::Rpc);
                    }
                    None => {
                        // Null ID error frame (unrecoverable request ID on daemon side).
                        return match envelope {
                            RpcResponse::Error { error, .. } => Err(ClientError::Rpc(error)),
                            RpcResponse::Success { .. } => Err(ClientError::Transport(
                                "protocol violation: success response with null id".to_string(),
                            )),
                        };
                    }
                    Some(other_id) => {
                        // Out-of-order response for another request ID; stash and continue.
                        conn.pending_responses
                            .insert(other_id.to_string(), raw_frame);
                    }
                }
            }
        })();

        if matches!(result, Err(ClientError::Transport(_))) {
            // Close broken/oversized connections without parsing further.
            self.drop_connection();
        }
        result
    }
}

/// Read a single LF-terminated frame up to `MAX_FRAME_BYTES` (65536 bytes including LF).
/// Never allocates beyond `MAX_FRAME_BYTES` on malicious or malformed streams.
fn read_bounded_frame<R: BufRead>(
    reader: &mut R,
    socket_path: &Path,
    received_any_response: bool,
) -> Result<Vec<u8>, ClientError> {
    let mut buf = Vec::with_capacity(512);
    loop {
        let available = reader.fill_buf().map_err(|e| {
            ClientError::Transport(classify_io_error(
                socket_path,
                "socket read",
                &e,
                received_any_response,
            ))
        })?;
        if available.is_empty() {
            return Err(ClientError::Transport(classify_peer_closed_error(
                socket_path,
                received_any_response,
            )));
        }

        if let Some(nl_pos) = available.iter().position(|&b| b == b'\n') {
            let take = nl_pos + 1;
            if buf.len() + take > MAX_FRAME_BYTES {
                return Err(ClientError::Transport(format!(
                    "response frame exceeds maximum {} bytes",
                    MAX_FRAME_BYTES
                )));
            }
            buf.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(buf);
        }

        let len = available.len();
        if buf.len() + len >= MAX_FRAME_BYTES {
            return Err(ClientError::Transport(format!(
                "response frame exceeds maximum {} bytes without LF",
                MAX_FRAME_BYTES
            )));
        }
        buf.extend_from_slice(available);
        reader.consume(len);
    }
}

impl VmmBackend for UnixSocketClient {
    fn backend_label(&self) -> String {
        self.socket_path.display().to_string()
    }

    fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    fn list_vms(&mut self) -> Result<Vec<Vm>, ClientError> {
        let res: ListVmsResult = self.call("list", method::LIST_VMS, EmptyParams {}, true)?;
        Ok(res.vms)
    }

    fn create_vm(&mut self, params: CreateVmParams) -> Result<Vm, ClientError> {
        self.call("create", method::CREATE_VM, params, false)
    }

    fn start_vm(&mut self, vm_id: &str) -> Result<Vm, ClientError> {
        self.call(
            "start",
            method::START_VM,
            VmIdParams {
                vm_id: vm_id.to_string(),
            },
            false,
        )
    }

    fn stop_vm(&mut self, vm_id: &str) -> Result<Vm, ClientError> {
        self.call(
            "stop",
            method::STOP_VM,
            VmIdParams {
                vm_id: vm_id.to_string(),
            },
            false,
        )
    }

    fn pause_vm(&mut self, vm_id: &str) -> Result<Vm, ClientError> {
        self.call(
            "pause",
            method::PAUSE_VM,
            VmIdParams {
                vm_id: vm_id.to_string(),
            },
            false,
        )
    }

    fn snapshot_vm(&mut self, vm_id: &str) -> Result<Snapshot, ClientError> {
        self.call(
            "snap",
            method::SNAPSHOT_VM,
            VmIdParams {
                vm_id: vm_id.to_string(),
            },
            false,
        )
    }

    fn list_snapshots(&mut self, vm_id: &str) -> Result<Vec<Snapshot>, ClientError> {
        let res: ListSnapshotsResult = self.call(
            "lsnap",
            method::LIST_SNAPSHOTS,
            VmIdParams {
                vm_id: vm_id.to_string(),
            },
            true,
        )?;
        Ok(res.snapshots)
    }

    fn restore_snapshot(&mut self, vm_id: &str, snapshot_id: &str) -> Result<Vm, ClientError> {
        self.call(
            "rsnap",
            method::RESTORE_SNAPSHOT,
            SnapshotIdParams {
                vm_id: vm_id.to_string(),
                snapshot_id: snapshot_id.to_string(),
            },
            false,
        )
    }

    fn delete_snapshot(&mut self, vm_id: &str, snapshot_id: &str) -> Result<(), ClientError> {
        let _: EmptyResult = self.call(
            "dsnap",
            method::DELETE_SNAPSHOT,
            SnapshotIdParams {
                vm_id: vm_id.to_string(),
                snapshot_id: snapshot_id.to_string(),
            },
            false,
        )?;
        Ok(())
    }

    fn set_balloon_ram(&mut self, vm_id: &str, ram_mib: u32) -> Result<Vm, ClientError> {
        self.call(
            "balloon",
            method::SET_BALLOON_RAM,
            SetBalloonRamParams {
                vm_id: vm_id.to_string(),
                ram_mib,
            },
            false,
        )
    }

    fn set_network_mode(
        &mut self,
        vm_id: &str,
        network_mode: NetworkMode,
    ) -> Result<Vm, ClientError> {
        self.call(
            "net",
            method::SET_NETWORK_MODE,
            SetNetworkModeParams {
                vm_id: vm_id.to_string(),
                network_mode,
            },
            false,
        )
    }

    fn start_capture(&mut self, vm_id: &str) -> Result<Capture, ClientError> {
        self.call(
            "capstart",
            method::START_CAPTURE,
            VmIdParams {
                vm_id: vm_id.to_string(),
            },
            false,
        )
    }

    fn stop_capture(&mut self, vm_id: &str) -> Result<Capture, ClientError> {
        self.call(
            "capstop",
            method::STOP_CAPTURE,
            VmIdParams {
                vm_id: vm_id.to_string(),
            },
            false,
        )
    }

    fn list_captures(&mut self, vm_id: &str) -> Result<Vec<Capture>, ClientError> {
        let res: ListCapturesResult = self.call(
            "caplist",
            method::LIST_CAPTURES,
            VmIdParams {
                vm_id: vm_id.to_string(),
            },
            true,
        )?;
        Ok(res.captures)
    }

    fn delete_capture(&mut self, vm_id: &str, capture_id: &str) -> Result<(), ClientError> {
        let _: EmptyResult = self.call(
            "capdel",
            method::DELETE_CAPTURE,
            CaptureIdParams {
                vm_id: vm_id.to_string(),
                capture_id: capture_id.to_string(),
            },
            false,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockVmmBackend;
    use crate::rpc::VmState;
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

        let rejected = classify_peer_closed_error(sock, false);
        assert!(rejected.contains("SO_PEERCRED"));
        assert!(rejected.contains("hyperos"));

        let restarted = classify_peer_closed_error(sock, true);
        assert!(restarted.contains("restarted"));

        let timed_out = classify_io_error(
            sock,
            "socket read",
            &io::Error::new(io::ErrorKind::TimedOut, "timed out"),
            true,
        );
        assert!(timed_out.contains("Timed out"));
        assert!(timed_out.contains("Refresh"));
    }

    #[test]
    fn immediate_peer_close_reports_so_peercred_without_blind_retry() {
        let sock_path =
            std::env::temp_dir().join(format!("hyperos-peercred-{}.sock", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        let accept_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accept_count_srv = std::sync::Arc::clone(&accept_count);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            accept_count_srv.fetch_add(1, Ordering::SeqCst);
            // Simulate daemon rejecting peer via SO_PEERCRED: close immediately with no response.
            drop(stream);
        });

        let mut client = UnixSocketClient::new(&sock_path);
        let err = client.list_vms().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("SO_PEERCRED"), "unexpected error: {msg}");
        assert!(msg.contains("hyperos"), "unexpected error: {msg}");
        assert_eq!(accept_count.load(Ordering::SeqCst), 1);

        server.join().unwrap();
        let _ = std::fs::remove_file(&sock_path);
    }

    #[test]
    fn nonroot_0660_socket_connects_and_retries_only_after_healthy_session_drops() {
        let sock_path =
            std::env::temp_dir().join(format!("hyperos-0660-{}.sock", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();
        std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o660)).unwrap();

        let server = thread::spawn(move || {
            // Connection 1: serve one list_vms call, then drop connection when client next reads.
            let (stream1, _) = listener.accept().unwrap();
            let mut writer1 = stream1.try_clone().unwrap();
            let mut reader1 = BufReader::new(stream1);
            let mut line = String::new();
            reader1.read_line(&mut line).unwrap();
            let req1: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id1 = req1["id"].as_str().unwrap();
            let resp1 =
                format!("{{\"jsonrpc\":\"2.0\",\"id\":\"{id1}\",\"result\":{{\"vms\":[]}}}}\n");
            writer1.write_all(resp1.as_bytes()).unwrap();
            writer1.flush().unwrap();
            drop(writer1);
            drop(reader1);

            // Connection 2: accept reconnect retry for the second read-only list_vms.
            let (stream2, _) = listener.accept().unwrap();
            let mut writer2 = stream2.try_clone().unwrap();
            let mut reader2 = BufReader::new(stream2);
            let mut line2 = String::new();
            reader2.read_line(&mut line2).unwrap();
            let req2: serde_json::Value = serde_json::from_str(&line2).unwrap();
            let id2 = req2["id"].as_str().unwrap();
            let resp2 =
                format!("{{\"jsonrpc\":\"2.0\",\"id\":\"{id2}\",\"result\":{{\"vms\":[]}}}}\n");
            writer2.write_all(resp2.as_bytes()).unwrap();
            writer2.flush().unwrap();
        });

        let mut client = UnixSocketClient::new(&sock_path);
        assert!(client.list_vms().unwrap().is_empty());
        // Second call hits EOF on closed stream1 and transparently reconnects because stream1 was healthy.
        assert!(client.list_vms().unwrap().is_empty());

        drop(client);
        server.join().unwrap();
        let _ = std::fs::remove_file(&sock_path);
    }

    #[test]
    fn unix_socket_client_speaks_all_fifteen_methods_over_af_unix() {
        use crate::rpc::{error_code, CaptureState};

        let sock_path =
            std::env::temp_dir().join(format!("hyperos-vmm-test-{}.sock", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = thread::spawn(move || {
            let mut mock = MockVmmBackend::empty(4096, 4);
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            loop {
                line.clear();
                let n = reader.read_until(b'\n', &mut line).unwrap();
                if n == 0 {
                    break;
                }
                let resp = mock.handle_raw_frame(&line);
                writer.write_all(&resp).unwrap();
                writer.flush().unwrap();
            }
        });

        let mut client = UnixSocketClient::new(&sock_path);
        assert!(client.list_vms().unwrap().is_empty());

        let created = client
            .create_vm(CreateVmParams {
                name: "test-alpine".to_string(),
                base_image_id: "alpine-3-21-virtio".to_string(),
                vcpu_count: 2,
                ram_mib: 1024,
                network_mode: NetworkMode::Isolated,
                disposable: false,
            })
            .unwrap();
        assert_eq!(created.state, VmState::Stopped);
        assert!(!created.disposable);
        assert_eq!(created.balloon_target_mib, 1024);

        let net_changed = client
            .set_network_mode(&created.vm_id, NetworkMode::Nat)
            .unwrap();
        assert_eq!(net_changed.network_mode, NetworkMode::Nat);

        let snap = client.snapshot_vm(&created.vm_id).unwrap();
        assert_eq!(snap.vm_id, created.vm_id);
        let snaps = client.list_snapshots(&created.vm_id).unwrap();
        assert_eq!(snaps, vec![snap.clone()]);

        let restored = client
            .restore_snapshot(&created.vm_id, &snap.snapshot_id)
            .unwrap();
        assert_eq!(restored.vm_id, created.vm_id);

        let started = client.start_vm(&created.vm_id).unwrap();
        assert_eq!(started.state, VmState::Running);

        let cap = client.start_capture(&created.vm_id).unwrap();
        assert_eq!(cap.state, CaptureState::Running);
        let caps = client.list_captures(&created.vm_id).unwrap();
        assert_eq!(caps.len(), 1);
        let stopped_cap = client.stop_capture(&created.vm_id).unwrap();
        assert_eq!(stopped_cap.state, CaptureState::Stopped);
        client
            .delete_capture(&created.vm_id, &cap.capture_id)
            .unwrap();
        assert!(client.list_captures(&created.vm_id).unwrap().is_empty());

        let ballooned = client.set_balloon_ram(&created.vm_id, 512).unwrap();
        assert_eq!(ballooned.balloon_target_mib, 512);

        let paused = client.pause_vm(&created.vm_id).unwrap();
        assert_eq!(paused.state, VmState::Paused);

        // delete_snapshot succeeds in any state
        client
            .delete_snapshot(&created.vm_id, &snap.snapshot_id)
            .unwrap();
        assert!(client.list_snapshots(&created.vm_id).unwrap().is_empty());

        let stopped = client.stop_vm(&created.vm_id).unwrap();
        assert_eq!(stopped.state, VmState::Stopped);

        // Disposable VM rejects snapshots (-32002), and isolated running VM rejects capture (-32004).
        let disp = client
            .create_vm(CreateVmParams {
                name: "disp-iso".to_string(),
                base_image_id: "alpine-3-21-virtio".to_string(),
                vcpu_count: 1,
                ram_mib: 512,
                network_mode: NetworkMode::Isolated,
                disposable: true,
            })
            .unwrap();
        assert!(matches!(
            client.snapshot_vm(&disp.vm_id),
            Err(ClientError::Rpc(RpcError {
                code: error_code::INVALID_STATE,
                ..
            }))
        ));
        client.start_vm(&disp.vm_id).unwrap();
        assert!(matches!(
            client.start_capture(&disp.vm_id),
            Err(ClientError::Rpc(RpcError {
                code: error_code::UNAVAILABLE_CAPABILITY,
                ..
            }))
        ));

        assert_eq!(client.list_vms().unwrap().len(), 2);

        drop(client);
        server.join().unwrap();
        let _ = std::fs::remove_file(&sock_path);
    }

    #[test]
    fn unix_socket_client_matches_out_of_order_response_ids() {
        let sock_path =
            std::env::temp_dir().join(format!("hyperos-vmm-ooo-{}.sock", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = req["id"].as_str().unwrap();

            // Send an unrelated response first, then the matching one.
            let ooo = "{\"jsonrpc\":\"2.0\",\"id\":\"other-99\",\"result\":{\"vms\":[]}}\n";
            let matching =
                format!("{{\"jsonrpc\":\"2.0\",\"id\":\"{id}\",\"result\":{{\"vms\":[]}}}}\n");
            writer.write_all(ooo.as_bytes()).unwrap();
            writer.write_all(matching.as_bytes()).unwrap();
            writer.flush().unwrap();
        });

        let mut client = UnixSocketClient::new(&sock_path);
        let vms = client.list_vms().unwrap();
        assert!(vms.is_empty());

        drop(client);
        server.join().unwrap();
        let _ = std::fs::remove_file(&sock_path);
    }
}
