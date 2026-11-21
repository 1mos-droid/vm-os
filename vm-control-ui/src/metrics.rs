//! Host RAM (`/proc/meminfo`) and ZRAM (`/sys/block/zram0/mm_stat`) telemetry.

use crate::rpc::{HOST_RAM_RESERVE_MIB, MAX_VCPU_COUNT, MIN_VCPU_COUNT};
use std::fs;
use std::path::Path;

pub const PROC_MEMINFO_PATH: &str = "/proc/meminfo";
pub const ZRAM_MM_STAT_PATH: &str = "/sys/block/zram0/mm_stat";
pub const ZRAM_DISKSIZE_PATH: &str = "/sys/block/zram0/disksize";
pub const ZRAM_COMP_ALG_PATH: &str = "/sys/block/zram0/comp_algorithm";

/// Parsed host RAM summary in KiB / MiB (`used = MemTotal - MemAvailable`, per `ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamMetrics {
    pub total_kib: u64,
    pub available_kib: u64,
    pub used_kib: u64,
    pub simulated: bool,
}

impl RamMetrics {
    pub fn total_mib(&self) -> u64 {
        self.total_kib / 1024
    }

    pub fn available_mib(&self) -> u64 {
        self.available_kib / 1024
    }

    pub fn used_mib(&self) -> u64 {
        self.used_kib / 1024
    }

    /// Fraction of RAM used in `[0.0, 1.0]`.
    pub fn usage_ratio(&self) -> f32 {
        if self.total_kib == 0 {
            return 0.0;
        }
        (self.used_kib as f64 / self.total_kib as f64).clamp(0.0, 1.0) as f32
    }

    /// Admission budget per `ARCHITECTURE.md`: `MemTotal - 768 MiB`.
    pub fn vm_admission_budget_mib(&self) -> u64 {
        self.total_mib().saturating_sub(HOST_RAM_RESERVE_MIB)
    }
}

/// Parsed ZRAM telemetry from `/sys/block/zram0/mm_stat` (+ `disksize` & `comp_algorithm`).
#[derive(Debug, Clone, PartialEq)]
pub struct ZramMetrics {
    pub orig_data_bytes: u64,
    pub compr_data_bytes: u64,
    pub mem_used_total_bytes: u64,
    pub disksize_bytes: u64,
    pub comp_algorithm: String,
    pub available: bool,
    pub simulated: bool,
}

impl ZramMetrics {
    pub fn orig_data_mib(&self) -> f64 {
        self.orig_data_bytes as f64 / 1_048_576.0
    }

    pub fn compr_data_mib(&self) -> f64 {
        self.compr_data_bytes as f64 / 1_048_576.0
    }

    pub fn mem_used_total_mib(&self) -> f64 {
        self.mem_used_total_bytes as f64 / 1_048_576.0
    }

    pub fn disksize_mib(&self) -> u64 {
        self.disksize_bytes / 1_048_576
    }

    /// Compression ratio `orig_data_size / compr_data_size` (or `1.0` when empty).
    pub fn compression_ratio(&self) -> f64 {
        if self.compr_data_bytes == 0 || self.orig_data_bytes == 0 {
            1.0
        } else {
            self.orig_data_bytes as f64 / self.compr_data_bytes as f64
        }
    }

    /// Fraction of configured ZRAM disksize currently storing uncompressed pages.
    pub fn usage_ratio(&self) -> f32 {
        if self.disksize_bytes == 0 {
            return 0.0;
        }
        (self.orig_data_bytes as f64 / self.disksize_bytes as f64).clamp(0.0, 1.0) as f32
    }
}

/// Combined host telemetry snapshot rendered on the Dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct HostMetrics {
    pub ram: RamMetrics,
    pub zram: ZramMetrics,
    pub online_cpus: u32,
}

/// Parse `/proc/meminfo` text.
/// Computes `used = MemTotal - MemAvailable` exactly as specified in `ARCHITECTURE.md`.
pub fn parse_proc_meminfo(contents: &str) -> Result<RamMetrics, String> {
    let mut total_kib: Option<u64> = None;
    let mut available_kib: Option<u64> = None;
    let mut free_kib: Option<u64> = None;

    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        let Some(val_str) = parts.next() else {
            continue;
        };
        let Ok(val) = val_str.parse::<u64>() else {
            continue;
        };
        match key {
            "MemTotal:" => total_kib = Some(val),
            "MemAvailable:" => available_kib = Some(val),
            "MemFree:" => free_kib = Some(val),
            _ => {}
        }
    }

    let total = total_kib.ok_or_else(|| "MemTotal missing from /proc/meminfo".to_string())?;
    let available = available_kib
        .or(free_kib)
        .ok_or_else(|| "MemAvailable/MemFree missing from /proc/meminfo".to_string())?;
    let used = total.saturating_sub(available);

    Ok(RamMetrics {
        total_kib: total,
        available_kib: available,
        used_kib: used,
        simulated: false,
    })
}

/// Parse `/sys/block/zram0/mm_stat` text:
/// `orig_data_size compr_data_size mem_used_total mem_limit mem_used_max same_pages pages_compacted [huge_pages ...]`
pub fn parse_zram_mm_stat(
    mm_stat: &str,
    disksize_bytes: u64,
    comp_algorithm: String,
) -> Result<ZramMetrics, String> {
    let nums: Vec<u64> = mm_stat
        .split_whitespace()
        .take(3)
        .map(|s| s.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse zram mm_stat integers: {e}"))?;

    if nums.len() < 3 {
        return Err("zram mm_stat has fewer than 3 columns".to_string());
    }

    Ok(ZramMetrics {
        orig_data_bytes: nums[0],
        compr_data_bytes: nums[1],
        mem_used_total_bytes: nums[2],
        disksize_bytes,
        comp_algorithm,
        available: true,
        simulated: false,
    })
}

/// Extract active algorithm from `/sys/block/zram0/comp_algorithm` (e.g. `lzo [zstd] lz4`).
pub fn parse_zram_comp_algorithm(contents: &str) -> String {
    for token in contents.split_whitespace() {
        if let Some(inner) = token.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            return inner.to_string();
        }
    }
    contents
        .split_whitespace()
        .next()
        .unwrap_or("zstd")
        .to_string()
}

