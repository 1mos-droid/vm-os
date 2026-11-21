//! Zero-Script VM Studio domain models:
//! - Plain-English Network Access, Memory Profile (`virtio-balloon`), and Display Mode selectors
//! - Template Store catalog & staged download/provisioning progress
//! - Visual Checkpoints (`snapshot_vm` Save & Rewind cards)
//! - Guided Settings (Wi-Fi, Sound, Display scaling, and Btrfs CoW storage savings)

use crate::rpc::{NetworkMode, Snapshot, Vm};
use std::collections::HashMap;

/// Plain-English network sandbox presets mapped 1:1 to `ARCHITECTURE.md` `NetworkMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPreset {
    /// Zero network NIC (`isolated`)
    OfflineVault,
    /// Outbound internet only (`nat`)
    SafeWeb,
    /// Host-to-space private link (`host-only`)
    HomeNetwork,
}

impl NetworkPreset {
    pub fn from_index(idx: i32) -> Self {
        match idx {
            1 => Self::SafeWeb,
            2 => Self::HomeNetwork,
            _ => Self::OfflineVault,
        }
    }

    pub fn from_network_mode(mode: NetworkMode) -> Self {
        match mode {
            NetworkMode::Isolated => Self::OfflineVault,
            NetworkMode::Nat => Self::SafeWeb,
            NetworkMode::HostOnly => Self::HomeNetwork,
        }
    }

    pub fn to_network_mode(self) -> NetworkMode {
        match self {
            Self::OfflineVault => NetworkMode::Isolated,
            Self::SafeWeb => NetworkMode::Nat,
            Self::HomeNetwork => NetworkMode::HostOnly,
        }
    }

    pub fn plain_label(self) -> &'static str {
        match self {
            Self::OfflineVault => "Offline Vault",
            Self::SafeWeb => "Safe Web",
            Self::HomeNetwork => "Home Network",
        }
    }

    pub fn plain_description(self) -> &'static str {
        match self {
            Self::OfflineVault => {
                "Completely offline. Best for private documents, keys, and banking archives."
            }
            Self::SafeWeb => {
                "Internet access only. Cannot reach your host computer or other Spaces."
            }
            Self::HomeNetwork => "Private link to this computer only. No internet access.",
        }
    }
}

/// Plain-English memory profiles mapped directly to `virtio-balloon` min/max RAM + vCPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryProfile {
    Light,
    Standard,
    Heavy,
}

impl MemoryProfile {
    pub fn from_index(idx: i32) -> Self {
        match idx {
            0 => Self::Light,
            2 => Self::Heavy,
            _ => Self::Standard,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Standard => "Standard",
            Self::Heavy => "Heavy",
        }
    }

    /// Desired active RAM target (`set_balloon_ram.ram_mib`).
    pub fn min_ram_mib(self) -> u32 {
        match self {
            Self::Light => 256,
            Self::Standard => 512,
            Self::Heavy => 1024,
        }
    }

    /// Fixed crosvm ceiling (`create_vm.ram_mib`, strictly within `128..=3072`).
    pub fn max_ram_mib(self) -> u32 {
        match self {
            Self::Light => 512,
            Self::Standard => 1024,
            Self::Heavy => 2048,
        }
    }

    pub fn recommended_vcpus(self, host_cpus: u32) -> u32 {
        let wanted = match self {
            Self::Light => 1,
            Self::Standard => 2,
            Self::Heavy => 4,
        };
        wanted.min(host_cpus.max(1))
    }
}

/// Visual display integration mode for a Space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// Guest apps appear directly on the HyperOS desktop with 2px colored origin borders.
    SeamlessAppWindow,
    /// Guest opens as a self-contained desktop environment window.
    FullEnvironment,
}

impl DisplayMode {
    pub fn from_index(idx: i32) -> Self {
        match idx {
            1 => Self::FullEnvironment,
            _ => Self::SeamlessAppWindow,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SeamlessAppWindow => "Seamless App Window",
            Self::FullEnvironment => "Full Environment",
        }
    }
}

