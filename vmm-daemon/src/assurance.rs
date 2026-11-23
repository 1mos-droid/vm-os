//! Runtime assurance: the host's isolation and resource invariants, written in
//! NASA FRET (`assurance/requirements/host.json`), turned into a constant-memory
//! C monitor by NASA Ogma and Copilot, and fed by `VmManager`.
//!
//! The monitor checks past-time formulas of the form `H(P)` ("P has held at every
//! sample so far"), so a violation is permanent for the life of the process; the
//! daemon fails closed until it is restarted.

use std::sync::Mutex;

/// One observation of the host, in the order of the generated monitor's inputs.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HyperosSample {
    pub nat_spaces_running: i64,
    pub nat_spaces_lan_blocked: i64,
    pub reserved_ram_mib: i64,
    pub ram_budget_mib: i64,
    pub largest_capture_bytes: i64,
}

/// A monitored requirement. `REQUIREMENTS[i]` is reported by bit `i` of the shim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirement {
    pub id: &'static str,
    pub fretish: &'static str,
}

pub const REQUIREMENTS: [Requirement; 3] = [
    Requirement {
        id: "HYP001",
        fretish: "host shall always satisfy nat_spaces_lan_blocked = nat_spaces_running",
    },
    Requirement {
        id: "HYP002",
        fretish: "host shall always satisfy reserved_ram_mib <= ram_budget_mib",
    },
    Requirement {
        id: "HYP003",
        fretish: "host shall always satisfy largest_capture_bytes <= 268435456",
    },
];

extern "C" {
    fn hyperos_monitor_sample(sample: *const HyperosSample) -> u32;
}

/// The generated monitor keeps static state; one sample at a time.
static MONITOR: Mutex<()> = Mutex::new(());

/// Feed one sample to the monitor and return the requirements it reports as
/// violated at this step.
pub fn sample(observation: &HyperosSample) -> Vec<&'static Requirement> {
    let bits = {
        let _guard = MONITOR.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: the pointer is valid for the call and the struct is #[repr(C)]
        // with the same field order as `struct hyperos_sample` in shim.c.
        unsafe { hyperos_monitor_sample(observation) }
    };
    REQUIREMENTS
        .iter()
        .enumerate()
        .filter(|(i, _)| bits & (1 << i) != 0)
        .map(|(_, r)| r)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Rust table must describe exactly the requirements the monitor was
    /// generated from, in the same order.
    #[test]
    fn requirement_table_matches_fret_source() {
        let src: serde_json::Value =
            serde_json::from_str(include_str!("../../assurance/requirements/host.json")).unwrap();
        let reqs = src["requirements"].as_array().unwrap();
        assert_eq!(reqs.len(), REQUIREMENTS.len());
        for (json, rust) in reqs.iter().zip(REQUIREMENTS.iter()) {
            assert_eq!(json["id"], rust.id);
            assert_eq!(json["fretish"], rust.fretish);
        }
    }

    /// A healthy trace never trips any monitor. (Violating traces live in
    /// tests/assurance_*.rs: the monitor's state is per process and permanent.)
    #[test]
    fn healthy_trace_reports_no_violation() {
        let trace = [
            HyperosSample {
                ram_budget_mib: 3328,
                ..Default::default()
            },
            HyperosSample {
                nat_spaces_running: 2,
                nat_spaces_lan_blocked: 2,
                reserved_ram_mib: 2048,
                ram_budget_mib: 3328,
                largest_capture_bytes: 268_435_456,
            },
            HyperosSample {
                nat_spaces_running: 0,
                nat_spaces_lan_blocked: 0,
                reserved_ram_mib: 3328,
                ram_budget_mib: 3328,
                largest_capture_bytes: 24,
            },
        ];
        for s in &trace {
            assert!(sample(s).is_empty(), "unexpected violation for {s:?}");
        }
    }
}