/// Read live host RAM and ZRAM telemetry, with graceful fallback in `--mock` mode.
pub fn read_host_metrics(mock_mode: bool, active_mock_ram_mib: u64) -> HostMetrics {
    let online_cpus = std::thread::available_parallelism()
        .map(|n| (n.get() as u32).clamp(MIN_VCPU_COUNT, MAX_VCPU_COUNT))
        .unwrap_or(4);

    let ram = match fs::read_to_string(PROC_MEMINFO_PATH)
        .ok()
        .and_then(|text| parse_proc_meminfo(&text).ok())
    {
        Some(real) if !mock_mode => real,
        Some(real) => real,
        None => {
            // Fallback simulated 4 GiB HyperOS host profile (base ~240 MiB + active VMs).
            let total_kib = 4096 * 1024;
            let used_mib = (240 + active_mock_ram_mib).min(3900);
            let used_kib = used_mib * 1024;
            RamMetrics {
                total_kib,
                available_kib: total_kib.saturating_sub(used_kib),
                used_kib,
                simulated: true,
            }
        }
    };

    let zram = read_zram_metrics(mock_mode, ram.total_kib, active_mock_ram_mib);

    HostMetrics {
        ram,
        zram,
        online_cpus,
    }
}

fn read_zram_metrics(mock_mode: bool, mem_total_kib: u64, active_mock_ram_mib: u64) -> ZramMetrics {
    if Path::new(ZRAM_MM_STAT_PATH).exists() {
        if let Ok(mm_stat) = fs::read_to_string(ZRAM_MM_STAT_PATH) {
            let disksize = fs::read_to_string(ZRAM_DISKSIZE_PATH)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or_else(|| mem_total_kib * 1024 * 65 / 100);
            let alg = fs::read_to_string(ZRAM_COMP_ALG_PATH)
                .map(|s| parse_zram_comp_algorithm(&s))
                .unwrap_or_else(|_| "zstd".to_string());
            if let Ok(parsed) = parse_zram_mm_stat(&mm_stat, disksize, alg) {
                return parsed;
            }
        }
    }

    if mock_mode {
        // HyperOS configures ZRAM to 65% of MemTotal with zstd (ARCHITECTURE.md).
        let disksize_bytes = mem_total_kib * 1024 * 65 / 100;
        let orig_mib = (96 + active_mock_ram_mib / 8).min(disksize_bytes / 1_048_576);
        let orig_bytes = orig_mib * 1_048_576;
        // ~3.1x typical zstd compression ratio.
        let compr_bytes = (orig_bytes * 10) / 31;
        let mem_used_total_bytes = compr_bytes + 4 * 1_048_576;
        ZramMetrics {
            orig_data_bytes: orig_bytes,
            compr_data_bytes: compr_bytes,
            mem_used_total_bytes,
            disksize_bytes,
            comp_algorithm: "zstd".to_string(),
            available: true,
            simulated: true,
        }
    } else {
        ZramMetrics {
            orig_data_bytes: 0,
            compr_data_bytes: 0,
            mem_used_total_bytes: 0,
            disksize_bytes: 0,
            comp_algorithm: "unavailable".to_string(),
            available: false,
            simulated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_meminfo_exact_formula() {
        let sample = "\
MemTotal:        4096000 kB
MemFree:         3500000 kB
MemAvailable:    3800000 kB
Buffers:           12000 kB
Cached:           280000 kB
";
        let m = parse_proc_meminfo(sample).unwrap();
        assert_eq!(m.total_kib, 4_096_000);
        assert_eq!(m.available_kib, 3_800_000);
        assert_eq!(m.used_kib, 296_000);
        assert!(!m.simulated);
        assert_eq!(m.total_mib(), 4000);
        assert_eq!(m.vm_admission_budget_mib(), 4000 - 768);
    }

    #[test]
    fn parses_zram_mm_stat_and_comp_algorithm() {
        // orig=256 MiB, compr=80 MiB, mem_used_total=84 MiB
        let mm_stat = "268435456 83886080 88080384 0 134217728 1024 0 0 0\n";
        let alg = parse_zram_comp_algorithm("lzo-rle lzo lz4 [zstd] deflate");
        assert_eq!(alg, "zstd");

        let disksize = 2_662 * 1_048_576;
        let z = parse_zram_mm_stat(mm_stat, disksize, alg).unwrap();
        assert_eq!(z.orig_data_bytes, 268_435_456);
        assert_eq!(z.compr_data_bytes, 83_886_080);
        assert_eq!(z.mem_used_total_bytes, 88_080_384);
        assert_eq!(z.comp_algorithm, "zstd");
        assert!(z.available);
        assert!(!z.simulated);
        assert!((z.compression_ratio() - 3.2).abs() < 0.01);
    }

    #[test]
    fn mock_zram_fallback_uses_65_percent_zstd_profile() {
        let z = read_zram_metrics(true, 4096 * 1024, 512);
        assert!(z.available);
        assert_eq!(z.comp_algorithm, "zstd");
        if z.simulated {
            // 65% of 4096 MiB = 2662 MiB
            assert_eq!(z.disksize_mib(), 4096 * 65 / 100);
            assert!(z.compression_ratio() > 2.5);
        }
    }
}