/// Download & provisioning state of a curated Template Store item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateStatus {
    Available,
    Downloading,
    Verifying,
    Installed,
}

impl TemplateStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::Downloading => "Downloading",
            Self::Verifying => "Verifying SHA-256",
            Self::Installed => "Ready",
        }
    }
}

/// Visual card entry in the Zero-Script Template Store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreTemplate {
    pub image_id: String,
    pub title: String,
    pub summary: String,
    pub download_mb: u32,
    pub logical_disk_gib: u32,
    pub recommended_profile: MemoryProfile,
    pub recommended_network: NetworkPreset,
    pub status: TemplateStatus,
    /// Progress percentage `0..=100`.
    pub progress_pct: u8,
}

/// Default curated Template Store catalog matching `guest-images/` builders.
pub fn default_store_catalog(installed_image_ids: &[String]) -> Vec<StoreTemplate> {
    let mut items = vec![
        StoreTemplate {
            image_id: "alpine-3-21-virtio".into(),
            title: "Alpine Quick Space".into(),
            summary:
                "Ultra-lightweight instant environment for disposable browsing and offline vaults."
                    .into(),
            download_mb: 84,
            logical_disk_gib: 2,
            recommended_profile: MemoryProfile::Light,
            recommended_network: NetworkPreset::OfflineVault,
            status: TemplateStatus::Available,
            progress_pct: 0,
        },
        StoreTemplate {
            image_id: "debian-12-slim-virtio".into(),
            title: "Debian Everyday Workstation".into(),
            summary:
                "Full-featured Linux apps, documents, browser, and Wayland/PipeWire audio ready."
                    .into(),
            download_mb: 240,
            logical_disk_gib: 4,
            recommended_profile: MemoryProfile::Standard,
            recommended_network: NetworkPreset::SafeWeb,
            status: TemplateStatus::Available,
            progress_pct: 0,
        },
        StoreTemplate {
            image_id: "linux-demo".into(),
            title: "Privacy Vault Minimal".into(),
            summary: "Compact air-gapped document and key storage Space with zero network stack."
                .into(),
            download_mb: 64,
            logical_disk_gib: 2,
            recommended_profile: MemoryProfile::Light,
            recommended_network: NetworkPreset::OfflineVault,
            status: TemplateStatus::Available,
            progress_pct: 0,
        },
        StoreTemplate {
            image_id: "browser-sandbox-virtio".into(),
            title: "Disposable Web Sandbox".into(),
            summary: "Hardened one-click browser image built for self-destructing web sessions."
                .into(),
            download_mb: 165,
            logical_disk_gib: 3,
            recommended_profile: MemoryProfile::Standard,
            recommended_network: NetworkPreset::SafeWeb,
            status: TemplateStatus::Available,
            progress_pct: 0,
        },
    ];

    for item in &mut items {
        if installed_image_ids.iter().any(|id| id == &item.image_id) {
            item.status = TemplateStatus::Installed;
            item.progress_pct = 100;
        }
    }
    items
}

/// Advance active Template Store download/verify progress bars (used in `--mock` provisioning).
/// Returns IDs of templates that just completed verification and became `Installed`.
pub fn advance_store_downloads(catalog: &mut [StoreTemplate], step_pct: u8) -> Vec<String> {
    let mut newly_installed = Vec::new();
    for item in catalog.iter_mut() {
        match item.status {
            TemplateStatus::Downloading => {
                let next = item.progress_pct.saturating_add(step_pct);
                if next >= 90 {
                    item.progress_pct = 92;
                    item.status = TemplateStatus::Verifying;
                } else {
                    item.progress_pct = next;
                }
            }
            TemplateStatus::Verifying => {
                item.progress_pct = 100;
                item.status = TemplateStatus::Installed;
                newly_installed.push(item.image_id.clone());
            }
            TemplateStatus::Available | TemplateStatus::Installed => {}
        }
    }
    newly_installed
}

