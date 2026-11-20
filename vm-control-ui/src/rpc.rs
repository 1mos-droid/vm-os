//! Exact JSON-RPC 2.0 wire types, validation, and framing matching `ARCHITECTURE.md`.

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::marker::PhantomData;

/// Default Unix domain socket path for `vmm-daemon` (`ARCHITECTURE.md`).
pub const DEFAULT_SOCKET_PATH: &str = "/run/hyperos/vmm.sock";

/// Persistent CoW storage root (`ARCHITECTURE.md`).
pub const STORAGE_ROOT: &str = "/var/lib/hyperos";
pub const BASE_IMAGES_DIR: &str = "/var/lib/hyperos/base-images";
pub const INSTANCES_DIR: &str = "/var/lib/hyperos/instances";

/// Maximum frame length in bytes including the trailing LF (`\n`).
pub const MAX_FRAME_BYTES: usize = 65_536;

/// MiB definition used throughout HyperOS (`ARCHITECTURE.md`).
pub const MIB_BYTES: u64 = 1_048_576;

/// Host RAM reserved for HyperOS base OS (`MemTotal - 768 MiB` admission limit).
pub const HOST_RAM_RESERVE_MIB: u64 = 768;

/// Allowed VM RAM bounds in MiB (`create_vm.ram_mib`).
pub const MIN_VM_RAM_MIB: u32 = 128;
pub const MAX_VM_RAM_MIB: u32 = 3072;

/// Allowed vCPU count bounds (`create_vm.vcpu_count`).
pub const MIN_VCPU_COUNT: u32 = 1;
pub const MAX_VCPU_COUNT: u32 = 64;

/// JSON-RPC 2.0 protocol version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// Standard and HyperOS-specific JSON-RPC 2.0 error codes (`ARCHITECTURE.md`).
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

/// RPC method names defined in `ARCHITECTURE.md`.
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

/// Validate a 64-character lowercase hex SHA-256 digest (for `manifest.json`).
pub fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Translate `(maximum_ram_mib, desired_ram_mib)` to balloon bytes per `ARCHITECTURE.md`:
/// `balloon_bytes = (maximum - desired) * 1048576`.
pub fn balloon_bytes(max_ram_mib: u32, desired_ram_mib: u32) -> Result<u64, String> {
    if desired_ram_mib < MIN_VM_RAM_MIB || desired_ram_mib > max_ram_mib {
        return Err(format!(
            "desired RAM ({desired_ram_mib} MiB) must be within {MIN_VM_RAM_MIB}..={max_ram_mib} MiB"
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

    pub fn from_index(idx: i32) -> Self {
        match idx {
            1 => Self::Nat,
            2 => Self::HostOnly,
            _ => Self::Isolated,
        }
    }

    pub fn to_index(self) -> i32 {
        match self {
            Self::Isolated => 0,
            Self::Nat => 1,
            Self::HostOnly => 2,
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

/// VM object schema (`ARCHITECTURE.md` — all 8 keys required, no unknown keys).
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

/// Snapshot object schema (`ARCHITECTURE.md` — all 3 keys required, no unknown keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub snapshot_id: String,
    pub vm_id: String,
    pub created_at: String,
}

/// `list_vms` result schema (`ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListVmsResult {
    pub vms: Vec<Vm>,
}

/// `create_vm` params schema (`ARCHITECTURE.md`).
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

/// `set_balloon_ram` params schema (`ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetBalloonRamParams {
    pub vm_id: String,
    pub ram_mib: u32,
}

/// `set_network_mode` params schema (`ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetNetworkModeParams {
    pub vm_id: String,
    pub network_mode: NetworkMode,
}

/// `list_vms` params schema (`{}` with no unknown keys).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

/// Base image `manifest.json` schema (`/var/lib/hyperos/base-images/<image_id>/manifest.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaseImageManifest {
    pub image_id: String,
    pub sha256: String,
    pub format: String,
}

/// JSON-RPC 2.0 error payload (`{"code": INTEGER, "message": STRING}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// JSON-RPC 2.0 request envelope:
/// `{"jsonrpc":"2.0","id":ID,"method":METHOD,"params":OBJECT}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RpcRequest<P> {
    pub jsonrpc: &'static str,
    pub id: String,
    pub method: &'static str,
    pub params: P,
}

