//! Exact JSON-RPC 2.0 wire types, strict request parser, and framing matching `ARCHITECTURE.md`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// Default Unix domain socket path (`ARCHITECTURE.md`).
pub const DEFAULT_SOCKET_PATH: &str = "/run/hyperos/vmm.sock";

/// Default runtime directory for per-VM crosvm sockets and jail pivot roots.
pub const DEFAULT_RUNTIME_DIR: &str = "/run/hyperos";

/// Default persistent Btrfs storage root (`ARCHITECTURE.md`).
pub const DEFAULT_STORAGE_ROOT: &str = "/var/lib/hyperos";

/// Maximum JSON-RPC frame size in bytes including trailing LF (`\n`).
pub const MAX_FRAME_BYTES: usize = 65_536;

/// MiB definition used throughout HyperOS (`ARCHITECTURE.md`).
pub const MIB_BYTES: u64 = 1_048_576;

/// Reserved host RAM subtracted from `MemTotal` for VM admission control (`ARCHITECTURE.md`).
pub const HOST_RAM_RESERVE_MIB: u64 = 768;

/// Allowed VM RAM limits in MiB (`create_vm.ram_mib`).
pub const MIN_VM_RAM_MIB: u32 = 128;
pub const MAX_VM_RAM_MIB: u32 = 3072;

/// Allowed vCPU bounds (`create_vm.vcpu_count`).
pub const MIN_VCPU_COUNT: u32 = 1;
pub const MAX_VCPU_COUNT: u32 = 64;

/// JSON-RPC protocol version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// Exact error codes defined in `ARCHITECTURE.md`.
pub mod error_code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const NOT_FOUND: i64 = -32001;
    pub const INVALID_STATE: i64 = -32002;
    pub const INSUFFICIENT_RESOURCES: i64 = -32003;
    pub const UNAVAILABLE_CAPABILITY: i64 = -32004;
    pub const IO_FAILURE: i64 = -32005;
}

/// Exact RPC method names defined in `ARCHITECTURE.md`.
pub mod method {
    pub const CREATE_VM: &str = "create_vm";
    pub const START_VM: &str = "start_vm";
    pub const STOP_VM: &str = "stop_vm";
    pub const PAUSE_VM: &str = "pause_vm";
    pub const SNAPSHOT_VM: &str = "snapshot_vm";
    pub const SET_BALLOON_RAM: &str = "set_balloon_ram";
    pub const SET_NETWORK_MODE: &str = "set_network_mode";
    pub const LIST_VMS: &str = "list_vms";
}

/// Validate a JSON-RPC request ID: 1-64 ASCII letters, digits, `_`, or `-`.
pub fn is_valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Validate an `image_id`: 1-64 ASCII letters, digits, `_`, or `-`.
pub fn is_valid_image_id(id: &str) -> bool {
    is_valid_request_id(id)
}

/// Validate a canonical lowercase UUID: `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
pub fn is_valid_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if b != b'-' {
                    return false;
                }
            }
            _ => {
                if !matches!(b, b'0'..=b'9' | b'a'..=b'f') {
                    return false;
                }
            }
        }
    }
    true
}

/// Validate a VM `name`: 1-64 Unicode code points without control characters.
pub fn is_valid_vm_name(name: &str) -> bool {
    let mut count = 0usize;
    for ch in name.chars() {
        if ch.is_control() {
            return false;
        }
        count += 1;
        if count > 64 {
            return false;
        }
    }
    count >= 1
}

/// Validate a 64-character lowercase hex SHA-256 digest.
pub fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Translate `(maximum_ram_mib, desired_ram_mib)` to balloon bytes per `ARCHITECTURE.md`:
/// `balloon_bytes = (maximum - desired) * 1048576`.
pub fn balloon_bytes(max_ram_mib: u32, desired_ram_mib: u32) -> Result<u64, RpcError> {
    if desired_ram_mib < MIN_VM_RAM_MIB || desired_ram_mib > max_ram_mib {
        return Err(RpcError::new(
            error_code::INVALID_PARAMS,
            format!("ram_mib must be between {MIN_VM_RAM_MIB} and {max_ram_mib} MiB"),
        ));
    }
    Ok(u64::from(max_ram_mib - desired_ram_mib) * MIB_BYTES)
}

/// Exact `network_mode` values from `ARCHITECTURE.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkMode {
    #[serde(rename = "isolated")]
    Isolated,
    #[serde(rename = "nat")]
    Nat,
    #[serde(rename = "host-only")]
    HostOnly,
}

impl NetworkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Isolated => "isolated",
            Self::Nat => "nat",
            Self::HostOnly => "host-only",
        }
    }
}