/// Visual checkpoint card stored per VM (backed by `snapshot_vm`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointCard {
    pub snapshot_id: String,
    pub vm_id: String,
    pub vm_name: String,
    pub label: String,
    pub created_at: String,
}

impl CheckpointCard {
    pub fn from_snapshot(snap: Snapshot, vm_name: impl Into<String>, index: usize) -> Self {
        Self {
            snapshot_id: snap.snapshot_id,
            vm_id: snap.vm_id,
            vm_name: vm_name.into(),
            label: format!("Checkpoint #{index}"),
            created_at: snap.created_at,
        }
    }
}

/// Guided Settings state for non-technical users.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidedSettings {
    pub wifi_available: bool,
    pub wifi_enabled: bool,
    pub wifi_network_name: String,
    pub airplane_isolation: bool,
    pub audio_available: bool,
    pub sound_volume_pct: u8,
    pub sound_muted: bool,
    pub guest_audio_forwarding: bool,
    pub display_scale_pct: u16,
    pub smooth_transitions: bool,
}

impl GuidedSettings {
    /// Build initial settings honoring live Linux hardware capabilities (or full preview in `--mock`).
    pub fn for_runtime(mock_mode: bool) -> Self {
        let wifi_available = mock_mode || host_has_wifi();
        let audio_available = mock_mode || host_has_audio();
        Self {
            wifi_available,
            wifi_enabled: wifi_available,
            wifi_network_name: if wifi_available {
                "Home-5G".into()
            } else {
                "Unavailable (kernel profile)".into()
            },
            airplane_isolation: false,
            audio_available,
            sound_volume_pct: if audio_available { 75 } else { 0 },
            sound_muted: !audio_available,
            guest_audio_forwarding: audio_available,
            display_scale_pct: 100,
            smooth_transitions: true,
        }
    }
}

impl Default for GuidedSettings {
    fn default() -> Self {
        Self::for_runtime(true)
    }
}

fn host_has_wifi() -> bool {
    if let Ok(entries) = std::fs::read_dir("/sys/class/ieee80211") {
        if entries.flatten().next().is_some() {
            return true;
        }
    }
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            if entry.path().join("wireless").is_dir() {
                return true;
            }
        }
    }
    false
}

fn host_has_audio() -> bool {
    let Ok(cards) = std::fs::read_to_string("/proc/asound/cards") else {
        return false;
    };
    cards.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with(|c: char| c.is_ascii_digit()) && trimmed.contains('[')
    })
}

/// Human-friendly Btrfs CoW storage savings readout for Guided Settings.
#[derive(Debug, Clone, PartialEq)]
pub struct CowStorageSummary {
    pub unique_base_images: usize,
    pub cloned_spaces: usize,
    pub logical_total_gib: f64,
    pub actual_used_gib: f64,
    pub saved_gib: f64,
    pub savings_pct: u8,
}