impl<P> RpcRequest<P> {
    pub fn new(id: impl Into<String>, method: &'static str, params: P) -> Result<Self, String> {
        let id = id.into();
        if !is_valid_request_id(&id) {
            return Err(format!("invalid JSON-RPC request id: {id:?}"));
        }
        Ok(Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            method,
            params,
        })
    }
}

/// Strongly typed request envelope covering every method in `ARCHITECTURE.md`.
/// Deserialization strictly validates `jsonrpc == "2.0"`, `id` syntax, exact
/// method name, exact params schema, and rejects unknown keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcMethodEnvelope {
    pub id: String,
    pub call: RpcMethodCall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcMethodCall {
    CreateVm(CreateVmParams),
    StartVm(VmIdParams),
    StopVm(VmIdParams),
    PauseVm(VmIdParams),
    SnapshotVm(VmIdParams),
    SetBalloonRam(SetBalloonRamParams),
    SetNetworkMode(SetNetworkModeParams),
    ListVms(EmptyParams),
}

impl RpcMethodCall {
    pub fn method_name(&self) -> &'static str {
        match self {
            Self::CreateVm(_) => method::CREATE_VM,
            Self::StartVm(_) => method::START_VM,
            Self::StopVm(_) => method::STOP_VM,
            Self::PauseVm(_) => method::PAUSE_VM,
            Self::SnapshotVm(_) => method::SNAPSHOT_VM,
            Self::SetBalloonRam(_) => method::SET_BALLOON_RAM,
            Self::SetNetworkMode(_) => method::SET_NETWORK_MODE,
            Self::ListVms(_) => method::LIST_VMS,
        }
    }
}

impl Serialize for RpcMethodEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if !is_valid_request_id(&self.id) {
            return Err(serde::ser::Error::custom("invalid request id"));
        }
        let mut map = serializer.serialize_map(Some(4))?;
        map.serialize_entry("jsonrpc", JSONRPC_VERSION)?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("method", self.call.method_name())?;
        match &self.call {
            RpcMethodCall::CreateVm(p) => map.serialize_entry("params", p)?,
            RpcMethodCall::StartVm(p)
            | RpcMethodCall::StopVm(p)
            | RpcMethodCall::PauseVm(p)
            | RpcMethodCall::SnapshotVm(p) => map.serialize_entry("params", p)?,
            RpcMethodCall::SetBalloonRam(p) => map.serialize_entry("params", p)?,
            RpcMethodCall::SetNetworkMode(p) => map.serialize_entry("params", p)?,
            RpcMethodCall::ListVms(p) => map.serialize_entry("params", p)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for RpcMethodEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawRequest {
            jsonrpc: String,
            id: String,
            method: String,
            params: serde_json::Value,
        }

        let raw = RawRequest::deserialize(deserializer)?;
        if raw.jsonrpc != JSONRPC_VERSION {
            return Err(de::Error::custom("jsonrpc must be \"2.0\""));
        }
        if !is_valid_request_id(&raw.id) {
            return Err(de::Error::custom(
                "id must be 1-64 ASCII letters, digits, '_' or '-'",
            ));
        }
        if !raw.params.is_object() {
            return Err(de::Error::custom("params must be a JSON object"));
        }

        let call = match raw.method.as_str() {
            method::CREATE_VM => {
                RpcMethodCall::CreateVm(serde_json::from_value(raw.params).map_err(de::Error::custom)?)
            }
            method::START_VM => {
                RpcMethodCall::StartVm(serde_json::from_value(raw.params).map_err(de::Error::custom)?)
            }
            method::STOP_VM => {
                RpcMethodCall::StopVm(serde_json::from_value(raw.params).map_err(de::Error::custom)?)
            }
            method::PAUSE_VM => {
                RpcMethodCall::PauseVm(serde_json::from_value(raw.params).map_err(de::Error::custom)?)
            }
            method::SNAPSHOT_VM => {
                RpcMethodCall::SnapshotVm(serde_json::from_value(raw.params).map_err(de::Error::custom)?)
            }
            method::SET_BALLOON_RAM => RpcMethodCall::SetBalloonRam(
                serde_json::from_value(raw.params).map_err(de::Error::custom)?,
            ),
            method::SET_NETWORK_MODE => RpcMethodCall::SetNetworkMode(
                serde_json::from_value(raw.params).map_err(de::Error::custom)?,
            ),
            method::LIST_VMS => {
                RpcMethodCall::ListVms(serde_json::from_value(raw.params).map_err(de::Error::custom)?)
            }
            other => {
                return Err(de::Error::custom(format!("unknown RPC method: {other}")));
            }
        };

        Ok(Self { id: raw.id, call })
    }
}

