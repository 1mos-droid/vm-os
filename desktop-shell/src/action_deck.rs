//! `Super`-key Action Deck state: instant search, one-click environment launchers, and quick hardware toggles.

use crate::vmm_bridge::{EnvironmentPreset, VmSummary};

/// A searchable entry displayed in the Action Deck results list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckEntry {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub category: &'static str,
    pub accent_hex: String,
}

/// Curated host apps searchable from the Action Deck.
const HOST_APPS: &[(&str, &str, &str)] = &[
    (
        "app:vm-studio",
        "VM Studio",
        "Create, checkpoint, and manage Spaces visually",
    ),
    (
        "app:files",
        "Shared Files",
        "Browse host & virtio-fs shared folders",
    ),
    (
        "app:settings",
        "Guided Settings",
        "Wi-Fi, sound, display scaling & Btrfs storage",
    ),
    (
        "app:reclaim",
        "Reclaim RAM Now",
        "Deflate idle VM balloons to free host memory",
    ),
];

/// Stateful controller for the `Super`-key Action Deck overlay.
#[derive(Debug, Default)]
pub struct ActionDeckState {
    pub visible: bool,
    pub query: String,
}

impl ActionDeckState {
    pub fn toggle(&mut self) -> bool {
        self.visible = !self.visible;
        if !self.visible {
            self.query.clear();
        }
        self.visible
    }

    pub fn open(&mut self) {
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.query.clear();
    }

    /// Filter instant presets, active VMs, and host tools by case-insensitive substring match.
    pub fn filtered_entries(&self, active_vms: &[VmSummary]) -> Vec<DeckEntry> {
        let q = self.query.trim().to_ascii_lowercase();
        let mut results = Vec::new();

        for preset in EnvironmentPreset::ALL {
            let matches = q.is_empty()
                || preset.title().to_ascii_lowercase().contains(&q)
                || preset.subtitle().to_ascii_lowercase().contains(&q)
                || preset.network_mode().contains(&q);
            if matches {
                results.push(DeckEntry {
                    id: format!("preset:{}", preset.id()),
                    title: preset.title().to_string(),
                    subtitle: preset.subtitle().to_string(),
                    category: "Instant Space",
                    accent_hex: preset.accent_hex().to_string(),
                });
            }
        }

        for vm in active_vms {
            let matches = q.is_empty()
                || vm.name.to_ascii_lowercase().contains(&q)
                || vm.network_mode.to_ascii_lowercase().contains(&q);
            if matches {
                results.push(DeckEntry {
                    id: format!("vm:{}", vm.vm_id),
                    title: format!("Switch to {}", vm.name),
                    subtitle: format!(
                        "{} · {} / {} MiB RAM · {}",
                        vm.state, vm.balloon_target_mib, vm.ram_mib, vm.network_mode
                    ),
                    category: "Active Space",
                    accent_hex: "#38bdf8".to_string(),
                });
            }
        }

        for &(id, title, subtitle) in HOST_APPS {
            let matches = q.is_empty()
                || title.to_ascii_lowercase().contains(&q)
                || subtitle.to_ascii_lowercase().contains(&q);
            if matches {
                results.push(DeckEntry {
                    id: id.to_string(),
                    title: title.to_string(),
                    subtitle: subtitle.to_string(),
                    category: "System",
                    accent_hex: "#94a3b8".to_string(),
                });
            }
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmm_bridge::{MockShellVmmBridge, ShellVmmBridge};

    #[test]
    fn action_deck_toggles_and_filters_presets_vms_and_apps() {
        let mut deck = ActionDeckState::default();
        assert!(!deck.visible);
        assert!(deck.toggle());
        assert!(deck.visible);

        let vms = MockShellVmmBridge::with_sample_spaces().list_vms().unwrap();
        let all = deck.filtered_entries(&vms);
        assert!(all.len() >= 10);

        deck.query = "vault".to_string();
        let filtered = deck.filtered_entries(&vms);
        assert!(filtered.iter().any(|e| e.title == "Offline Vault"));
        assert!(filtered
            .iter()
            .any(|e| e.title == "Switch to Offline Vault"));
        assert!(!filtered.iter().any(|e| e.title == "Disposable Browser"));

        deck.close();
        assert!(!deck.visible);
        assert!(deck.query.is_empty());
    }
}
