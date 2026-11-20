//! Guest base image template discovery from `/var/lib/hyperos/base-images/<image_id>/manifest.json`.

use crate::rpc::{is_valid_image_id, is_valid_sha256_hex, BaseImageManifest};
use std::fs;
use std::path::Path;

/// A selectable guest template presented in the Create Controlled VM Wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestTemplate {
    pub image_id: String,
    pub label: String,
    pub description: String,
    pub recommended_min_ram_mib: u32,
    pub recommended_max_ram_mib: u32,
    pub recommended_vcpus: u32,
}

/// Built-in default templates used in `--mock` mode (or when no base images are imported yet).
pub fn default_mock_templates() -> Vec<GuestTemplate> {
    vec![
        GuestTemplate {
            image_id: "alpine-3-21-virtio".to_string(),
            label: "Alpine Linux 3.21 (virtio)".to_string(),
            description: "Minimal musl guest with virtio-balloon, virtio-fs, and vsock Wayland/PipeWire".to_string(),
            recommended_min_ram_mib: 256,
            recommended_max_ram_mib: 1024,
            recommended_vcpus: 1,
        },
        GuestTemplate {
            image_id: "debian-12-slim-virtio".to_string(),
            label: "Debian 12 Slim (virtio)".to_string(),
            description: "Debian bookworm minimal workstation guest with virtio-balloon, virtio-fs, and vsock".to_string(),
            recommended_min_ram_mib: 512,
            recommended_max_ram_mib: 1536,
            recommended_vcpus: 2,
        },
        GuestTemplate {
            image_id: "linux-demo".to_string(),
            label: "Linux Demo (ARCHITECTURE.md)".to_string(),
            description: "Minimal contract reference base image (raw sparse disk)".to_string(),
            recommended_min_ram_mib: 256,
            recommended_max_ram_mib: 1024,
            recommended_vcpus: 1,
        },
    ]
}

/// Discover validated base images under `/var/lib/hyperos/base-images/<image_id>/`.
///
/// Validates each entry against `ARCHITECTURE.md`:
/// - Directory name matches `manifest.json`'s `image_id` (1-64 ASCII `[A-Za-z0-9_-]`)
/// - `sha256` is 64 lowercase hex characters
/// - `format == "raw"`
/// - `disk.raw` exists as a regular file inside the base image directory
pub fn discover_templates(base_images_dir: &Path, mock_mode: bool) -> Vec<GuestTemplate> {
    let mut found = Vec::new();

    if let Ok(entries) = fs::read_dir(base_images_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !is_valid_image_id(dir_name) {
                continue;
            }
            let manifest_path = path.join("manifest.json");
            let disk_path = path.join("disk.raw");
            if !disk_path.is_file() {
                continue;
            }
            let Ok(text) = fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<BaseImageManifest>(&text) else {
                continue;
            };
            if manifest.image_id != dir_name
                || manifest.format != "raw"
                || !is_valid_sha256_hex(&manifest.sha256)
            {
                continue;
            }
            let short_sha = &manifest.sha256[..12];
            found.push(GuestTemplate {
                image_id: manifest.image_id.clone(),
                label: manifest.image_id,
                description: format!("Imported raw Btrfs base image (sha256:{short_sha}...)"),
                recommended_min_ram_mib: 256,
                recommended_max_ram_mib: 1024,
                recommended_vcpus: 1,
            });
        }
    }

    found.sort_by(|a, b| a.image_id.cmp(&b.image_id));

    if found.is_empty() && mock_mode {
        default_mock_templates()
    } else if found.is_empty() {
        // Still allow manual/contract template selection even before importing base images.
        default_mock_templates()
    } else {
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_valid_base_image_manifest_and_skips_invalid() {
        let dir = std::env::temp_dir().join(format!("hyperos-images-{}", uuid::Uuid::new_v4()));
        let valid_dir = dir.join("alpine-3-21-virtio");
        fs::create_dir_all(&valid_dir).unwrap();
        fs::write(valid_dir.join("disk.raw"), b"sparse").unwrap();
        fs::write(
            valid_dir.join("manifest.json"),
            format!(
                r#"{{"image_id":"alpine-3-21-virtio","sha256":"{}","format":"raw"}}"#,
                "a".repeat(64)
            ),
        )
        .unwrap();

        // Invalid format should be skipped.
        let bad_dir = dir.join("bad-qcow");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("disk.raw"), b"sparse").unwrap();
        fs::write(
            bad_dir.join("manifest.json"),
            format!(
                r#"{{"image_id":"bad-qcow","sha256":"{}","format":"qcow2"}}"#,
                "b".repeat(64)
            ),
        )
        .unwrap();

        let templates = discover_templates(&dir, false);
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].image_id, "alpine-3-21-virtio");

        let _ = fs::remove_dir_all(&dir);
    }
}
