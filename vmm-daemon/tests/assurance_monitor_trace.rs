//! Replays a trace through the generated Ogma/Copilot monitor. Its own test
//! binary (= process): the monitor's H(P) state is global and permanent.

use vmm_daemon::assurance::{sample, HyperosSample};

fn ids(observation: HyperosSample) -> Vec<&'static str> {
    sample(&observation).iter().map(|r| r.id).collect()
}

#[test]
fn each_requirement_trips_on_its_violation_and_stays_violated() {
    let healthy = HyperosSample {
        nat_spaces_running: 1,
        nat_spaces_lan_blocked: 1,
        reserved_ram_mib: 1024,
        ram_budget_mib: 3328,
        largest_capture_bytes: 4096,
    };
    assert!(ids(healthy).is_empty());

    // HYP002: more RAM reserved than the admission budget.
    assert_eq!(
        ids(HyperosSample {
            reserved_ram_mib: 3329,
            ..healthy
        }),
        ["HYP002"]
    );
    // H(P) is permanent: back to healthy values, HYP002 still reported.
    assert_eq!(ids(healthy), ["HYP002"]);

    // HYP003: a capture one byte over 256 MiB.
    assert_eq!(
        ids(HyperosSample {
            largest_capture_bytes: 268_435_457,
            ..healthy
        }),
        ["HYP002", "HYP003"]
    );

    // HYP001: a running NAT Space whose firewall table is no longer intact.
    assert_eq!(
        ids(HyperosSample {
            nat_spaces_running: 2,
            ..healthy
        }),
        ["HYP001", "HYP002", "HYP003"]
    );
}
