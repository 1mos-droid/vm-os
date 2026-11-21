//! Low-overhead system status readers for the Top Context Rail (Host RAM, ZRAM, Battery, Wi-Fi, Audio).

use std::fs;
use std::path::Path;

/// Compact host telemetry shown in the Top Context Rail.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemStatus {
    pub ram_used_mib: u64,
    pub ram_total_mib: u64,
    pub zram_orig_mib: f64,
    pub zram_compr_mib: f64,
    pub zram_active: bool,
    pub battery_percent: Option<u8>,
    pub battery_charging: bool,
    pub wifi_enabled: bool,
    pub wifi_label: String,
    pub audio_muted: bool,
    pub audio_volume_pct: u8,
    pub display_scale_pct: u16,
    pub global_offline_shield: bool,
}

impl SystemStatus {
    pub fn read(mock_mode: bool, prev: Option<&Self>) -> Self {
        let (ram_used_mib, ram_total_mib) = read_ram_mib().unwrap_or((310, 4096));
        let (zram_orig_mib, zram_compr_mib, zram_active) = read_zram_mib(mock_mode);
        let (battery_percent, battery_charging) = read_battery(mock_mode);

        let wifi_enabled = prev.map(|p| p.wifi_enabled).unwrap_or(true);
        let audio_muted = prev.map(|p| p.audio_muted).unwrap_or(false);
        let audio_volume_pct = prev.map(|p| p.audio_volume_pct).unwrap_or(72);
        let display_scale_pct = prev.map(|p| p.display_scale_pct).unwrap_or(100);
        let global_offline_shield = prev.map(|p| p.global_offline_shield).unwrap_or(false);
        let wifi_label = if global_offline_shield || !wifi_enabled {
            "Offline".to_string()
        } else {
            "Wi-Fi".to_string()
        };

        Self {
            ram_used_mib,
            ram_total_mib,
            zram_orig_mib,
            zram_compr_mib,
            zram_active,
            battery_percent,
            battery_charging,
            wifi_enabled,
            wifi_label,
            audio_muted,
            audio_volume_pct,
            display_scale_pct,
            global_offline_shield,
        }
    }

    pub fn ram_ratio(&self) -> f32 {
        if self.ram_total_mib == 0 {
            0.0
        } else {
            (self.ram_used_mib as f32 / self.ram_total_mib as f32).clamp(0.0, 1.0)
        }
    }

    pub fn compact_memory_label(&self) -> String {
        if self.zram_active {
            format!(
                "RAM {} / {}M · ZRAM {:.0}M",
                self.ram_used_mib, self.ram_total_mib, self.zram_orig_mib
            )
        } else {
            format!("RAM {} / {}M", self.ram_used_mib, self.ram_total_mib)
        }
    }

    pub fn battery_label(&self) -> String {
        match self.battery_percent {
            Some(pct) if self.battery_charging => format!("{pct}% ⚡"),
            Some(pct) => format!("{pct}%"),
            None => "AC".to_string(),
        }
    }

    pub fn audio_label(&self) -> String {
        if self.audio_muted {
            "Muted".to_string()
        } else {
            format!("Vol {}%", self.audio_volume_pct)
        }
    }
}

fn read_ram_mib() -> Option<(u64, u64)> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kib = None;
    let mut avail_kib = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next()?;
        let val = parts.next()?.parse::<u64>().ok()?;
        match key {
            "MemTotal:" => total_kib = Some(val),
            "MemAvailable:" => avail_kib = Some(val),
            _ => {}
        }
    }
    let total = total_kib?;
    let avail = avail_kib.unwrap_or(0);
    Some((total.saturating_sub(avail) / 1024, total / 1024))
}

fn read_zram_mib(mock_mode: bool) -> (f64, f64, bool) {
    if let Ok(stat) = fs::read_to_string("/sys/block/zram0/mm_stat") {
        let nums: Vec<u64> = stat
            .split_whitespace()
            .take(2)
            .filter_map(|s| s.parse::<u64>().ok())
            .collect();
        if nums.len() == 2 {
            return (
                nums[0] as f64 / 1_048_576.0,
                nums[1] as f64 / 1_048_576.0,
                true,
            );
        }
    }
    if mock_mode {
        (142.0, 46.0, true)
    } else {
        (0.0, 0.0, false)
    }
}

fn read_battery(mock_mode: bool) -> (Option<u8>, bool) {
    let ps_dir = Path::new("/sys/class/power_supply");
    if let Ok(entries) = fs::read_dir(ps_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("BAT") {
                let cap = fs::read_to_string(entry.path().join("capacity"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u8>().ok());
                let charging = fs::read_to_string(entry.path().join("status"))
                    .map(|s| s.trim().eq_ignore_ascii_case("Charging"))
                    .unwrap_or(false);
                if cap.is_some() {
                    return (cap, charging);
                }
            }
        }
    }
    if mock_mode {
        (Some(84), false)
    } else {
        (None, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_status_mock_fallback_formats_clean_rail_labels() {
        let st = SystemStatus::read(true, None);
        assert!(st.ram_total_mib > 0);
        assert!(st.compact_memory_label().contains("RAM "));
        assert!(st.compact_memory_label().contains("ZRAM"));
        assert!(!st.battery_label().is_empty());
        assert_eq!(st.audio_label(), "Vol 72%");
    }
}