/// JSON-RPC 2.0 response envelope (`ARCHITECTURE.md`):
/// - Success: `{"jsonrpc":"2.0","id":ID,"result":RESULT}`
/// - Error:   `{"jsonrpc":"2.0","id":ID_OR_NULL,"error":{"code":INTEGER,"message":STRING}}`
///
/// Enforces mutual exclusivity of `result` and `error`, rejects unknown keys,
/// requires `jsonrpc == "2.0"`, and forbids `id: null` on success responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcResponse<T> {
    Success { id: String, result: T },
    Error { id: Option<String>, error: RpcError },
}

impl<T> RpcResponse<T> {
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Success { id, .. } => Some(id.as_str()),
            Self::Error { id, .. } => id.as_deref(),
        }
    }

    pub fn into_result(self) -> Result<T, RpcError> {
        match self {
            Self::Success { result, .. } => Ok(result),
            Self::Error { error, .. } => Err(error),
        }
    }
}

impl<T: Serialize> Serialize for RpcResponse<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("jsonrpc", JSONRPC_VERSION)?;
        match self {
            Self::Success { id, result } => {
                if !is_valid_request_id(id) {
                    return Err(serde::ser::Error::custom("invalid response id"));
                }
                map.serialize_entry("id", id)?;
                map.serialize_entry("result", result)?;
            }
            Self::Error { id, error } => {
                if let Some(id_str) = id {
                    if !is_valid_request_id(id_str) {
                        return Err(serde::ser::Error::custom("invalid error response id"));
                    }
                }
                map.serialize_entry("id", id)?;
                map.serialize_entry("error", error)?;
            }
        }
        map.end()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for RpcResponse<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ResponseVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ResponseVisitor<T> {
            type Value = RpcResponse<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON-RPC 2.0 response object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut jsonrpc: Option<String> = None;
                let mut id_seen = false;
                let mut id: Option<String> = None;
                let mut result_seen = false;
                let mut result: Option<T> = None;
                let mut error_seen = false;
                let mut error: Option<RpcError> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "jsonrpc" => {
                            if jsonrpc.is_some() {
                                return Err(de::Error::duplicate_field("jsonrpc"));
                            }
                            jsonrpc = Some(map.next_value()?);
                        }
                        "id" => {
                            if id_seen {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            id_seen = true;
                            id = map.next_value()?;
                        }
                        "result" => {
                            if result_seen {
                                return Err(de::Error::duplicate_field("result"));
                            }
                            result_seen = true;
                            result = Some(map.next_value()?);
                        }
                        "error" => {
                            if error_seen {
                                return Err(de::Error::duplicate_field("error"));
                            }
                            error_seen = true;
                            error = Some(map.next_value()?);
                        }
                        unknown => {
                            return Err(de::Error::unknown_field(
                                unknown,
                                &["jsonrpc", "id", "result", "error"],
                            ));
                        }
                    }
                }

                match jsonrpc.as_deref() {
                    Some(JSONRPC_VERSION) => {}
                    Some(other) => {
                        return Err(de::Error::custom(format!(
                            "invalid jsonrpc version: {other}"
                        )));
                    }
                    None => return Err(de::Error::missing_field("jsonrpc")),
                }

                if !id_seen {
                    return Err(de::Error::missing_field("id"));
                }
                if let Some(ref id_str) = id {
                    if !is_valid_request_id(id_str) {
                        return Err(de::Error::custom("invalid response id format"));
                    }
                }

                match (result_seen, error_seen) {
                    (true, true) => {
                        Err(de::Error::custom("response must not contain both result and error"))
                    }
                    (false, false) => {
                        Err(de::Error::custom("response must contain either result or error"))
                    }
                    (true, false) => {
                        let id_str = id.ok_or_else(|| {
                            de::Error::custom("success response must have a non-null string id")
                        })?;
                        Ok(RpcResponse::Success {
                            id: id_str,
                            result: result.expect("result_seen implies Some"),
                        })
                    }
                    (false, true) => Ok(RpcResponse::Error {
                        id,
                        error: error.expect("error_seen implies Some"),
                    }),
                }
            }
        }

        deserializer.deserialize_map(ResponseVisitor(PhantomData))
    }
}