impl fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Exact VM `state` values from `ARCHITECTURE.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmState {
    #[serde(rename = "stopped")]
    Stopped,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "paused")]
    Paused,
    #[serde(rename = "failed")]
    Failed,
}

impl VmState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for VmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Wire VM object (`ARCHITECTURE.md` — all 8 keys required, no unknown keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vm {
    pub vm_id: String,
    pub name: String,
    pub base_image_id: String,
    pub vcpu_count: u32,
    pub ram_mib: u32,
    pub balloon_target_mib: u32,
    pub network_mode: NetworkMode,
    pub state: VmState,
}

/// Wire Snapshot object (`ARCHITECTURE.md` — all 3 keys required, no unknown keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub snapshot_id: String,
    pub vm_id: String,
    pub created_at: String,
}

/// Immutable snapshot metadata stored at `snapshot-metadata/<snapshot_id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotMetadataFile {
    pub snapshot: Snapshot,
    pub vm: Vm,
}

/// Wire `list_vms` result object (`ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListVmsResult {
    pub vms: Vec<Vm>,
}

/// `create_vm` params (`ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateVmParams {
    pub name: String,
    pub base_image_id: String,
    pub vcpu_count: u32,
    pub ram_mib: u32,
    pub network_mode: NetworkMode,
}

/// Params for `start_vm`, `stop_vm`, `pause_vm`, and `snapshot_vm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmIdParams {
    pub vm_id: String,
}

/// `set_balloon_ram` params (`ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetBalloonRamParams {
    pub vm_id: String,
    pub ram_mib: u32,
}

/// `set_network_mode` params (`ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetNetworkModeParams {
    pub vm_id: String,
    pub network_mode: NetworkMode,
}

/// `list_vms` params (`{}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

/// Base image manifest (`/var/lib/hyperos/base-images/<image_id>/manifest.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaseImageManifest {
    pub image_id: String,
    pub sha256: String,
    pub format: String,
}

/// JSON-RPC 2.0 error object (`{"code": INTEGER, "message": STRING}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Parsed JSON-RPC 2.0 method call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcCall {
    CreateVm(CreateVmParams),
    StartVm(VmIdParams),
    StopVm(VmIdParams),
    PauseVm(VmIdParams),
    SnapshotVm(VmIdParams),
    SetBalloonRam(SetBalloonRamParams),
    SetNetworkMode(SetNetworkModeParams),
    ListVms(EmptyParams),
}

/// Validated incoming JSON-RPC 2.0 request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRequest {
    pub id: String,
    pub call: RpcCall,
}

/// Error produced while parsing a raw JSON-RPC frame, carrying an optional recovered valid `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestParseError {
    pub id: Option<String>,
    pub error: RpcError,
}

