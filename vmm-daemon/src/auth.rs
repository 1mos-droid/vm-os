//! Control-socket peer authorization (`ARCHITECTURE.md`, socket authorization).
//!
//! UID 0 (or the configured UID in `--dry-run`) is always allowed. When the `hyperos`
//! group exists, any peer whose primary or supplementary groups include it is also
//! allowed; membership grants every method.

use std::fs;

/// Group whose members may use the control socket.
pub const CONTROL_GROUP: &str = "hyperos";

/// Who may connect to the control socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerPolicy {
    pub uid: u32,
    pub gid: Option<u32>,
}

impl PeerPolicy {
    /// Allow exactly one UID and no group.
    pub fn uid_only(uid: u32) -> Self {
        Self { uid, gid: None }
    }

    /// Production policy: root, plus members of `hyperos` if that group exists.
    pub fn production() -> Self {
        let gid = fs::read_to_string("/etc/group")
            .ok()
            .and_then(|text| lookup_group_gid(&text, CONTROL_GROUP));
        Self { uid: 0, gid }
    }

    /// Decide whether a peer with these credentials may connect.
    pub fn allows(&self, uid: u32, gid: u32, supplementary: &[u32]) -> bool {
        if uid == self.uid {
            return true;
        }
        match self.gid {
            Some(g) => gid == g || supplementary.contains(&g),
            None => false,
        }
    }
}

/// Find `name`'s GID in `/etc/group` text (`name:password:gid:members`).
pub fn lookup_group_gid(group_file: &str, name: &str) -> Option<u32> {
    group_file.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != name {
            return None;
        }
        fields.next()?;
        fields.next()?.parse().ok()
    })
}

/// Parse the `Groups:` line of `/proc/<pid>/status` (supplementary GIDs).
pub fn parse_status_groups(status: &str) -> Vec<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Groups:"))
        .map(|rest| {
            rest.split_whitespace()
                .filter_map(|g| g.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Read a live process's supplementary groups; empty if the process is gone.
pub fn process_groups(pid: i32) -> Vec<u32> {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .map(|s| parse_status_groups(&s))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_up_group_and_parses_proc_groups() {
        let group = "root:x:0:\nkvm:x:78:\nhyperos:x:970:alice,bob\nhyperos2:x:971:\n";
        assert_eq!(lookup_group_gid(group, "hyperos"), Some(970));
        assert_eq!(lookup_group_gid(group, "kvm"), Some(78));
        assert_eq!(lookup_group_gid(group, "hyper"), None);
        assert_eq!(lookup_group_gid("hyperos:x:notanumber:\n", "hyperos"), None);

        let status =
            "Name:\tslint\nUid:\t1000\t1000\t1000\t1000\nGroups:\t10 970 1000 \nNgid:\t0\n";
        assert_eq!(parse_status_groups(status), vec![10, 970, 1000]);
        assert!(parse_status_groups("Groups:\t\n").is_empty());
        assert!(parse_status_groups("Name:\tx\n").is_empty());
    }

    #[test]
    fn allows_root_and_group_members_only() {
        let with_group = PeerPolicy {
            uid: 0,
            gid: Some(970),
        };
        assert!(with_group.allows(0, 0, &[]));
        assert!(with_group.allows(1000, 970, &[]));
        assert!(with_group.allows(1000, 1000, &[10, 970]));
        assert!(!with_group.allows(1000, 1000, &[10, 971]));

        let root_only = PeerPolicy::uid_only(0);
        assert!(root_only.allows(0, 0, &[]));
        assert!(!root_only.allows(1000, 970, &[970]));
    }
}