/// Encode a JSON-RPC message into a single UTF-8 JSON line terminated by LF (`\n`).
/// Enforces the `MAX_FRAME_BYTES` (65536 bytes including LF) contract.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| format!("JSON encode error: {e}"))?;
    if bytes.contains(&b'\n') || bytes.contains(&b'\r') {
        return Err("encoded JSON frame unexpectedly contains line breaks".to_string());
    }
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(format!(
            "frame size {} exceeds maximum {} bytes",
            bytes.len(),
            MAX_FRAME_BYTES
        ));
    }
    Ok(bytes)
}

/// Decode a single LF-terminated JSON-RPC response frame.
/// Rejects oversized frames (`> 65536` bytes including LF) and non-object payloads (such as batches).
pub fn decode_response_frame<T: for<'de> Deserialize<'de>>(
    frame: &[u8],
) -> Result<RpcResponse<T>, String> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(format!(
            "response frame size {} exceeds maximum {} bytes",
            frame.len(),
            MAX_FRAME_BYTES
        ));
    }
    let line = frame
        .strip_suffix(b"\n")
        .ok_or_else(|| "response frame missing trailing LF".to_string())?;
    let trimmed = std::str::from_utf8(line)
        .map_err(|e| format!("response frame is not valid UTF-8: {e}"))?;
    if !trimmed.trim_start().starts_with('{') {
        return Err("response frame must be a single JSON object (no batches)".to_string());
    }
    serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON-RPC response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_VM_ID: &str = "b6f51eed-8dc7-47f6-a8e7-81b6ae783a21";
    const SAMPLE_SNAP_ID: &str = "4f281e57-3717-40fe-9d90-025f37962bbd";

    fn sample_vm() -> Vm {
        Vm {
            vm_id: SAMPLE_VM_ID.to_string(),
            name: "demo".to_string(),
            base_image_id: "linux-demo".to_string(),
            vcpu_count: 1,
            ram_mib: 1024,
            balloon_target_mib: 1024,
            network_mode: NetworkMode::Isolated,
            state: VmState::Stopped,
        }
    }

    #[test]
    fn architecture_example_create_vm_roundtrip_verbatim() {
        let req = RpcMethodEnvelope {
            id: "create-1".to_string(),
            call: RpcMethodCall::CreateVm(CreateVmParams {
                name: "demo".to_string(),
                base_image_id: "linux-demo".to_string(),
                vcpu_count: 1,
                ram_mib: 1024,
                network_mode: NetworkMode::Isolated,
            }),
        };
        let wire = serde_json::to_string(&req).unwrap();
        assert_eq!(
            wire,
            r#"{"jsonrpc":"2.0","id":"create-1","method":"create_vm","params":{"name":"demo","base_image_id":"linux-demo","vcpu_count":1,"ram_mib":1024,"network_mode":"isolated"}}"#
        );
        let parsed: RpcMethodEnvelope = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed, req);

        let resp_wire = r#"{"jsonrpc":"2.0","id":"create-1","result":{"vm_id":"b6f51eed-8dc7-47f6-a8e7-81b6ae783a21","name":"demo","base_image_id":"linux-demo","vcpu_count":1,"ram_mib":1024,"balloon_target_mib":1024,"network_mode":"isolated","state":"stopped"}}"#;
        let resp: RpcResponse<Vm> = serde_json::from_str(resp_wire).unwrap();
        assert_eq!(
            resp,
            RpcResponse::Success {
                id: "create-1".to_string(),
                result: sample_vm(),
            }
        );
        assert_eq!(serde_json::to_string(&resp).unwrap(), resp_wire);
    }

    #[test]
    fn architecture_example_list_vms_roundtrip_verbatim() {
        let req = RpcMethodEnvelope {
            id: "list-1".to_string(),
            call: RpcMethodCall::ListVms(EmptyParams {}),
        };
        let wire = serde_json::to_string(&req).unwrap();
        assert_eq!(
            wire,
            r#"{"jsonrpc":"2.0","id":"list-1","method":"list_vms","params":{}}"#
        );
        let parsed: RpcMethodEnvelope = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed, req);

        let empty_resp_wire = r#"{"jsonrpc":"2.0","id":"list-1","result":{"vms":[]}}"#;
        let empty_resp: RpcResponse<ListVmsResult> = serde_json::from_str(empty_resp_wire).unwrap();
        assert_eq!(
            empty_resp,
            RpcResponse::Success {
                id: "list-1".to_string(),
                result: ListVmsResult { vms: vec![] },
            }
        );
        assert_eq!(serde_json::to_string(&empty_resp).unwrap(), empty_resp_wire);

        let populated = RpcResponse::Success {
            id: "list-2".to_string(),
            result: ListVmsResult {
                vms: vec![sample_vm()],
            },
        };
        let populated_wire = serde_json::to_string(&populated).unwrap();
        let populated_parsed: RpcResponse<ListVmsResult> =
            serde_json::from_str(&populated_wire).unwrap();
        assert_eq!(populated_parsed, populated);
    }

    #[test]
    fn start_stop_pause_vm_roundtrip() {
        for (id, method_name, call) in [
            (
                "start-1",
                "start_vm",
                RpcMethodCall::StartVm(VmIdParams {
                    vm_id: SAMPLE_VM_ID.to_string(),
                }),
            ),
            (
                "stop-1",
                "stop_vm",
                RpcMethodCall::StopVm(VmIdParams {
                    vm_id: SAMPLE_VM_ID.to_string(),
                }),
            ),
            (
                "pause-1",
                "pause_vm",
                RpcMethodCall::PauseVm(VmIdParams {
                    vm_id: SAMPLE_VM_ID.to_string(),
                }),
            ),
        ] {
            let env = RpcMethodEnvelope {
                id: id.to_string(),
                call,
            };
            let wire = serde_json::to_string(&env).unwrap();
            let expected = format!(
                r#"{{"jsonrpc":"2.0","id":"{id}","method":"{method_name}","params":{{"vm_id":"{SAMPLE_VM_ID}"}}}}"#
            );
            assert_eq!(wire, expected);
            let parsed: RpcMethodEnvelope = serde_json::from_str(&wire).unwrap();
            assert_eq!(parsed, env);
        }

        for state in [
            VmState::Running,
            VmState::Paused,
            VmState::Stopped,
            VmState::Failed,
        ] {
            let mut vm = sample_vm();
            vm.state = state;
            let resp = RpcResponse::Success {
                id: "state-1".to_string(),
                result: vm.clone(),
            };
            let wire = serde_json::to_string(&resp).unwrap();
            let decoded: RpcResponse<Vm> = serde_json::from_str(&wire).unwrap();
            assert_eq!(decoded, resp);
        }
    }

    #[test]
    fn snapshot_vm_roundtrip_verbatim() {
        let req = RpcMethodEnvelope {
            id: "snap-1".to_string(),
            call: RpcMethodCall::SnapshotVm(VmIdParams {
                vm_id: SAMPLE_VM_ID.to_string(),
            }),
        };
        let req_wire = serde_json::to_string(&req).unwrap();
        assert_eq!(
            req_wire,
            format!(
                r#"{{"jsonrpc":"2.0","id":"snap-1","method":"snapshot_vm","params":{{"vm_id":"{SAMPLE_VM_ID}"}}}}"#
            )
        );
        assert_eq!(serde_json::from_str::<RpcMethodEnvelope>(&req_wire).unwrap(), req);

        let snap_resp_wire = r#"{"jsonrpc":"2.0","id":"snap-1","result":{"snapshot_id":"4f281e57-3717-40fe-9d90-025f37962bbd","vm_id":"b6f51eed-8dc7-47f6-a8e7-81b6ae783a21","created_at":"2026-11-20T00:00:00Z"}}"#;
        let resp: RpcResponse<Snapshot> = serde_json::from_str(snap_resp_wire).unwrap();
        assert_eq!(
            resp,
            RpcResponse::Success {
                id: "snap-1".to_string(),
                result: Snapshot {
                    snapshot_id: SAMPLE_SNAP_ID.to_string(),
                    vm_id: SAMPLE_VM_ID.to_string(),
                    created_at: "2026-11-20T00:00:00Z".to_string(),
                },
            }
        );
        assert_eq!(serde_json::to_string(&resp).unwrap(), snap_resp_wire);
    }

    #[test]
    fn set_balloon_ram_roundtrip() {
        let req = RpcMethodEnvelope {
            id: "balloon-1".to_string(),
            call: RpcMethodCall::SetBalloonRam(SetBalloonRamParams {
                vm_id: SAMPLE_VM_ID.to_string(),
                ram_mib: 512,
            }),
        };
        let wire = serde_json::to_string(&req).unwrap();
        assert_eq!(
            wire,
            format!(
                r#"{{"jsonrpc":"2.0","id":"balloon-1","method":"set_balloon_ram","params":{{"vm_id":"{SAMPLE_VM_ID}","ram_mib":512}}}}"#
            )
        );
        assert_eq!(serde_json::from_str::<RpcMethodEnvelope>(&wire).unwrap(), req);

        assert_eq!(balloon_bytes(1024, 512).unwrap(), 512 * 1_048_576);
        assert_eq!(balloon_bytes(1024, 1024).unwrap(), 0);
        assert!(balloon_bytes(1024, 127).is_err());
        assert!(balloon_bytes(1024, 1025).is_err());
    }

    #[test]
    fn set_network_mode_roundtrip_all_modes() {
        for (mode, expected_str) in [
            (NetworkMode::Isolated, "isolated"),
            (NetworkMode::Nat, "nat"),
            (NetworkMode::HostOnly, "host-only"),
        ] {
            let req = RpcMethodEnvelope {
                id: format!("net-{expected_str}"),
                call: RpcMethodCall::SetNetworkMode(SetNetworkModeParams {
                    vm_id: SAMPLE_VM_ID.to_string(),
                    network_mode: mode,
                }),
            };
            let wire = serde_json::to_string(&req).unwrap();
            assert_eq!(
                wire,
                format!(
                    r#"{{"jsonrpc":"2.0","id":"net-{expected_str}","method":"set_network_mode","params":{{"vm_id":"{SAMPLE_VM_ID}","network_mode":"{expected_str}"}}}}"#
                )
            );
            assert_eq!(serde_json::from_str::<RpcMethodEnvelope>(&wire).unwrap(), req);
        }
    }

    #[test]
    fn error_response_roundtrip_with_id_and_null_id() {
        let err_with_id = r#"{"jsonrpc":"2.0","id":"req-9","error":{"code":-32002,"message":"VM must be stopped to snapshot"}}"#;
        let parsed: RpcResponse<Snapshot> = serde_json::from_str(err_with_id).unwrap();
        assert_eq!(
            parsed,
            RpcResponse::Error {
                id: Some("req-9".to_string()),
                error: RpcError {
                    code: error_code::INVALID_STATE,
                    message: "VM must be stopped to snapshot".to_string(),
                },
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), err_with_id);

        let err_null_id = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#;
        let parsed_null: RpcResponse<Vm> = serde_json::from_str(err_null_id).unwrap();
        assert_eq!(
            parsed_null,
            RpcResponse::Error {
                id: None,
                error: RpcError {
                    code: error_code::PARSE_ERROR,
                    message: "parse error".to_string(),
                },
            }
        );
        assert_eq!(serde_json::to_string(&parsed_null).unwrap(), err_null_id);
    }

    #[test]
    fn rejects_unknown_fields_and_invalid_numeric_types() {
        // Unknown key on create_vm params
        let unknown_param = r#"{"jsonrpc":"2.0","id":"c1","method":"create_vm","params":{"name":"demo","base_image_id":"linux-demo","vcpu_count":1,"ram_mib":1024,"network_mode":"isolated","ephemeral":true}}"#;
        assert!(serde_json::from_str::<RpcMethodEnvelope>(unknown_param).is_err());

        // Unknown key on list_vms params
        let unknown_list = r#"{"jsonrpc":"2.0","id":"l1","method":"list_vms","params":{"all":true}}"#;
        assert!(serde_json::from_str::<RpcMethodEnvelope>(unknown_list).is_err());

        // Unknown key on VM response
        let unknown_vm_key = r#"{"jsonrpc":"2.0","id":"c1","result":{"vm_id":"b6f51eed-8dc7-47f6-a8e7-81b6ae783a21","name":"demo","base_image_id":"linux-demo","vcpu_count":1,"ram_mib":1024,"balloon_target_mib":1024,"network_mode":"isolated","state":"stopped","extra":1}}"#;
        assert!(serde_json::from_str::<RpcResponse<Vm>>(unknown_vm_key).is_err());

        // Float, boolean, and numeric string rejected for integer fields
        let float_ram = r#"{"jsonrpc":"2.0","id":"c1","method":"create_vm","params":{"name":"demo","base_image_id":"linux-demo","vcpu_count":1,"ram_mib":1024.0,"network_mode":"isolated"}}"#;
        assert!(serde_json::from_str::<RpcMethodEnvelope>(float_ram).is_err());

        let string_vcpu = r#"{"jsonrpc":"2.0","id":"c1","method":"create_vm","params":{"name":"demo","base_image_id":"linux-demo","vcpu_count":"1","ram_mib":1024,"network_mode":"isolated"}}"#;
        assert!(serde_json::from_str::<RpcMethodEnvelope>(string_vcpu).is_err());

        let bool_ram = r#"{"jsonrpc":"2.0","id":"b1","method":"set_balloon_ram","params":{"vm_id":"b6f51eed-8dc7-47f6-a8e7-81b6ae783a21","ram_mib":true}}"#;
        assert!(serde_json::from_str::<RpcMethodEnvelope>(bool_ram).is_err());

        let float_error_code = r#"{"jsonrpc":"2.0","id":"e1","error":{"code":-32602.0,"message":"bad"}}"#;
        assert!(serde_json::from_str::<RpcResponse<Vm>>(float_error_code).is_err());
    }

    #[test]
    fn rejects_invalid_response_envelopes_and_oversized_frames() {
        // Both result and error
        let both = r#"{"jsonrpc":"2.0","id":"1","result":{"vms":[]},"error":{"code":-32603,"message":"oops"}}"#;
        assert!(serde_json::from_str::<RpcResponse<ListVmsResult>>(both).is_err());

        // Neither result nor error
        let neither = r#"{"jsonrpc":"2.0","id":"1"}"#;
        assert!(serde_json::from_str::<RpcResponse<ListVmsResult>>(neither).is_err());

        // Null id on success
        let null_success = r#"{"jsonrpc":"2.0","id":null,"result":{"vms":[]}}"#;
        assert!(serde_json::from_str::<RpcResponse<ListVmsResult>>(null_success).is_err());

        // Wrong jsonrpc version
        let wrong_ver = r#"{"jsonrpc":"1.0","id":"1","result":{"vms":[]}}"#;
        assert!(serde_json::from_str::<RpcResponse<ListVmsResult>>(wrong_ver).is_err());

        // Batch response rejected by frame decoder
        let batch = b"[{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"result\":{\"vms\":[]}}]\n";
        assert!(decode_response_frame::<ListVmsResult>(batch).is_err());

        // Oversized frame rejected
        let mut oversized = vec![b' '; MAX_FRAME_BYTES];
        oversized.push(b'\n');
        assert!(decode_response_frame::<ListVmsResult>(&oversized).is_err());
    }

    #[test]
    fn validates_ids_uuids_and_names() {
        assert!(is_valid_request_id("create-1_ABC"));
        assert!(!is_valid_request_id(""));
        assert!(!is_valid_request_id("bad id"));
        assert!(!is_valid_request_id(&"a".repeat(65)));

        assert!(is_valid_uuid("b6f51eed-8dc7-47f6-a8e7-81b6ae783a21"));
        assert!(!is_valid_uuid("B6F51EED-8DC7-47F6-A8E7-81B6AE783A21")); // uppercase disallowed
        assert!(!is_valid_uuid("not-a-uuid"));

        assert!(is_valid_vm_name("workstation-01"));
        assert!(!is_valid_vm_name(""));
        assert!(!is_valid_vm_name("line\nbreak"));
        assert!(!is_valid_vm_name(&"x".repeat(65)));

        assert!(is_valid_sha256_hex(&"a".repeat(64)));
        assert!(!is_valid_sha256_hex(&"A".repeat(64)));
        assert!(!is_valid_sha256_hex("short"));
    }
}
