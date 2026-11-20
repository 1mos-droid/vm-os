//! Network sandboxing (`isolated`, `nat`, `host-only`) with rollback and teardown.
//!
//! All system invocations use `std::process::Command` with explicit argument arrays;
//! request values are never interpolated into a shell.

use crate::rpc::{error_code, NetworkMode, RpcError};
use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};
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
}

/// Abstract network sandbox controller implemented for Linux (`ip`/`nft`) and `--dry-run`.
pub trait NetworkController: Send + Sync {
    fn setup(&self, vm_id: &str, mode: NetworkMode) -> Result<Option<NetworkAllocation>, RpcError>;
    fn teardown(&self, alloc: &NetworkAllocation);
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
                if pair[0] == "dev" && !pair[1].is_empty() {
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
        let alloc = build_allocation(vm_id, mode, slot);

        let setup_res = (|| -> Result<(), RpcError> {
            Self::run_ip(&["tuntap", "add", "dev", &alloc.tap_name, "mode", "tap"])?;

            match mode {
                NetworkMode::Nat => {
                    let uplink = uplink.as_deref().expect("uplink checked above");
                    Self::run_ip(&["addr", "add", &alloc.host_cidr, "dev", &alloc.tap_name])?;
                    Self::run_ip(&["link", "set", "dev", &alloc.tap_name, "up"])?;
                    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1\n");

                    let subnet = format!("10.200.{}.0/30", alloc.subnet_slot);
                    let nft_rules = format!(
                        "table inet {tbl} {{\n\
                           chain forward {{\n\
                             type filter hook forward priority 0; policy accept;\n\
                             iifname \"{tap}\" oifname \"{uplink}\" ip saddr {subnet} ip daddr != 10.200.0.0/15 accept\n\
                             iifname \"{uplink}\" oifname \"{tap}\" ct state established,related accept\n\
                             iifname \"{tap}\" drop\n\
                             oifname \"{tap}\" drop\n\
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
                    );
                    Self::apply_nft_ruleset(&nft_rules)?;
                }
                NetworkMode::HostOnly => {
                    let bridge = alloc.bridge_name.as_deref().expect("host-only bridge");
                    Self::run_ip(&["link", "add", "name", bridge, "type", "bridge"])?;
                    Self::run_ip(&["link", "set", "dev", &alloc.tap_name, "master", bridge])?;
                    Self::run_ip(&["addr", "add", &alloc.host_cidr, "dev", bridge])?;
                    Self::run_ip(&["link", "set", "dev", &alloc.tap_name, "up"])?;
                    Self::run_ip(&["link", "set", "dev", bridge, "up"])?;

                    let nft_rules = format!(
                        "table inet {tbl} {{\n\
                           chain forward {{\n\
                             type filter hook forward priority 0; policy accept;\n\
                             iifname \"{br}\" drop\n\
                             oifname \"{br}\" drop\n\
                           }}\n\
                         }}\n",
                        tbl = alloc.nft_table,
                        br = bridge,
                    );
                    Self::apply_nft_ruleset(&nft_rules)?;
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
}

/// In-memory mock network controller for `--dry-run` and CI tests (requires no `CAP_NET_ADMIN`).
#[derive(Default)]
pub struct MockNetworkController {
    used_slots: Mutex<HashSet<u8>>,
    active: Mutex<Vec<NetworkAllocation>>,
    torn_down: Mutex<Vec<NetworkAllocation>>,
}

impl MockNetworkController {
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
        let alloc = build_allocation(vm_id, mode, slot);
        self.active.lock().unwrap().push(alloc.clone());
        Ok(Some(alloc))
    }

    fn teardown(&self, alloc: &NetworkAllocation) {
        self.used_slots.lock().unwrap().remove(&alloc.subnet_slot);
        self.active
            .lock()
            .unwrap()
            .retain(|a| a.tap_name != alloc.tap_name);
        self.torn_down.lock().unwrap().push(alloc.clone());
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
}
