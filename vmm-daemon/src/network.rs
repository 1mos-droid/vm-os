//! Network sandboxing (`isolated`, `nat`, `host-only`) with rollback and teardown.
//!
//! All system invocations use `std::process::Command` with explicit argument arrays;
//! request values are never interpolated into a shell.

use crate::capture::{AfPacketSource, PacketSource};
use crate::rpc::{error_code, NetworkMode, RpcError};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;

/// Active host network allocation attached to a running/paused VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkAllocation {
    pub vm_id: String,
    pub mode: NetworkMode,
    pub tap_name: String,
    pub bridge_name: Option<String>,
    pub nft_table: String,
    pub host_cidr: String,
    pub guest_ip: String,
    pub subnet_slot: u8,
    /// The kernel's own listing of this Space's nftables table, recorded right
    /// after installation. Assurance monitor HYP001 compares the live table
    /// against it (the kernel normalises rules, e.g. merges adjacent ranges, so
    /// a fixed text match would be brittle).
    pub ruleset_baseline: Option<String>,
}

/// Abstract network sandbox controller implemented for Linux (`ip`/`nft`) and `--dry-run`.
pub trait NetworkController: Send + Sync {
    fn setup(&self, vm_id: &str, mode: NetworkMode) -> Result<Option<NetworkAllocation>, RpcError>;
    fn teardown(&self, alloc: &NetworkAllocation);
    /// Open a frame source seeing both directions of the VM's TAP.
    fn open_capture(&self, alloc: &NetworkAllocation) -> Result<Box<dyn PacketSource>, RpcError>;
    /// Whether the Space's firewall table is still exactly as installed
    /// (input to assurance monitor HYP001).
    fn isolation_intact(&self, alloc: &NetworkAllocation) -> bool;
}

fn short_id(vm_id: &str) -> String {
    vm_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .collect()
}

fn allocate_slot(used: &mut HashSet<u8>) -> Result<u8, RpcError> {
    for slot in 1u8..=254 {
        if used.insert(slot) {
            return Ok(slot);
        }
    }
    Err(RpcError::new(
        error_code::INSUFFICIENT_RESOURCES,
        "no available private VM network subnets",
    ))
}

fn build_allocation(vm_id: &str, mode: NetworkMode, slot: u8) -> NetworkAllocation {
    let short = short_id(vm_id);
    match mode {
        NetworkMode::Nat => NetworkAllocation {
            vm_id: vm_id.to_string(),
            mode,
            tap_name: format!("hvmtap{short}"),
            bridge_name: None,
            nft_table: format!("hyperos_{short}"),
            host_cidr: format!("10.200.{slot}.1/30"),
            guest_ip: format!("10.200.{slot}.2"),
            subnet_slot: slot,
            ruleset_baseline: None,
        },
        NetworkMode::HostOnly => NetworkAllocation {
            vm_id: vm_id.to_string(),
            mode,
            tap_name: format!("hvmtap{short}"),
            bridge_name: Some(format!("hvmbr{short}")),
            nft_table: format!("hyperos_{short}"),
            host_cidr: format!("10.201.{slot}.1/30"),
            guest_ip: format!("10.201.{slot}.2"),
            subnet_slot: slot,
            ruleset_baseline: None,
        },
        NetworkMode::Isolated => unreachable!("isolated mode allocates no NIC"),
    }
}

/// Production Linux network controller using `ip` and `nft` (`nftables`).
pub struct LinuxNetworkController {
    used_slots: Mutex<HashSet<u8>>,
}

impl Default for LinuxNetworkController {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxNetworkController {
    pub fn new() -> Self {
        Self {
            used_slots: Mutex::new(HashSet::new()),
        }
    }