/// Parse a single UTF-8 JSON-RPC 2.0 request line (without trailing LF).
///
/// Enforces every `ARCHITECTURE.md` wire rule:
/// - No batches, notifications, positional params, or unknown keys
/// - Exact `jsonrpc: "2.0"`
/// - Valid 1-64 char ASCII `[A-Za-z0-9_-]` string `id`
/// - Exact integer types (rejects floats, booleans, numeric strings)
pub fn parse_request_line(line: &str) -> Result<ParsedRequest, RequestParseError> {
    let raw: Value = serde_json::from_str(line).map_err(|_| RequestParseError {
        id: None,
        error: RpcError::new(error_code::PARSE_ERROR, "invalid JSON syntax"),
    })?;

    let Some(obj) = raw.as_object() else {
        return Err(RequestParseError {
            id: None,
            error: RpcError::new(
                error_code::INVALID_REQUEST,
                "request must be a single JSON object (batches are not supported)",
            ),
        });
    };

    // Recover a valid request ID if present before validating the rest of the envelope.
    let recovered_id = match obj.get("id") {
        Some(Value::String(s)) if is_valid_request_id(s) => Some(s.clone()),
        _ => None,
    };

    // Reject unknown top-level keys.
    for key in obj.keys() {
        if !matches!(key.as_str(), "jsonrpc" | "id" | "method" | "params") {
            return Err(RequestParseError {
                id: recovered_id,
                error: RpcError::new(
                    error_code::INVALID_REQUEST,
                    format!("unknown request envelope key: {key}"),
                ),
            });
        }
    }

    // Validate `jsonrpc == "2.0"`.
    match obj.get("jsonrpc") {
        Some(Value::String(v)) if v == JSONRPC_VERSION => {}
        _ => {
            return Err(RequestParseError {
                id: recovered_id,
                error: RpcError::new(error_code::INVALID_REQUEST, "jsonrpc must be \"2.0\""),
            });
        }
    }

    // Require a valid string `id` (notifications are forbidden).
    let id = recovered_id.ok_or_else(|| RequestParseError {
        id: None,
        error: RpcError::new(
            error_code::INVALID_REQUEST,
            "id must be a 1-64 char ASCII [A-Za-z0-9_-] string",
        ),
    })?;

    // Require string `method`.
    let method_name = match obj.get("method") {
        Some(Value::String(m)) => m.as_str(),
        _ => {
            return Err(RequestParseError {
                id: Some(id),
                error: RpcError::new(error_code::INVALID_REQUEST, "method must be a string"),
            });
        }
    };

    // Require JSON object `params` (positional parameter arrays are forbidden).
    let params_val = match obj.get("params") {
        Some(v) if v.is_object() => v.clone(),
        _ => {
            return Err(RequestParseError {
                id: Some(id),
                error: RpcError::new(error_code::INVALID_REQUEST, "params must be a JSON object"),
            });
        }
    };

    let map_params_err = |msg: &str| RequestParseError {
        id: Some(id.clone()),
        error: RpcError::new(error_code::INVALID_PARAMS, msg),
    };

    let call = match method_name {
        method::CREATE_VM => {
            let p: CreateVmParams = serde_json::from_value(params_val)
                .map_err(|_| map_params_err("invalid or unexpected parameters for create_vm"))?;
            RpcCall::CreateVm(p)
        }
        method::START_VM => {
            let p: VmIdParams = serde_json::from_value(params_val)
                .map_err(|_| map_params_err("invalid or unexpected parameters for start_vm"))?;
            RpcCall::StartVm(p)
        }
        method::STOP_VM => {
            let p: VmIdParams = serde_json::from_value(params_val)
                .map_err(|_| map_params_err("invalid or unexpected parameters for stop_vm"))?;
            RpcCall::StopVm(p)
        }
        method::PAUSE_VM => {
            let p: VmIdParams = serde_json::from_value(params_val)
                .map_err(|_| map_params_err("invalid or unexpected parameters for pause_vm"))?;
            RpcCall::PauseVm(p)
        }
        method::SNAPSHOT_VM => {
            let p: VmIdParams = serde_json::from_value(params_val)
                .map_err(|_| map_params_err("invalid or unexpected parameters for snapshot_vm"))?;
            RpcCall::SnapshotVm(p)
        }
        method::SET_BALLOON_RAM => {
            let p: SetBalloonRamParams = serde_json::from_value(params_val).map_err(|_| {
                map_params_err("invalid or unexpected parameters for set_balloon_ram")
            })?;
            RpcCall::SetBalloonRam(p)
        }
        method::SET_NETWORK_MODE => {
            let p: SetNetworkModeParams = serde_json::from_value(params_val).map_err(|_| {
                map_params_err("invalid or unexpected parameters for set_network_mode")
            })?;
            RpcCall::SetNetworkMode(p)
        }
        method::LIST_VMS => {
            let p: EmptyParams = serde_json::from_value(params_val)
                .map_err(|_| map_params_err("list_vms requires an empty params object {}"))?;
            RpcCall::ListVms(p)
        }
        _ => {
            return Err(RequestParseError {
                id: Some(id),
                error: RpcError::new(error_code::METHOD_NOT_FOUND, "unknown RPC method"),
            });
        }
    };

    Ok(ParsedRequest { id, call })
}

#[derive(Serialize)]
struct SuccessEnvelope<'a, T: Serialize> {
    jsonrpc: &'static str,
    id: &'a str,
    result: &'a T,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    jsonrpc: &'static str,
    id: Option<&'a str>,
    error: &'a RpcError,
}

/// Encode a JSON-RPC 2.0 success response frame (UTF-8 JSON + `\n`, `<= 65536` bytes).
pub fn encode_success_frame<T: Serialize>(id: &str, result: &T) -> Result<Vec<u8>, RpcError> {
    let env = SuccessEnvelope {
        jsonrpc: JSONRPC_VERSION,
        id,
        result,
    };
    let mut buf = serde_json::to_vec(&env)
        .map_err(|_| RpcError::new(error_code::INTERNAL_ERROR, "failed to serialize response"))?;
    buf.push(b'\n');
    if buf.len() > MAX_FRAME_BYTES {
        return Err(RpcError::new(
            error_code::INTERNAL_ERROR,
            "response exceeds maximum frame size",
        ));
    }
    Ok(buf)
}

