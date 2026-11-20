//! VMM backend abstraction and Unix domain socket JSON-RPC 2.0 client.

use crate::rpc::{
    decode_response_frame, encode_frame, method, CreateVmParams, EmptyParams, ListVmsResult,
    NetworkMode, RpcError, RpcRequest, RpcResponse, SetBalloonRamParams, SetNetworkModeParams,
    Snapshot, Vm, VmIdParams, DEFAULT_SOCKET_PATH, MAX_FRAME_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Write};
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
            Self::Transport(msg) => write!(f, "transport error: {msg}"),
            Self::Rpc(err) => write!(f, "{err}"),
        }
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
    fn set_balloon_ram(&mut self, vm_id: &str, ram_mib: u32) -> Result<Vm, ClientError>;
    fn set_network_mode(
        &mut self,
        vm_id: &str,
        network_mode: NetworkMode,
    ) -> Result<Vm, ClientError>;
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
                ClientError::Transport(format!(
                    "failed to connect to {}: {e}",
                    self.socket_path.display()
                ))
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
        let request = RpcRequest::new(req_id.clone(), method_name, params)
            .map_err(ClientError::Transport)?;
        let frame = encode_frame(&request).map_err(ClientError::Transport)?;

        match self.send_and_receive::<R>(&req_id, &frame) {
            Ok(res) => Ok(res),
            Err(ClientError::Transport(first_err)) if allow_reconnect_retry => {
                // Only safe for idempotent read-only queries such as `list_vms`.
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
        let result = (|| {
            let conn = self.ensure_connected()?;
            conn.writer
                .write_all(frame)
                .and_then(|_| conn.writer.flush())
                .map_err(|e| ClientError::Transport(format!("socket write failed: {e}")))?;

            if let Some(buffered) = conn.pending_responses.remove(expected_id) {
                let decoded = decode_response_frame::<R>(&buffered)
                    .map_err(ClientError::Transport)?;
                return decoded.into_result().map_err(ClientError::Rpc);
            }

            loop {
                let raw_frame = read_bounded_frame(&mut conn.reader)?;
                let envelope: RpcResponse<serde_json::Value> =
                    decode_response_frame(&raw_frame).map_err(ClientError::Transport)?;

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
fn read_bounded_frame<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, ClientError> {
    let mut buf = Vec::with_capacity(512);
    loop {
        let available = reader
            .fill_buf()
            .map_err(|e| ClientError::Transport(format!("socket read failed: {e}")))?;
        if available.is_empty() {
            return Err(ClientError::Transport(
                "daemon closed Unix socket connection".to_string(),
            ));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockVmmBackend;
    use crate::rpc::VmState;
    use std::os::unix::net::UnixListener;
    use std::thread;

    #[test]
    fn unix_socket_client_speaks_all_eight_methods_over_af_unix() {
        let sock_path = std::env::temp_dir().join(format!(
            "hyperos-vmm-test-{}.sock",
            uuid::Uuid::new_v4()
        ));
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
            })
            .unwrap();
        assert_eq!(created.state, VmState::Stopped);
        assert_eq!(created.balloon_target_mib, 1024);

        let net_changed = client
            .set_network_mode(&created.vm_id, NetworkMode::HostOnly)
            .unwrap();
        assert_eq!(net_changed.network_mode, NetworkMode::HostOnly);

        let snap = client.snapshot_vm(&created.vm_id).unwrap();
        assert_eq!(snap.vm_id, created.vm_id);

        let started = client.start_vm(&created.vm_id).unwrap();
        assert_eq!(started.state, VmState::Running);

        let ballooned = client.set_balloon_ram(&created.vm_id, 512).unwrap();
        assert_eq!(ballooned.balloon_target_mib, 512);

        let paused = client.pause_vm(&created.vm_id).unwrap();
        assert_eq!(paused.state, VmState::Paused);

        let stopped = client.stop_vm(&created.vm_id).unwrap();
        assert_eq!(stopped.state, VmState::Stopped);

        assert_eq!(client.list_vms().unwrap().len(), 1);

        drop(client);
        server.join().unwrap();
        let _ = std::fs::remove_file(&sock_path);
    }

    #[test]
    fn unix_socket_client_matches_out_of_order_response_ids() {
        let sock_path = std::env::temp_dir().join(format!(
            "hyperos-vmm-ooo-{}.sock",
            uuid::Uuid::new_v4()
        ));
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
            let matching = format!("{{\"jsonrpc\":\"2.0\",\"id\":\"{id}\",\"result\":{{\"vms\":[]}}}}\n");
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