/// Compute clear Btrfs reflink (`cp --reflink=always`) disk savings across active Spaces.
pub fn compute_cow_storage_savings(vms: &[Vm], catalog: &[StoreTemplate]) -> CowStorageSummary {
    let mut size_by_image: HashMap<&str, f64> = HashMap::new();
    for t in catalog {
        size_by_image.insert(t.image_id.as_str(), t.logical_disk_gib as f64);
    }

    let mut base_set = std::collections::HashSet::new();
    let mut logical_total_gib = 0.0;
    for vm in vms {
        base_set.insert(vm.base_image_id.as_str());
        logical_total_gib += *size_by_image.get(vm.base_image_id.as_str()).unwrap_or(&2.0);
    }

    // One physical copy per unique base image (~0.25 GiB compressed on Btrfs zstd)
    // plus small copy-on-write delta (~0.12 GiB) per cloned instance.
    let base_physical_gib: f64 = base_set
        .iter()
        .map(|id| {
            catalog
                .iter()
                .find(|t| t.image_id == *id)
                .map(|t| t.download_mb as f64 / 1024.0)
                .unwrap_or(0.25)
        })
        .sum();
    let deltas_gib = (vms.len() as f64) * 0.12;
    let actual_used_gib = (base_physical_gib + deltas_gib).min(logical_total_gib);
    let saved_gib = (logical_total_gib - actual_used_gib).max(0.0);
    let savings_pct = if logical_total_gib > 0.01 {
        ((saved_gib / logical_total_gib) * 100.0)
            .round()
            .clamp(0.0, 99.0) as u8
    } else {
        0
    };

    CowStorageSummary {
        unique_base_images: base_set.len(),
        cloned_spaces: vms.len(),
        logical_total_gib,
        actual_used_gib,
        saved_gib,
        savings_pct,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::VmState;

    #[test]
    fn plain_english_presets_map_cleanly_to_architecture_contract() {
        assert_eq!(
            NetworkPreset::OfflineVault.to_network_mode(),
            NetworkMode::Isolated
        );
        assert_eq!(NetworkPreset::SafeWeb.to_network_mode(), NetworkMode::Nat);
        assert_eq!(
            NetworkPreset::HomeNetwork.to_network_mode(),
            NetworkMode::HostOnly
        );

        assert_eq!(MemoryProfile::Light.min_ram_mib(), 256);
        assert_eq!(MemoryProfile::Light.max_ram_mib(), 512);
        assert_eq!(MemoryProfile::Standard.min_ram_mib(), 512);
        assert_eq!(MemoryProfile::Standard.max_ram_mib(), 1024);
        assert_eq!(MemoryProfile::Heavy.min_ram_mib(), 1024);
        assert_eq!(MemoryProfile::Heavy.max_ram_mib(), 2048);
    }

    #[test]
    fn template_store_download_advances_through_verifying_to_installed() {
        let mut catalog = default_store_catalog(&["alpine-3-21-virtio".into()]);
        assert_eq!(catalog[0].status, TemplateStatus::Installed);
        assert_eq!(catalog[3].status, TemplateStatus::Available);

        catalog[3].status = TemplateStatus::Downloading;
        let done1 = advance_store_downloads(&mut catalog, 50);
        assert!(done1.is_empty());
        assert_eq!(catalog[3].status, TemplateStatus::Downloading);
        assert_eq!(catalog[3].progress_pct, 50);

        let done2 = advance_store_downloads(&mut catalog, 50);
        assert!(done2.is_empty());
        assert_eq!(catalog[3].status, TemplateStatus::Verifying);

        let done3 = advance_store_downloads(&mut catalog, 50);
        assert_eq!(done3, vec!["browser-sandbox-virtio".to_string()]);
        assert_eq!(catalog[3].status, TemplateStatus::Installed);
        assert_eq!(catalog[3].progress_pct, 100);
    }

    #[test]
    fn cow_storage_summary_reports_btrfs_reflink_savings() {
        let catalog = default_store_catalog(&[]);
        let vms = vec![
            Vm {
                vm_id: "1".into(),
                name: "Work".into(),
                base_image_id: "debian-12-slim-virtio".into(),
                vcpu_count: 2,
                ram_mib: 1024,
                balloon_target_mib: 512,
                network_mode: NetworkMode::Nat,
                state: VmState::Running,
            },
            Vm {
                vm_id: "2".into(),
                name: "Study".into(),
                base_image_id: "debian-12-slim-virtio".into(),
                vcpu_count: 2,
                ram_mib: 1024,
                balloon_target_mib: 512,
                network_mode: NetworkMode::Nat,
                state: VmState::Stopped,
            },
        ];
        let summary = compute_cow_storage_savings(&vms, &catalog);
        assert_eq!(summary.unique_base_images, 1);
        assert_eq!(summary.cloned_spaces, 2);
        assert_eq!(summary.logical_total_gib, 8.0);
        assert!(summary.saved_gib > 7.0);
        assert!(summary.savings_pct >= 85);
    }
}
