//! Window origin attribution, semantic security color palette, and 2px border + badge layout.

use serde::{Deserialize, Serialize};

/// Exact border width drawn around every managed window to signify its owning VM or Host.
pub const ORIGIN_BORDER_PX: i32 = 2;

/// Height of the subtle origin badge pill rendered in the top-left of every window frame.
pub const ORIGIN_BADGE_HEIGHT_PX: i32 = 22;

/// RGB color tuple for compositor rendering and Slint chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Packed `0xAARRGGBB` pixel for software / Pixman framebuffer blitting.
    pub const fn to_argb8888(self, alpha: u8) -> u32 {
        ((alpha as u32) << 24) | ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Curated, high-contrast Workspace-First accent palette.
pub mod palette {
    use super::Rgb;

    /// Host system windows: Crisp Titanium.
    pub const HOST_TITANIUM: Rgb = Rgb::new(148, 163, 184);
    /// Offline Vault (`isolated`): Amber Shield.
    pub const VAULT_AMBER: Rgb = Rgb::new(245, 158, 11);
    /// Disposable Browser / Safe Web (`nat`): Cyan Pulse.
    pub const WEB_CYAN: Rgb = Rgb::new(6, 182, 212);
    /// Work Pod: Emerald Mint.
    pub const WORK_EMERALD: Rgb = Rgb::new(16, 185, 129);
    /// Home Network (`host-only`): Violet Spark.
    pub const HOME_VIOLET: Rgb = Rgb::new(139, 92, 246);
    /// Secondary custom spaces: Rose Coral.
    pub const SPACE_ROSE: Rgb = Rgb::new(244, 63, 94);
}

/// Security domain owning a Wayland surface / window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowOrigin {
    /// Native HyperOS host surface (e.g., VM Studio, System Settings).
    Host,
    /// Surface forwarded from a `crosvm` guest VM over Wayland/virtio-vsock or virtio-gpu.
    Vm {
        vm_id: String,
        vm_name: String,
        network_mode: String,
        disposable: bool,
    },
}

impl WindowOrigin {
    /// Deterministic accent color communicating trust/network boundary at a glance.
    pub fn accent_color(&self) -> Rgb {
        match self {
            Self::Host => palette::HOST_TITANIUM,
            Self::Vm {
                vm_name,
                network_mode,
                disposable,
                ..
            } => {
                let lower = vm_name.to_ascii_lowercase();
                if network_mode == "isolated"
                    || lower.contains("vault")
                    || lower.contains("offline")
                {
                    palette::VAULT_AMBER
                } else if *disposable || lower.contains("browser") || lower.contains("disposable") {
                    palette::WEB_CYAN
                } else if lower.contains("work") || lower.contains("pod") || lower.contains("dev") {
                    palette::WORK_EMERALD
                } else if network_mode == "host-only" {
                    palette::HOME_VIOLET
                } else {
                    palette::SPACE_ROSE
                }
            }
        }
    }

    /// Short plain-English badge text shown in the subtle top-left origin pill.
    pub fn badge_label(&self) -> String {
        match self {
            Self::Host => "HOST".to_string(),
            Self::Vm {
                vm_name,
                network_mode,
                disposable,
                ..
            } => {
                let net_tag = match network_mode.as_str() {
                    "isolated" => "Offline",
                    "nat" => "NAT · Internet Only",
                    "host-only" => "Host-Only",
                    other => other,
                };
                if *disposable {
                    format!("{vm_name} · {net_tag} · Disposable")
                } else {
                    format!("{vm_name} · {net_tag}")
                }
            }
        }
    }

    /// Identifier used to group windows into Workspace-First Context Rail tabs.
    pub fn space_key(&self) -> &str {
        match self {
            Self::Host => "host",
            Self::Vm { vm_id, .. } => vm_id.as_str(),
        }
    }
}

/// Outer frame geometry (2px colored border + subtle top-left origin badge pill)
/// computed around a client content rectangle `(x, y, width, height)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginChromeRects {
    pub border_x: i32,
    pub border_y: i32,
    pub border_w: i32,
    pub border_h: i32,
    pub border_thickness: i32,
    pub badge_x: i32,
    pub badge_y: i32,
    pub badge_w: i32,
    pub badge_h: i32,
}

/// Compute exact outer 2px border and top-left origin badge rectangles for a client surface.
pub fn compute_origin_chrome(
    content_x: i32,
    content_y: i32,
    content_w: i32,
    content_h: i32,
    label_chars: usize,
) -> OriginChromeRects {
    let badge_w = ((label_chars as i32) * 7 + 20).clamp(64, content_w.max(64));
    OriginChromeRects {
        border_x: content_x - ORIGIN_BORDER_PX,
        border_y: content_y - ORIGIN_BORDER_PX,
        border_w: content_w + ORIGIN_BORDER_PX * 2,
        border_h: content_h + ORIGIN_BORDER_PX * 2,
        border_thickness: ORIGIN_BORDER_PX,
        badge_x: content_x + 8,
        badge_y: content_y - ORIGIN_BADGE_HEIGHT_PX / 2,
        badge_w,
        badge_h: ORIGIN_BADGE_HEIGHT_PX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_assigns_distinct_security_colors_and_2px_border() {
        let host = WindowOrigin::Host;
        assert_eq!(host.accent_color(), palette::HOST_TITANIUM);
        assert_eq!(host.badge_label(), "HOST");

        let vault = WindowOrigin::Vm {
            vm_id: "11111111-1111-1111-1111-111111111111".into(),
            vm_name: "Offline Vault".into(),
            network_mode: "isolated".into(),
            disposable: false,
        };
        assert_eq!(vault.accent_color(), palette::VAULT_AMBER);
        assert_eq!(vault.badge_label(), "Offline Vault · Offline");

        let browser = WindowOrigin::Vm {
            vm_id: "22222222-2222-2222-2222-222222222222".into(),
            vm_name: "Disposable Browser".into(),
            network_mode: "nat".into(),
            disposable: true,
        };
        assert_eq!(browser.accent_color(), palette::WEB_CYAN);
        assert_eq!(
            browser.badge_label(),
            "Disposable Browser · NAT · Internet Only · Disposable"
        );

        let chrome = compute_origin_chrome(100, 80, 640, 480, browser.badge_label().len());
        assert_eq!(chrome.border_thickness, 2);
        assert_eq!(chrome.border_x, 98);
        assert_eq!(chrome.border_y, 78);
        assert_eq!(chrome.border_w, 644);
        assert_eq!(chrome.border_h, 484);
    }
}