/// Encode a JSON-RPC 2.0 error response frame (UTF-8 JSON + `\n`, `<= 65536` bytes).
pub fn encode_error_frame(id: Option<&str>, error: &RpcError) -> Vec<u8> {
    let env = ErrorEnvelope {
        jsonrpc: JSONRPC_VERSION,
        id,
        error,
    };
    let mut buf = serde_json::to_vec(&env).unwrap_or_else(|_| {
        br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal error"}}"#
            .to_vec()
    });
    buf.push(b'\n');
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_eight_methods_and_encodes_verbatim_responses() {
        let req = parse_request_line(
            r#"{"jsonrpc":"2.0","id":"create-1","method":"create_vm","params":{"name":"demo","base_image_id":"linux-demo","vcpu_count":1,"ram_mib":1024,"network_mode":"isolated"}}"#,
        )
        .unwrap();
        assert_eq!(req.id, "create-1");
        assert!(matches!(req.call, RpcCall::CreateVm(_)));

        let vm = Vm {
            vm_id: "b6f51eed-8dc7-47f6-a8e7-81b6ae783a21".to_string(),
            name: "demo".to_string(),
            base_image_id: "linux-demo".to_string(),
            vcpu_count: 1,
            ram_mib: 1024,
            balloon_target_mib: 1024,
            network_mode: NetworkMode::Isolated,
            state: VmState::Stopped,
        };
        let frame = encode_success_frame("create-1", &vm).unwrap();
        assert_eq!(
            String::from_utf8(frame).unwrap(),
            "{\"jsonrpc\":\"2.0\",\"id\":\"create-1\",\"result\":{\"vm_id\":\"b6f51eed-8dc7-47f6-a8e7-81b6ae783a21\",\"name\":\"demo\",\"base_image_id\":\"linux-demo\",\"vcpu_count\":1,\"ram_mib\":1024,\"balloon_target_mib\":1024,\"network_mode\":\"isolated\",\"state\":\"stopped\"}}\n"
        );

        let list_req = parse_request_line(
            r#"{"jsonrpc":"2.0","id":"list-1","method":"list_vms","params":{}}"#,
        )
        .unwrap();
        assert!(matches!(list_req.call, RpcCall::ListVms(_)));
        let list_frame = encode_success_frame("list-1", &ListVmsResult { vms: vec![] }).unwrap();
        assert_eq!(
            String::from_utf8(list_frame).unwrap(),
            "{\"jsonrpc\":\"2.0\",\"id\":\"list-1\",\"result\":{\"vms\":[]}}\n"
        );
    }

    #[test]
    fn classifies_parse_envelope_method_and_param_errors_correctly() {
        // Parse error -> -32700, id: null
        let e = parse_request_line("{not json").unwrap_err();
        assert_eq!(e.id, None);
        assert_eq!(e.error.code, error_code::PARSE_ERROR);

        // Batch -> -32600, id: null
        let e = parse_request_line("[]").unwrap_err();
        assert_eq!(e.id, None);
        assert_eq!(e.error.code, error_code::INVALID_REQUEST);

        // Notification (missing id) -> -32600, id: null
        let e =
            parse_request_line(r#"{"jsonrpc":"2.0","method":"list_vms","params":{}}"#).unwrap_err();
        assert_eq!(e.id, None);
        assert_eq!(e.error.code, error_code::INVALID_REQUEST);

        // Positional params -> -32600, preserves valid id
        let e =
            parse_request_line(r#"{"jsonrpc":"2.0","id":"r1","method":"list_vms","params":[]}"#)
                .unwrap_err();
        assert_eq!(e.id.as_deref(), Some("r1"));
        assert_eq!(e.error.code, error_code::INVALID_REQUEST);

        // Unknown method -> -32601
        let e =
            parse_request_line(r#"{"jsonrpc":"2.0","id":"r2","method":"delete_vm","params":{}}"#)
                .unwrap_err();
        assert_eq!(e.id.as_deref(), Some("r2"));
        assert_eq!(e.error.code, error_code::METHOD_NOT_FOUND);

        // Unknown param key -> -32602
        let e = parse_request_line(
            r#"{"jsonrpc":"2.0","id":"r3","method":"list_vms","params":{"extra":true}}"#,
        )
        .unwrap_err();
        assert_eq!(e.id.as_deref(), Some("r3"));
        assert_eq!(e.error.code, error_code::INVALID_PARAMS);

        // Float for integer ram_mib -> -32602
        let e = parse_request_line(
            r#"{"jsonrpc":"2.0","id":"r4","method":"set_balloon_ram","params":{"vm_id":"b6f51eed-8dc7-47f6-a8e7-81b6ae783a21","ram_mib":512.0}}"#,
        )
        .unwrap_err();
        assert_eq!(e.id.as_deref(), Some("r4"));
        assert_eq!(e.error.code, error_code::INVALID_PARAMS);
    }
}