    fn is_valid_ifname(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 15
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'@'))
    }

    fn detect_uplink() -> Result<String, RpcError> {
        let output = Command::new("ip")
            .args(["route", "show", "default"])
            .output()
            .map_err(|_| {
                RpcError::new(
                    error_code::UNAVAILABLE_CAPABILITY,
                    "iproute2 utility unavailable for networking",
                )
            })?;

        if !output.status.success() {
            return Err(RpcError::new(
                error_code::UNAVAILABLE_CAPABILITY,
                "failed to query default uplink route",
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            for pair in tokens.windows(2) {
                if pair[0] == "dev" && Self::is_valid_ifname(pair[1]) {
                    return Ok(pair[1].to_string());
                }
            }
        }

        Err(RpcError::new(
            error_code::UNAVAILABLE_CAPABILITY,
            "no host uplink configured for nat network mode",
        ))
    }

    fn run_ip(args: &[&str]) -> Result<(), RpcError> {
        let output = Command::new("ip").args(args).output().map_err(|_| {
            RpcError::new(
                error_code::UNAVAILABLE_CAPABILITY,
                "iproute2 utility unavailable",
            )
        })?;
        if !output.status.success() {
            return Err(RpcError::new(
                error_code::UNAVAILABLE_CAPABILITY,
                "network interface configuration failed",
            ));
        }
        Ok(())
    }

    fn installed_baseline(table: &str) -> Result<String, RpcError> {
        Self::list_table(table).ok_or_else(|| {
            RpcError::new(
                error_code::UNAVAILABLE_CAPABILITY,
                "nftables sandbox table missing after installation",
            )
        })
    }

    /// The kernel's current listing of one HyperOS table, or `None` if it is gone.
    fn list_table(table: &str) -> Option<String> {
        let out = Command::new("nft")
            .args(["list", "table", "inet", table])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }

    fn apply_nft_ruleset(script: &str) -> Result<(), RpcError> {
        let mut child = Command::new("nft")
            .args(["-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| {
                RpcError::new(
                    error_code::UNAVAILABLE_CAPABILITY,
                    "nftables utility unavailable for network sandboxing",
                )
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(script.as_bytes());
        }
        let status = child.wait().map_err(|_| {
            RpcError::new(
                error_code::UNAVAILABLE_CAPABILITY,
                "failed waiting for nftables configuration",
            )
        })?;
        if !status.success() {
            return Err(RpcError::new(
                error_code::UNAVAILABLE_CAPABILITY,
                "nftables sandbox rule installation failed",
            ));
        }
        Ok(())
    }
}

/// Non-global IPv4 destinations a `nat` guest must never reach (`ARCHITECTURE.md`,
/// NAT semantics): this-network, private, CGNAT, loopback, link-local, IETF protocol
/// assignments, benchmarking, multicast and reserved (including broadcast).
pub const NAT_BLOCKED_IPV4: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

/// Generate the `nftables` NAT ruleset: the guest may reach the public IPv4 internet
/// through `uplink` (masqueraded) and nothing else. Guest traffic to non-global IPv4
/// ranges (the uplink LAN, other VMs, link-local, multicast) and all guest IPv6 is
/// dropped; the input chain drops unsolicited guest traffic addressed to the host.
pub fn build_nat_nft_ruleset(alloc: &NetworkAllocation, uplink: &str) -> String {
    let subnet = format!("10.200.{}.0/30", alloc.subnet_slot);
    let blocked = NAT_BLOCKED_IPV4.join(", ");
    format!(
        "table inet {tbl} {{\n\
           chain forward {{\n\
             type filter hook forward priority 0; policy accept;\n\
             iifname \"{tap}\" meta nfproto ipv6 drop\n\
             iifname \"{tap}\" ip daddr {{ {blocked} }} drop\n\
             iifname \"{tap}\" oifname \"{uplink}\" ip saddr {subnet} accept\n\
             iifname \"{uplink}\" oifname \"{tap}\" ct state established,related accept\n\
             iifname \"{tap}\" drop\n\
             oifname \"{tap}\" drop\n\
           }}\n\
           chain input {{\n\
             type filter hook input priority 0; policy accept;\n\
             iifname \"{tap}\" ct state established,related accept\n\
             iifname \"{tap}\" drop\n\
           }}\n\
           chain postrouting {{\n\
             type nat hook postrouting priority srcnat; policy accept;\n\
             oifname \"{uplink}\" ip saddr {subnet} masquerade\n\
           }}\n\
         }}\n",
        tbl = alloc.nft_table,
        tap = alloc.tap_name,
        uplink = uplink,
        subnet = subnet,
        blocked = blocked,
    )
}

/// Generate an `nftables` host-only ruleset blocking all forwarding to/from the VM bridge.
pub fn build_host_only_nft_ruleset(alloc: &NetworkAllocation, bridge: &str) -> String {
    format!(
        "table inet {tbl} {{\n\
           chain forward {{\n\
             type filter hook forward priority 0; policy accept;\n\
             iifname \"{br}\" drop\n\
             oifname \"{br}\" drop\n\
           }}\n\
         }}\n",
        tbl = alloc.nft_table,
        br = bridge,
    )
}

impl NetworkController for LinuxNetworkController {
    fn setup(&self, vm_id: &str, mode: NetworkMode) -> Result<Option<NetworkAllocation>, RpcError> {
        if mode == NetworkMode::Isolated {
            return Ok(None);
        }

        let uplink = if mode == NetworkMode::Nat {
            Some(Self::detect_uplink()?)
        } else {
            None
        };

        let slot = {
            let mut guard = self.used_slots.lock().unwrap();
            allocate_slot(&mut guard)?
        };
        let mut alloc = build_allocation(vm_id, mode, slot);

        let setup_res = (|| -> Result<(), RpcError> {
            Self::run_ip(&["tuntap", "add", "dev", &alloc.tap_name, "mode", "tap"])?;

            match mode {
                NetworkMode::Nat => {
                    let uplink = uplink.as_deref().expect("uplink checked above");
                    Self::run_ip(&["addr", "add", &alloc.host_cidr, "dev", &alloc.tap_name])?;
                    Self::run_ip(&["link", "set", "dev", &alloc.tap_name, "up"])?;
                    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1\n");

                    let nft_rules = build_nat_nft_ruleset(&alloc, uplink);
                    Self::apply_nft_ruleset(&nft_rules)?;
                    alloc.ruleset_baseline = Some(Self::installed_baseline(&alloc.nft_table)?);
                }
                NetworkMode::HostOnly => {
                    let bridge = alloc.bridge_name.as_deref().expect("host-only bridge");
                    Self::run_ip(&["link", "add", "name", bridge, "type", "bridge"])?;
                    Self::run_ip(&["link", "set", "dev", &alloc.tap_name, "master", bridge])?;
                    Self::run_ip(&["addr", "add", &alloc.host_cidr, "dev", bridge])?;
                    Self::run_ip(&["link", "set", "dev", &alloc.tap_name, "up"])?;
                    Self::run_ip(&["link", "set", "dev", bridge, "up"])?;

                    let nft_rules = build_host_only_nft_ruleset(&alloc, bridge);
                    Self::apply_nft_ruleset(&nft_rules)?;
                    alloc.ruleset_baseline = Some(Self::installed_baseline(&alloc.nft_table)?);
                }
                NetworkMode::Isolated => {}
            }

            Ok(())
        })();

        if let Err(err) = setup_res {
            self.teardown(&alloc);
            return Err(err);
        }

        Ok(Some(alloc))
    }

    fn teardown(&self, alloc: &NetworkAllocation) {
        let _ = Command::new("nft")
            .args(["delete", "table", "inet", &alloc.nft_table])
            .output();
        let _ = Command::new("ip")
            .args(["link", "delete", "dev", &alloc.tap_name])
            .output();
        if let Some(ref bridge) = alloc.bridge_name {
            let _ = Command::new("ip")
                .args(["link", "delete", "dev", bridge])
                .output();
        }
        let mut guard = self.used_slots.lock().unwrap();
        guard.remove(&alloc.subnet_slot);
    }

    fn open_capture(&self, alloc: &NetworkAllocation) -> Result<Box<dyn PacketSource>, RpcError> {
        AfPacketSource::open(&alloc.tap_name)
            .map(|s| Box::new(s) as Box<dyn PacketSource>)
            .map_err(|_| {
                RpcError::new(
                    error_code::IO_FAILURE,
                    "failed to open packet capture socket",
                )
            })
    }

    fn isolation_intact(&self, alloc: &NetworkAllocation) -> bool {
        match (&alloc.ruleset_baseline, Self::list_table(&alloc.nft_table)) {
            (Some(baseline), Some(live)) => *baseline == live,
            _ => false,
        }
    }
}

/// In-memory mock network controller for `--dry-run` and CI tests (requires no `CAP_NET_ADMIN`).
#[derive(Default)]
pub struct MockNetworkController {
    used_slots: Mutex<HashSet<u8>>,
    active: Mutex<Vec<NetworkAllocation>>,
    torn_down: Mutex<Vec<NetworkAllocation>>,
    /// Frames queued per TAP name, consumed by mock capture sources.
    frames: Mutex<HashMap<String, FrameQueue>>,
    /// VMs whose firewall table a test has "tampered with".
    tampered: Mutex<HashSet<String>>,
}

/// Frames waiting to be read by one mock capture source.
type FrameQueue = Arc<Mutex<VecDeque<Vec<u8>>>>;

/// Mock capture source draining frames queued with `MockNetworkController::inject_frame`.
struct MockPacketSource(FrameQueue);

impl PacketSource for MockPacketSource {
    fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<Option<(usize, usize)>> {
        let next = self.0.lock().unwrap().pop_front();
        match next {
            Some(frame) => {
                let n = frame.len().min(buf.len());
                buf[..n].copy_from_slice(&frame[..n]);
                Ok(Some((n, frame.len())))
            }
            None => {
                std::thread::sleep(std::time::Duration::from_millis(5));
                Ok(None)
            }
        }
    }
}

impl MockNetworkController {
    /// Queue a frame as if it crossed the TAP of `vm_id`'s active allocation.
    /// Returns `false` if the VM has no NIC.
    pub fn inject_frame(&self, vm_id: &str, frame: Vec<u8>) -> bool {
        let Some(tap) = self
            .active
            .lock()
            .unwrap()
            .iter()
            .find(|a| a.vm_id == vm_id)
            .map(|a| a.tap_name.clone())
        else {
            return false;
        };
        self.frame_queue(&tap).lock().unwrap().push_back(frame);
        true
    }

    /// Simulate someone flushing or editing the VM's firewall table.
    pub fn tamper_isolation(&self, vm_id: &str) {
        self.tampered.lock().unwrap().insert(vm_id.to_string());
    }

    fn frame_queue(&self, tap: &str) -> FrameQueue {
        Arc::clone(
            self.frames
                .lock()
                .unwrap()
                .entry(tap.to_string())
                .or_default(),
        )
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn active_allocations(&self) -> Vec<NetworkAllocation> {
        self.active.lock().unwrap().clone()
    }

    pub fn torn_down_allocations(&self) -> Vec<NetworkAllocation> {
        self.torn_down.lock().unwrap().clone()
    }
}

impl NetworkController for MockNetworkController {
    fn setup(&self, vm_id: &str, mode: NetworkMode) -> Result<Option<NetworkAllocation>, RpcError> {
        if mode == NetworkMode::Isolated {
            return Ok(None);
        }
        let slot = {
            let mut guard = self.used_slots.lock().unwrap();
            allocate_slot(&mut guard)?
        };
        let mut alloc = build_allocation(vm_id, mode, slot);
        alloc.ruleset_baseline = Some(format!("mock table {}", alloc.nft_table));
        self.active.lock().unwrap().push(alloc.clone());
        Ok(Some(alloc))
    }

    fn teardown(&self, alloc: &NetworkAllocation) {
        self.used_slots.lock().unwrap().remove(&alloc.subnet_slot);
        self.active
            .lock()
            .unwrap()
            .retain(|a| a.tap_name != alloc.tap_name);
        self.frames.lock().unwrap().remove(&alloc.tap_name);
        self.torn_down.lock().unwrap().push(alloc.clone());
    }

    fn open_capture(&self, alloc: &NetworkAllocation) -> Result<Box<dyn PacketSource>, RpcError> {
        Ok(Box::new(MockPacketSource(
            self.frame_queue(&alloc.tap_name),
        )))
    }

    fn isolation_intact(&self, alloc: &NetworkAllocation) -> bool {
        alloc.ruleset_baseline.is_some() && !self.tampered.lock().unwrap().contains(&alloc.vm_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_network_isolates_and_allocates_non_overlapping_subnets() {
        let net = MockNetworkController::new();
        assert!(net
            .setup(
                "b6f51eed-8dc7-47f6-a8e7-81b6ae783a21",
                NetworkMode::Isolated
            )
            .unwrap()
            .is_none());

        let nat = net
            .setup("b6f51eed-8dc7-47f6-a8e7-81b6ae783a21", NetworkMode::Nat)
            .unwrap()
            .unwrap();
        assert!(nat.bridge_name.is_none());
        assert!(nat.tap_name.starts_with("hvmtap"));
        assert_eq!(nat.subnet_slot, 1);

        let host_only = net
            .setup(
                "4f281e57-3717-40fe-9d90-025f37962bbd",
                NetworkMode::HostOnly,
            )
            .unwrap()
            .unwrap();
        assert!(host_only.bridge_name.is_some());
        assert_eq!(host_only.subnet_slot, 2);
        assert_ne!(nat.host_cidr, host_only.host_cidr);

        net.teardown(&nat);
        net.teardown(&host_only);
        assert!(net.active_allocations().is_empty());
        assert_eq!(net.torn_down_allocations().len(), 2);
    }

    #[test]
    fn generates_strict_nftables_rulesets_and_validates_ifnames() {
        let nat_alloc =
            build_allocation("b6f51eed-8dc7-47f6-a8e7-81b6ae783a21", NetworkMode::Nat, 7);
        let nat_rules = build_nat_nft_ruleset(&nat_alloc, "eth0");
        assert!(nat_rules.contains("table inet hyperos_b6f51eed"));
        // Guest traffic to every non-global IPv4 range (uplink LAN, other VMs,
        // link-local, multicast) and all guest IPv6 is dropped before the accept.
        let drop_v4 = nat_rules
            .lines()
            .position(|l| l.contains("iifname \"hvmtapb6f51eed\" ip daddr {"))
            .expect("non-global IPv4 drop rule");
        let drop_v6 = nat_rules
            .lines()
            .position(|l| l.contains("iifname \"hvmtapb6f51eed\" meta nfproto ipv6 drop"))
            .expect("guest IPv6 drop rule");
        let accept = nat_rules
            .lines()
            .position(|l| l.contains("ip saddr 10.200.7.0/30 accept"))
            .expect("uplink accept rule");
        assert!(drop_v4 < accept && drop_v6 < accept);
        let drop_line = nat_rules.lines().nth(drop_v4).unwrap();
        for range in [
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16",
            "100.64.0.0/10",
            "224.0.0.0/4",
            "240.0.0.0/4",
        ] {
            assert!(drop_line.contains(range), "missing blocked range {range}");
        }
        assert!(drop_line.trim_end().ends_with("} drop"));
        assert!(!nat_rules.contains("!= 10.200.0.0/15"));
        // Host is unreachable from the guest except replies.
        assert!(nat_rules.contains("type filter hook input priority 0"));
        assert!(nat_rules.contains("ct state established,related accept"));
        assert!(nat_rules.contains("oifname \"eth0\" ip saddr 10.200.7.0/30 masquerade"));
        assert!(nat_rules.contains("iifname \"hvmtapb6f51eed\" drop"));

        let ho_alloc = build_allocation(
            "4f281e57-3717-40fe-9d90-025f37962bbd",
            NetworkMode::HostOnly,
            8,
        );
        let ho_rules =
            build_host_only_nft_ruleset(&ho_alloc, ho_alloc.bridge_name.as_deref().unwrap());
        assert!(ho_rules.contains("iifname \"hvmbr4f281e57\" drop"));
        assert!(ho_rules.contains("oifname \"hvmbr4f281e57\" drop"));
        assert!(!ho_rules.contains("masquerade"));

        assert!(LinuxNetworkController::is_valid_ifname("eth0"));
        assert!(LinuxNetworkController::is_valid_ifname("enp0s3"));
        assert!(!LinuxNetworkController::is_valid_ifname(""));
        assert!(!LinuxNetworkController::is_valid_ifname("eth0;rm -rf /"));
        assert!(!LinuxNetworkController::is_valid_ifname(
            "interface_name_too_long"
        ));
    }
}
