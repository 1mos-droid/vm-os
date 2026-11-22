//! HyperOS Workspace-First Wayland Desktop Shell binary (`desktop-shell`).
//!
//! Combines a Smithay 0.7 Wayland compositor with a lightweight software-rendered Slint
//! Top Context Rail and `Super`-key Action Deck targeting <100 MB total RAM usage.

use desktop_shell::compositor::{accept_pending_clients, HyperShellState};
use desktop_shell::layout::{Rect, SnapZone, TOP_RAIL_HEIGHT_PX};
use desktop_shell::origin::{Rgb, WindowOrigin};
use desktop_shell::sys_status::SystemStatus;
use desktop_shell::theme::{load_theme_or_fallback, parse_hex_rgb, resolve_theme_path, ThemeFile};
use desktop_shell::vmm_bridge::{
    format_capture_bytes, launch_environment_preset, network_badge_label, reclaim_idle_ram,
    CaptureSummary, EnvironmentPreset, MockShellVmmBridge, ShellVmmBridge, SnapshotSummary,
    UnixVmmBridge, VmSummary, DEFAULT_VMM_SOCKET,
};
use slint::{Color, ModelRc, SharedString, VecModel};
#[cfg(feature = "winit")]
use slint::{Timer, TimerMode};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
#[cfg(feature = "winit")]
use std::time::Duration;
use std::time::Instant;

slint::include_modules!();

struct ShellCli {
    mock: bool,
    headless: bool,
    kms: bool,
    vmm_socket: PathBuf,
    wayland_socket: Option<String>,
    theme: Option<PathBuf>,
}

fn parse_cli() -> Result<Option<ShellCli>, String> {
    let mut mock = false;
    let mut headless = false;
    let mut kms = false;
    let mut vmm_socket = PathBuf::from(DEFAULT_VMM_SOCKET);
    let mut wayland_socket = None;
    let mut theme = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mock" => mock = true,
            "--headless" => headless = true,
            "--kms" => kms = true,
            "--socket" => {
                let p = args
                    .next()
                    .ok_or_else(|| "--socket requires a path".to_string())?;
                vmm_socket = PathBuf::from(p);
            }
            "--wayland-socket" => {
                let name = args
                    .next()
                    .ok_or_else(|| "--wayland-socket requires a socket name".to_string())?;
                wayland_socket = Some(name);
            }
            "--theme" => {
                let p = args
                    .next()
                    .ok_or_else(|| "--theme requires a JSON theme file path".to_string())?;
                theme = Some(PathBuf::from(p));
            }
            "-h" | "--help" => {
                println!(
                    "HyperOS Workspace-First Desktop Shell (Smithay + Slint)\n\
                     \n\
                     USAGE:\n\
                       desktop-shell [OPTIONS]\n\
                     \n\
                     OPTIONS:\n\
                       --mock                    Run with in-memory mock VMs and telemetry fallback\n\
                       --headless                Run Smithay Wayland compositor without opening a display\n\
                       --kms                     Force bare-metal DRM/KMS + libinput backend\n\
                       --socket <PATH>           vmm-daemon Unix socket (default: {DEFAULT_VMM_SOCKET})\n\
                       --wayland-socket <NAME>   Wayland listening socket name (default: auto wayland-hyperos-*)\n\
                       --theme <PATH>            Load a JSON token theme file (or set HYPEROS_THEME)\n\
                       -h, --help                Print this help message"
                );
                return Ok(None);
            }
            other => return Err(format!("unknown argument: {other} (try --help)")),
        }
    }

    Ok(Some(ShellCli {
        mock,
        headless,
        kms,
        vmm_socket,
        wayland_socket,
        theme,
    }))
}

struct ShellRuntime {
    compositor: HyperShellState,
    bridge: Box<dyn ShellVmmBridge>,
    mock_mode: bool,
    vms: Vec<VmSummary>,
    snapshots: Vec<SnapshotSummary>,
    captures: Vec<CaptureSummary>,
    vmm_connected: bool,
    vmm_error: Option<String>,
    sys: SystemStatus,
}

fn rgb_to_slint(c: Rgb) -> Color {
    Color::from_rgb_u8(c.r, c.g, c.b)
}

fn theme_color(hex: &str) -> Color {
    let (r, g, b) = parse_hex_rgb(hex).expect("validated ThemeFile must hold #RRGGBB colors");
    Color::from_rgb_u8(r, g, b)
}

fn apply_theme_to_ui(ui: &HyperShellWindow, theme: &ThemeFile) {
    let g = ui.global::<Theme>();
    g.set_id(SharedString::from(theme.id.as_str()));
    g.set_name(SharedString::from(theme.name.as_str()));
    g.set_layout_preset(SharedString::from(theme.layout_preset.as_str()));
    g.set_density_level(theme.layout_preset.density_level());
    g.set_window_bg(theme_color(&theme.colors.window_bg));
    g.set_canvas_bg(theme_color(&theme.colors.canvas_bg));
    g.set_surface_bg(theme_color(&theme.colors.surface_bg));
    g.set_surface_elevated(theme_color(&theme.colors.surface_elevated));
    g.set_border(theme_color(&theme.colors.border));
    g.set_text_primary(theme_color(&theme.colors.text_primary));
    g.set_text_secondary(theme_color(&theme.colors.text_secondary));
    g.set_text_muted(theme_color(&theme.colors.text_muted));
    g.set_accent(theme_color(&theme.colors.accent));
    g.set_danger(theme_color(&theme.colors.danger));
    g.set_success(theme_color(&theme.colors.success));
    g.set_ui_font_family(SharedString::from(theme.fonts.ui_family.as_str()));
    g.set_mono_font_family(SharedString::from(theme.fonts.mono_family.as_str()));
    g.set_font_base(theme.fonts.base_size_px as f32);
    g.set_font_heading(theme.fonts.heading_size_px as f32);
    g.set_radius_sm(theme.radii.sm_px as f32);
    g.set_radius_md(theme.radii.md_px as f32);
    g.set_radius_lg(theme.radii.lg_px as f32);
    g.set_radius_pill(theme.radii.pill_px as f32);
    g.set_space_xs(theme.spacing.xs_px as f32);
    g.set_space_sm(theme.spacing.sm_px as f32);
    g.set_space_md(theme.spacing.md_px as f32);
    g.set_space_lg(theme.spacing.lg_px as f32);
    g.invoke_sync_palette(!theme.is_light_surface());
}

fn hex_to_slint(hex: &str) -> Color {
    let clean = hex.trim_start_matches('#');
    if clean.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&clean[0..2], 16),
            u8::from_str_radix(&clean[2..4], 16),
            u8::from_str_radix(&clean[4..6], 16),
        ) {
            return Color::from_rgb_u8(r, g, b);
        }
    }
    Color::from_rgb_u8(148, 163, 184)
}

fn sync_shell_ui(ui: &HyperShellWindow, rt: &mut ShellRuntime) {
    match rt.bridge.list_vms() {
        Ok(vms) => {
            rt.vms = vms;
            rt.vmm_connected = true;
            rt.vmm_error = None;
            let mut snaps = Vec::new();
            let mut caps = Vec::new();
            for vm in &rt.vms {
                if let Ok(mut s) = rt.bridge.list_snapshots(&vm.vm_id) {
                    snaps.append(&mut s);
                }
                if let Ok(mut c) = rt.bridge.list_captures(&vm.vm_id) {
                    caps.append(&mut c);
                }
            }
            rt.snapshots = snaps;
            rt.captures = caps;
        }
        Err(err) => {
            rt.vmm_connected = false;
            rt.vmm_error = Some(err);
        }
    }
    ui.set_vmm_connected(rt.vmm_connected);
    ui.set_vmm_error(SharedString::from(rt.vmm_error.as_deref().unwrap_or("")));
    rt.sys = SystemStatus::read(rt.mock_mode, Some(&rt.sys));

    // Sync Top Context Rail VM Space pills
    let pills: Vec<SpacePill> = rt
        .vms
        .iter()
        .map(|vm| {
            let origin = WindowOrigin::Vm {
                vm_id: vm.vm_id.clone(),
                vm_name: vm.name.clone(),
                network_mode: vm.network_mode.clone(),
                disposable: vm.disposable,
            };
            let capturing = rt
                .captures
                .iter()
                .any(|c| c.vm_id == vm.vm_id && c.state == "running");
            SpacePill {
                vm_id: SharedString::from(vm.vm_id.as_str()),
                name: SharedString::from(vm.name.as_str()),
                ram_label: SharedString::from(format!("{}M", vm.balloon_target_mib)),
                network_badge: SharedString::from(network_badge_label(&vm.network_mode)),
                disposable: vm.disposable,
                capturing,
                accent: rgb_to_slint(origin.accent_color()),
                active: rt.compositor.active_space_key == vm.vm_id,
                running: vm.state == "running",
            }
        })
        .collect();
    ui.set_space_pills(ModelRc::new(VecModel::from(pills)));

    // Sync Active Space Research Controls in Action Deck
    let selected_vm = rt
        .vms
        .iter()
        .find(|v| v.vm_id == rt.compositor.active_space_key)
        .or_else(|| rt.vms.first());
    if let Some(vm) = selected_vm {
        let capture_supported = vm.network_mode != "isolated";
        let active_cap = rt
            .captures
            .iter()
            .find(|c| c.vm_id == vm.vm_id && c.state == "running");
        let capture_status = if let Some(c) = active_cap {
            format!("● Capturing · {}", format_capture_bytes(c.bytes))
        } else if !capture_supported {
            "Capture N/A (isolated)".to_string()
        } else if let Some(latest) = rt.captures.iter().rfind(|c| c.vm_id == vm.vm_id) {
            format!("{} · {}", latest.state, format_capture_bytes(latest.bytes))
        } else {
            "Capture Idle".to_string()
        };

        ui.set_selected_vm_id(SharedString::from(vm.vm_id.as_str()));
        ui.set_selected_vm_name(SharedString::from(vm.name.as_str()));
        ui.set_selected_vm_state(SharedString::from(vm.state.as_str()));
        ui.set_selected_vm_network_badge(SharedString::from(network_badge_label(&vm.network_mode)));
        ui.set_selected_vm_disposable(vm.disposable);
        ui.set_selected_capture_supported(capture_supported);
        ui.set_selected_capture_active(active_cap.is_some());
        ui.set_selected_capture_status(SharedString::from(capture_status));

        let snap_items: Vec<ShellSnapshotItem> = rt
            .snapshots
            .iter()
            .filter(|s| s.vm_id == vm.vm_id)
            .map(|s| ShellSnapshotItem {
                snapshot_id: SharedString::from(s.snapshot_id.as_str()),
                short_id: SharedString::from(s.snapshot_id.chars().take(8).collect::<String>()),
                vm_id: SharedString::from(s.vm_id.as_str()),
                created_at: SharedString::from(s.created_at.as_str()),
                can_restore: !vm.disposable && vm.state == "stopped",
            })
            .collect();
        ui.set_selected_snapshots(ModelRc::new(VecModel::from(snap_items)));

        let cap_items: Vec<ShellCaptureItem> = rt
            .captures
            .iter()
            .filter(|c| c.vm_id == vm.vm_id)
            .map(|c| ShellCaptureItem {
                capture_id: SharedString::from(c.capture_id.as_str()),
                short_id: SharedString::from(c.capture_id.chars().take(8).collect::<String>()),
                vm_id: SharedString::from(c.vm_id.as_str()),
                started_at: SharedString::from(c.started_at.as_str()),
                state: SharedString::from(c.state.as_str()),
                bytes_label: SharedString::from(format_capture_bytes(c.bytes)),
            })
            .collect();
        ui.set_selected_captures(ModelRc::new(VecModel::from(cap_items)));
    } else {
        ui.set_selected_vm_id(SharedString::from(""));
        ui.set_selected_snapshots(ModelRc::new(
            VecModel::from(Vec::<ShellSnapshotItem>::new()),
        ));
        ui.set_selected_captures(ModelRc::new(VecModel::from(Vec::<ShellCaptureItem>::new())));
    }

    // Sync system status pills
    ui.set_host_zram_label(SharedString::from(rt.sys.compact_memory_label()));
    ui.set_host_ram_ratio(rt.sys.ram_ratio());
    ui.set_wifi_enabled(rt.sys.wifi_enabled);
    ui.set_wifi_label(SharedString::from(rt.sys.wifi_label.as_str()));
    ui.set_audio_muted(rt.sys.audio_muted);
    ui.set_audio_label(SharedString::from(rt.sys.audio_label()));
    ui.set_battery_label(SharedString::from(rt.sys.battery_label()));
    ui.set_display_scale_pct(rt.sys.display_scale_pct as i32);
    ui.set_global_offline_shield(rt.sys.global_offline_shield);

    // Sync managed compositor windows (sorted by focus so focused draws on top)
    let mut win_list: Vec<_> = rt.compositor.windows.values().cloned().collect();
    win_list.sort_by_key(|w| (w.focused, w.id));
    let cards: Vec<ShellWindowCard> = win_list
        .into_iter()
        .map(|w| ShellWindowCard {
            window_id: w.id as i32,
            title: SharedString::from(w.title.as_str()),
            badge_label: SharedString::from(w.origin.badge_label()),
            accent: rgb_to_slint(w.origin.accent_color()),
            x: w.rect.x as f32,
            y: w.rect.y as f32,
            width: w.rect.w as f32,
            height: w.rect.h as f32,
            focused: w.focused,
            snap_label: SharedString::from(w.snap_zone.map(|z| z.label()).unwrap_or("")),
        })
        .collect();
    ui.set_windows(ModelRc::new(VecModel::from(cards)));

    // Sync Action Deck search results
    let entries: Vec<DeckResultItem> = rt
        .compositor
        .action_deck
        .filtered_entries(&rt.vms)
        .into_iter()
        .map(|e| DeckResultItem {
            entry_id: SharedString::from(e.id),
            title: SharedString::from(e.title),
            subtitle: SharedString::from(e.subtitle),
            category: SharedString::from(e.category),
            accent: hex_to_slint(&e.accent_hex),
        })
        .collect();
    ui.set_deck_results(ModelRc::new(VecModel::from(entries)));
    ui.set_action_deck_visible(rt.compositor.action_deck.visible);
}

fn seed_mock_workspace_windows(compositor: &mut HyperShellState, vms: &[VmSummary]) {
    let (out_w, out_h) = compositor.output_size;
    if let Some(work) = vms.first() {
        let origin = WindowOrigin::Vm {
            vm_id: work.vm_id.clone(),
            vm_name: work.name.clone(),
            network_mode: work.network_mode.clone(),
            disposable: work.disposable,
        };
        let rect = SnapZone::LeftHalf.target_rect(out_w, out_h);
        let id = compositor.register_managed_window(
            "Documents & Notes",
            "work-docs",
            origin,
            rect,
            None,
        );
        if let Some(w) = compositor.windows.get_mut(&id) {
            w.snap_zone = Some(SnapZone::LeftHalf);
        }
    }
    if let Some(browser) = vms.get(1) {
        let origin = WindowOrigin::Vm {
            vm_id: browser.vm_id.clone(),
            vm_name: browser.name.clone(),
            network_mode: browser.network_mode.clone(),
            disposable: browser.disposable,
        };
        let rect = SnapZone::RightHalf.target_rect(out_w, out_h);
        let id = compositor.register_managed_window(
            "Private Web Session",
            "web-browser",
            origin,
            rect,
            None,
        );
        if let Some(w) = compositor.windows.get_mut(&id) {
            w.snap_zone = Some(SnapZone::RightHalf);
        }
    }
    compositor.register_managed_window(
        "HyperOS VM Studio",
        "vm-control-ui",
        WindowOrigin::Host,
        Rect::new(340, TOP_RAIL_HEIGHT_PX + 80, 560, 380),
        None,
    );
}

fn wire_shell(ui: &HyperShellWindow, rt: Rc<RefCell<ShellRuntime>>) {
    sync_shell_ui(ui, &mut rt.borrow_mut());

    // Super-key Action Deck toggle
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_toggle_action_deck(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                st.compositor.handle_super_key();
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    // Live Action Deck search
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_deck_query_changed(move |q| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                st.compositor.action_deck.query = q.to_string();
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    // Activate a VM Space tab on Top Context Rail
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_activate_space(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                st.compositor.active_space_key = vm_id.to_string();
                let target_win = st
                    .compositor
                    .windows
                    .values()
                    .find(|w| w.origin.space_key() == vm_id.as_str())
                    .map(|w| w.id);
                if let Some(wid) = target_win {
                    st.compositor.focus_window(wid);
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    // "+ New Space" opens Action Deck ready to launch or open VM Studio
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_new_space_clicked(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                st.compositor.action_deck.open();
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    // "Reclaim RAM" deflates idle VM balloons via set_balloon_ram
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_reclaim_ram_clicked(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                let focused_key = st.compositor.active_space_key.clone();
                let focused = if focused_key == "host" {
                    None
                } else {
                    Some(focused_key.as_str())
                };
                match reclaim_idle_ram(st.bridge.as_mut(), focused) {
                    Ok(report) => {
                        let msg = if report.reclaimed_mib > 0 {
                            format!(
                                "Reclaimed {} MiB from {} active Space(s)",
                                report.reclaimed_mib, report.deflated_vms
                            )
                        } else {
                            "All active Spaces are already at minimum RAM".to_string()
                        };
                        ui.set_rail_toast(SharedString::from(msg));
                    }
                    Err(err) => {
                        ui.set_rail_toast(SharedString::from(format!("Reclaim failed: {err}")));
                    }
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    // One-click preset launcher (Disposable Browser, Work Pod, Offline Vault)
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_launch_preset(move |preset_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let preset = match preset_id.as_str() {
                    "disposable-browser" => EnvironmentPreset::DisposableBrowser,
                    "work-pod" => EnvironmentPreset::WorkPod,
                    _ => EnvironmentPreset::OfflineVault,
                };
                let force_disp = if ui.get_deck_launch_disposable() {
                    Some(true)
                } else {
                    None
                };
                let mut st = rt.borrow_mut();
                match launch_environment_preset(st.bridge.as_mut(), preset, force_disp) {
                    Ok(vm) => {
                        let origin = WindowOrigin::Vm {
                            vm_id: vm.vm_id.clone(),
                            vm_name: vm.name.clone(),
                            network_mode: vm.network_mode.clone(),
                            disposable: vm.disposable,
                        };
                        let spawn_rect = Rect::new(220, TOP_RAIL_HEIGHT_PX + 70, 620, 420);
                        let wid = st.compositor.register_managed_window(
                            preset.title(),
                            preset.id(),
                            origin,
                            spawn_rect,
                            None,
                        );
                        st.compositor
                            .snap_window(wid, SnapZone::RightHalf, Instant::now());
                        st.compositor.active_space_key = vm.vm_id;
                        st.compositor.action_deck.close();
                        ui.set_deck_launch_disposable(false);
                        ui.set_rail_toast(SharedString::from(format!(
                            "Launched {} ({} MiB{})",
                            preset.title(),
                            vm.ram_mib,
                            if vm.disposable { " · Disposable" } else { "" }
                        )));
                    }
                    Err(err) => {
                        ui.set_rail_toast(SharedString::from(format!("Launch failed: {err}")));
                    }
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    // Active Space Research Controls (Start / Force Stop / Snapshot Save, Restore, Delete / Packet Capture)
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_start_vm_action(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                match st.bridge.start_vm(vm_id.as_str()) {
                    Ok(vm) => ui
                        .set_rail_toast(SharedString::from(format!("Started Space '{}'", vm.name))),
                    Err(err) => {
                        ui.set_rail_toast(SharedString::from(format!("Start failed: {err}")))
                    }
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_confirm_stop_vm_action(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                match st.bridge.stop_vm(vm_id.as_str()) {
                    Ok(vm) => {
                        let extra = if vm.disposable {
                            " (disposable disk reset)"
                        } else {
                            ""
                        };
                        ui.set_rail_toast(SharedString::from(format!(
                            "Closed Space '{}'{}",
                            vm.name, extra
                        )));
                    }
                    Err(err) => {
                        ui.set_rail_toast(SharedString::from(format!("Stop failed: {err}")))
                    }
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_snapshot_vm_action(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                match st.bridge.snapshot_vm(vm_id.as_str()) {
                    Ok(snap) => ui.set_rail_toast(SharedString::from(format!(
                        "Saved snapshot {}",
                        &snap.snapshot_id[..8.min(snap.snapshot_id.len())]
                    ))),
                    Err(err) => {
                        ui.set_rail_toast(SharedString::from(format!("Snapshot failed: {err}")))
                    }
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_confirm_restore_snapshot_action(move |vm_id, snap_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                match st.bridge.restore_snapshot(vm_id.as_str(), snap_id.as_str()) {
                    Ok(vm) => ui.set_rail_toast(SharedString::from(format!(
                        "Restored snapshot onto '{}'",
                        vm.name
                    ))),
                    Err(err) => {
                        ui.set_rail_toast(SharedString::from(format!("Restore failed: {err}")))
                    }
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_delete_snapshot_action(move |vm_id, snap_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                match st.bridge.delete_snapshot(vm_id.as_str(), snap_id.as_str()) {
                    Ok(()) => ui.set_rail_toast(SharedString::from("Deleted snapshot")),
                    Err(err) => {
                        ui.set_rail_toast(SharedString::from(format!("Delete failed: {err}")))
                    }
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_start_capture_action(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                match st.bridge.start_capture(vm_id.as_str()) {
                    Ok(cap) => ui.set_rail_toast(SharedString::from(format!(
                        "Capturing packets ({})",
                        &cap.capture_id[..8.min(cap.capture_id.len())]
                    ))),
                    Err(err) => {
                        ui.set_rail_toast(SharedString::from(format!("Capture failed: {err}")))
                    }
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_stop_capture_action(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                match st.bridge.stop_capture(vm_id.as_str()) {
                    Ok(cap) => ui.set_rail_toast(SharedString::from(format!(
                        "Stopped capture ({}, {})",
                        cap.state,
                        format_capture_bytes(cap.bytes)
                    ))),
                    Err(err) => {
                        ui.set_rail_toast(SharedString::from(format!("Stop capture failed: {err}")))
                    }
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_delete_capture_action(move |vm_id, cap_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                match st.bridge.delete_capture(vm_id.as_str(), cap_id.as_str()) {
                    Ok(()) => ui.set_rail_toast(SharedString::from("Deleted packet capture")),
                    Err(err) => ui.set_rail_toast(SharedString::from(format!(
                        "Delete capture failed: {err}"
                    ))),
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    // Activate an Action Deck search entry
    {
        let ui_weak = ui.as_weak();
        ui.on_activate_deck_entry(move |entry_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let id = entry_id.as_str();
                if let Some(preset) = id.strip_prefix("preset:") {
                    ui.invoke_launch_preset(SharedString::from(preset));
                } else if let Some(vm_id) = id.strip_prefix("vm:") {
                    ui.invoke_activate_space(SharedString::from(vm_id));
                    ui.invoke_toggle_action_deck();
                } else if id == "app:reclaim" {
                    ui.invoke_reclaim_ram_clicked();
                    ui.invoke_toggle_action_deck();
                } else {
                    ui.set_rail_toast(SharedString::from(format!("Opened {id}")));
                    ui.invoke_toggle_action_deck();
                }
            }
        });
    }

    // Window focus & magnetic snap actions
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_focus_window(move |wid| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                st.compositor.focus_window(wid as u64);
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_snap_window_action(move |wid, zone_str| {
            if let Some(ui) = ui_weak.upgrade() {
                let zone = match zone_str.as_str() {
                    "left-half" => SnapZone::LeftHalf,
                    "right-half" => SnapZone::RightHalf,
                    "top-left" => SnapZone::TopLeftQuarter,
                    "bottom-right" => SnapZone::BottomRightQuarter,
                    _ => SnapZone::FullWorkspace,
                };
                let mut st = rt.borrow_mut();
                st.compositor.snap_window(wid as u64, zone, Instant::now());
                // Also step immediately so even single-frame renders reflect movement
                st.compositor
                    .tick_transitions(Instant::now() + std::time::Duration::from_millis(200));
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    // Hardware toggles
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_toggle_wifi(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                if !st.sys.wifi_available {
                    ui.set_rail_toast(SharedString::from(
                        "Wi-Fi unavailable in current kernel profile (wired/isolated host)",
                    ));
                    sync_shell_ui(&ui, &mut st);
                    return;
                }
                st.sys.wifi_enabled = !st.sys.wifi_enabled;
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_toggle_audio_mute(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                if !st.sys.audio_available {
                    ui.set_rail_toast(SharedString::from(
                        "Local audio unavailable in current kernel profile",
                    ));
                    sync_shell_ui(&ui, &mut st);
                    return;
                }
                st.sys.audio_muted = !st.sys.audio_muted;
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_cycle_display_scale(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                st.sys.display_scale_pct = match st.sys.display_scale_pct {
                    100 => 125,
                    125 => 150,
                    _ => 100,
                };
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        ui.on_toggle_offline_shield(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                st.sys.global_offline_shield = !st.sys.global_offline_shield;
                ui.set_rail_toast(SharedString::from(if st.sys.global_offline_shield {
                    "Offline Shield enabled — wireless radios paused"
                } else {
                    "Offline Shield disabled"
                }));
                sync_shell_ui(&ui, &mut st);
            }
        });
    }
}

fn build_bridge(cli: &ShellCli) -> Box<dyn ShellVmmBridge> {
    if cli.mock {
        Box::new(MockShellVmmBridge::with_sample_spaces())
    } else {
        Box::new(UnixVmmBridge::new(&cli.vmm_socket))
    }
}

fn should_use_kms(cli: &ShellCli) -> Result<bool, String> {
    #[cfg(all(feature = "kms-direct", not(feature = "winit")))]
    {
        let _ = cli.kms;
        Ok(true)
    }
    #[cfg(all(feature = "kms-direct", feature = "winit"))]
    {
        Ok(cli.kms
            || (std::env::var_os("WAYLAND_DISPLAY").is_none()
                && std::env::var_os("DISPLAY").is_none()))
    }
    #[cfg(not(feature = "kms-direct"))]
    {
        if cli.kms {
            return Err(
                "--kms requested, but desktop-shell was built without --features kms".to_string(),
            );
        }
        Ok(false)
    }
}

#[cfg(feature = "kms-direct")]
fn run_kms_shell(
    cli: ShellCli,
    theme_outcome: &desktop_shell::theme::ThemeLoadOutcome,
) -> Result<(), Box<dyn std::error::Error>> {
    let kms_session = desktop_shell::kms::KmsSession::open_and_install_platform()
        .map_err(|e| format!("KMS backend init failed: {e}"))?;
    let (out_w, out_h) = kms_session.size();

    let (display, mut compositor) = HyperShellState::new(out_w as i32, out_h as i32)?;
    let listener = HyperShellState::bind_socket(cli.wayland_socket.as_deref()).ok();

    let mut bridge = build_bridge(&cli);
    if cli.mock {
        if let Ok(vms) = bridge.list_vms() {
            seed_mock_workspace_windows(&mut compositor, &vms);
        }
    }

    let ui = HyperShellWindow::new()?;
    apply_theme_to_ui(&ui, &theme_outcome.theme);
    let rt = Rc::new(RefCell::new(ShellRuntime {
        compositor,
        bridge,
        mock_mode: cli.mock,
        vms: Vec::new(),
        snapshots: Vec::new(),
        captures: Vec::new(),
        vmm_connected: true,
        vmm_error: None,
        sys: SystemStatus::read(cli.mock, None),
    }));
    wire_shell(&ui, Rc::clone(&rt));
    if let Some(ref w) = theme_outcome.warning {
        ui.set_rail_toast(SharedString::from(w.as_str()));
    }

    let display_rc = Rc::new(RefCell::new(display));

    let on_super_key = {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                st.compositor.handle_super_key();
                sync_shell_ui(&ui, &mut st);
            }
        }
    };

    let on_tick = {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        let disp = Rc::clone(&display_rc);
        move |now: Instant| {
            let mut st = rt.borrow_mut();
            if let Some(ref sock) = listener {
                let mut dh = disp.borrow().handle();
                accept_pending_clients(sock, &mut dh, None);
            }
            let _ = disp.borrow_mut().dispatch_clients(&mut st.compositor);
            if st.compositor.tick_transitions(now) > 0 {
                if let Some(ui) = ui_weak.upgrade() {
                    sync_shell_ui(&ui, &mut st);
                }
            }
        }
    };

    let on_telemetry = {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                if st.mock_mode {
                    st.bridge.tick_mock_captures();
                }
                sync_shell_ui(&ui, &mut st);
            }
        }
    };

    kms_session.run_event_loop(on_super_key, on_tick, on_telemetry)?;
    Ok(())
}

#[cfg(feature = "winit")]
fn run_winit_shell(
    cli: ShellCli,
    theme_outcome: &desktop_shell::theme::ThemeLoadOutcome,
) -> Result<(), Box<dyn std::error::Error>> {
    let (display, mut compositor) = HyperShellState::new(1280, 760)?;
    let listener = HyperShellState::bind_socket(cli.wayland_socket.as_deref()).ok();

    let mut bridge = build_bridge(&cli);
    if cli.mock {
        if let Ok(vms) = bridge.list_vms() {
            seed_mock_workspace_windows(&mut compositor, &vms);
        }
    }

    let ui = HyperShellWindow::new()?;
    apply_theme_to_ui(&ui, &theme_outcome.theme);
    let rt = Rc::new(RefCell::new(ShellRuntime {
        compositor,
        bridge,
        mock_mode: cli.mock,
        vms: Vec::new(),
        snapshots: Vec::new(),
        captures: Vec::new(),
        vmm_connected: true,
        vmm_error: None,
        sys: SystemStatus::read(cli.mock, None),
    }));

    wire_shell(&ui, Rc::clone(&rt));
    if let Some(ref w) = theme_outcome.warning {
        ui.set_rail_toast(SharedString::from(w.as_str()));
    }

    let anim_timer = Timer::default();
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        let display = Rc::new(RefCell::new(display));
        anim_timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
            let mut st = rt.borrow_mut();
            if let Some(ref sock) = listener {
                let mut dh = display.borrow().handle();
                accept_pending_clients(sock, &mut dh, None);
            }
            let _ = display.borrow_mut().dispatch_clients(&mut st.compositor);
            if st.compositor.tick_transitions(Instant::now()) > 0 {
                if let Some(ui) = ui_weak.upgrade() {
                    sync_shell_ui(&ui, &mut st);
                }
            }
        });
    }

    let telemetry_timer = Timer::default();
    {
        let ui_weak = ui.as_weak();
        let rt = Rc::clone(&rt);
        telemetry_timer.start(TimerMode::Repeated, Duration::from_secs(2), move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = rt.borrow_mut();
                if st.mock_mode {
                    st.bridge.tick_mock_captures();
                }
                sync_shell_ui(&ui, &mut st);
            }
        });
    }

    ui.run()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(cli) = parse_cli().map_err(|e| format!("CLI error: {e}"))? else {
        return Ok(());
    };

    let env_theme = std::env::var("HYPEROS_THEME").ok();
    let theme_path = resolve_theme_path(cli.theme.as_deref(), env_theme.as_deref());
    let theme_outcome = load_theme_or_fallback(theme_path.as_deref());
    if let Some(ref warning) = theme_outcome.warning {
        eprintln!("desktop-shell: {warning}");
    }

    if cli.headless {
        let (mut display, mut compositor) = HyperShellState::new(1280, 760)?;
        let listener = HyperShellState::bind_socket(cli.wayland_socket.as_deref()).ok();
        let mut bridge = build_bridge(&cli);
        match bridge.list_vms() {
            Ok(vms) if cli.mock => seed_mock_workspace_windows(&mut compositor, &vms),
            Ok(_) => {}
            Err(err) => eprintln!("desktop-shell: {err}"),
        }
        println!(
            "desktop-shell: running Smithay Wayland compositor headlessly (mock={}, theme={})",
            cli.mock, theme_outcome.theme.id
        );
        let mut dh = display.handle();
        if let Some(ref sock) = listener {
            let _ = accept_pending_clients(sock, &mut dh, Some(WindowOrigin::Host));
        }
        let _ = display.dispatch_clients(&mut compositor);
        return Ok(());
    }

    if should_use_kms(&cli)? {
        #[cfg(feature = "kms-direct")]
        return run_kms_shell(cli, &theme_outcome);
        #[cfg(not(feature = "kms-direct"))]
        unreachable!("should_use_kms checked kms-direct feature");
    } else {
        #[cfg(feature = "winit")]
        return run_winit_shell(cli, &theme_outcome);
        #[cfg(not(feature = "winit"))]
        unreachable!("should_use_kms requires kms when winit is disabled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::software_renderer::{
        MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel,
    };
    use slint::platform::{Platform, WindowAdapter};
    use slint::{Model, PhysicalSize};

    struct HeadlessShellPlatform {
        window: Rc<MinimalSoftwareWindow>,
    }

    impl Platform for HeadlessShellPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }

    #[test]
    fn headless_workspace_first_shell_renders_rail_action_deck_and_reclaim_ram() {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        window.set_size(PhysicalSize::new(1280, 760));
        slint::platform::set_platform(Box::new(HeadlessShellPlatform {
            window: window.clone(),
        }))
        .unwrap();

        let (_display, mut compositor) = HyperShellState::new(1280, 760).unwrap();
        let mut bridge: Box<dyn ShellVmmBridge> =
            Box::new(MockShellVmmBridge::with_sample_spaces());
        let vms = bridge.list_vms().unwrap();
        seed_mock_workspace_windows(&mut compositor, &vms);

        let ui = HyperShellWindow::new().unwrap();
        let rt = Rc::new(RefCell::new(ShellRuntime {
            compositor,
            bridge,
            mock_mode: true,
            vms,
            snapshots: Vec::new(),
            captures: Vec::new(),
            vmm_connected: true,
            vmm_error: None,
            sys: SystemStatus::read(true, None),
        }));
        wire_shell(&ui, Rc::clone(&rt));

        assert!(ui.get_vmm_connected());
        assert_eq!(ui.get_vmm_error(), "");
        assert_eq!(ui.get_space_pills().row_count(), 3);
        assert_eq!(ui.get_windows().row_count(), 3);
        assert!(!ui.get_action_deck_visible());

        let work_id = SharedString::from("10000000-0000-4000-8000-000000000001");
        let work_pill = (0..ui.get_space_pills().row_count())
            .filter_map(|i| ui.get_space_pills().row_data(i))
            .find(|p| p.vm_id == work_id)
            .unwrap();
        assert_eq!(work_pill.network_badge, "NAT · Internet Only");
        assert!(!work_pill.disposable);
        let disp_pill = (0..ui.get_space_pills().row_count())
            .filter_map(|i| ui.get_space_pills().row_data(i))
            .find(|p| p.vm_id == "10000000-0000-4000-8000-000000000002")
            .unwrap();
        assert!(disp_pill.disposable);

        // Render Workspace + Top Context Rail into software buffer
        let mut fb = vec![Rgb565Pixel(0); 1280 * 760];
        window.draw_if_needed(|r| {
            r.render(&mut fb, 1280);
        });

        // Click "Reclaim RAM" and confirm balloon deflation toast
        ui.invoke_reclaim_ram_clicked();
        assert!(ui.get_rail_toast().contains("Reclaimed "));

        // Toggle Super-key Action Deck, search "vault", check Disposable toggle, and launch Offline Vault
        ui.invoke_toggle_action_deck();
        assert!(ui.get_action_deck_visible());
        ui.invoke_deck_query_changed(SharedString::from("vault"));
        assert!(ui.get_deck_results().row_count() >= 2);
        window.draw_if_needed(|r| {
            r.render(&mut fb, 1280);
        });

        ui.set_deck_launch_disposable(true);
        ui.invoke_launch_preset(SharedString::from("offline-vault"));
        assert!(!ui.get_action_deck_visible());
        assert_eq!(ui.get_space_pills().row_count(), 4);
        assert_eq!(ui.get_windows().row_count(), 4);
        assert!(ui.get_selected_vm_disposable());

        // Switch to Work Pod (NAT) -> start/stop/delete capture
        ui.invoke_activate_space(work_id.clone());
        assert_eq!(ui.get_selected_vm_network_badge(), "NAT · Internet Only");
        ui.invoke_start_capture_action(work_id.clone());
        assert!(ui.get_selected_capture_active());
        assert!(
            (0..ui.get_space_pills().row_count())
                .filter_map(|i| ui.get_space_pills().row_data(i))
                .find(|p| p.vm_id == work_id)
                .unwrap()
                .capturing
        );
        ui.invoke_stop_capture_action(work_id);
        assert!(!ui.get_selected_capture_active());
        let cap_row = ui.get_selected_captures().row_data(0).unwrap();
        ui.invoke_delete_capture_action(cap_row.vm_id, cap_row.capture_id);
        assert_eq!(ui.get_selected_captures().row_count(), 0);

        // Switch to stopped persistent Offline Vault -> snapshot save / restore / delete
        let vault_id = SharedString::from("10000000-0000-4000-8000-000000000003");
        ui.invoke_activate_space(vault_id.clone());
        assert_eq!(ui.get_selected_snapshots().row_count(), 1);
        ui.invoke_snapshot_vm_action(vault_id.clone());
        assert_eq!(ui.get_selected_snapshots().row_count(), 2);
        let snap_row = ui.get_selected_snapshots().row_data(0).unwrap();
        assert!(snap_row.can_restore);
        ui.invoke_confirm_restore_snapshot_action(
            snap_row.vm_id.clone(),
            snap_row.snapshot_id.clone(),
        );
        ui.invoke_delete_snapshot_action(snap_row.vm_id, snap_row.snapshot_id);
        assert_eq!(ui.get_selected_snapshots().row_count(), 1);

        // Verify non-root / SO_PEERCRED / missing-daemon socket error propagates cleanly to shell banner
        struct FailingBridge;
        impl ShellVmmBridge for FailingBridge {
            fn list_vms(&mut self) -> Result<Vec<VmSummary>, String> {
                Err(desktop_shell::vmm_bridge::classify_peer_closed_error(
                    std::path::Path::new(DEFAULT_VMM_SOCKET),
                    false,
                ))
            }
            fn create_vm(
                &mut self,
                _: &str,
                _: &str,
                _: u32,
                _: u32,
                _: &str,
                _: bool,
            ) -> Result<VmSummary, String> {
                Err("disconnected".into())
            }
            fn start_vm(&mut self, _: &str) -> Result<VmSummary, String> {
                Err("disconnected".into())
            }
            fn stop_vm(&mut self, _: &str) -> Result<VmSummary, String> {
                Err("disconnected".into())
            }
            fn set_balloon_ram(&mut self, _: &str, _: u32) -> Result<VmSummary, String> {
                Err("disconnected".into())
            }
            fn snapshot_vm(&mut self, _: &str) -> Result<SnapshotSummary, String> {
                Err("disconnected".into())
            }
            fn list_snapshots(&mut self, _: &str) -> Result<Vec<SnapshotSummary>, String> {
                Err("disconnected".into())
            }
            fn restore_snapshot(&mut self, _: &str, _: &str) -> Result<VmSummary, String> {
                Err("disconnected".into())
            }
            fn delete_snapshot(&mut self, _: &str, _: &str) -> Result<(), String> {
                Err("disconnected".into())
            }
            fn start_capture(&mut self, _: &str) -> Result<CaptureSummary, String> {
                Err("disconnected".into())
            }
            fn stop_capture(&mut self, _: &str) -> Result<CaptureSummary, String> {
                Err("disconnected".into())
            }
            fn list_captures(&mut self, _: &str) -> Result<Vec<CaptureSummary>, String> {
                Err("disconnected".into())
            }
            fn delete_capture(&mut self, _: &str, _: &str) -> Result<(), String> {
                Err("disconnected".into())
            }
        }

        let (_d2, c2) = HyperShellState::new(1280, 760).unwrap();
        let err_ui = HyperShellWindow::new().unwrap();
        let err_rt = Rc::new(RefCell::new(ShellRuntime {
            compositor: c2,
            bridge: Box::new(FailingBridge),
            mock_mode: false,
            vms: Vec::new(),
            snapshots: Vec::new(),
            captures: Vec::new(),
            vmm_connected: true,
            vmm_error: None,
            sys: SystemStatus::read(true, None),
        }));
        wire_shell(&err_ui, err_rt);
        assert!(!err_ui.get_vmm_connected());
        assert!(err_ui.get_vmm_error().contains("SO_PEERCRED"));
        assert!(err_ui.get_vmm_error().contains("hyperos"));

        // Verify C4 data-only JSON themes (Obsidian + Daylight) & origin color isolation
        let obsidian =
            desktop_shell::theme::parse_theme_str(desktop_shell::theme::BUILTIN_OBSIDIAN_JSON)
                .unwrap();
        let daylight =
            desktop_shell::theme::parse_theme_str(desktop_shell::theme::BUILTIN_DAYLIGHT_JSON)
                .unwrap();

        apply_theme_to_ui(&ui, &obsidian);
        assert_eq!(ui.global::<Theme>().get_id(), "obsidian");
        assert_eq!(ui.global::<Theme>().get_layout_preset(), "compact");
        assert_eq!(ui.global::<Theme>().get_density_level(), 0);
        let origin_accent_before = ui.get_windows().row_data(0).unwrap().accent;
        window.draw_if_needed(|r| {
            r.render(&mut fb, 1280);
        });

        apply_theme_to_ui(&ui, &daylight);
        assert_eq!(ui.global::<Theme>().get_id(), "daylight");
        assert_eq!(ui.global::<Theme>().get_layout_preset(), "comfortable");
        assert_eq!(ui.global::<Theme>().get_density_level(), 1);
        assert_eq!(
            ui.global::<Theme>().get_window_bg(),
            Color::from_rgb_u8(236, 238, 243)
        );
        assert_eq!(ui.global::<Theme>().get_ui_font_family(), "Inter");
        assert!((ui.global::<Theme>().get_font_base() - 13.0).abs() < 0.01);
        assert!((ui.global::<Theme>().get_radius_md() - 12.0).abs() < 0.01);
        assert!((ui.global::<Theme>().get_space_md() - 14.0).abs() < 0.01);
        assert_eq!(
            ui.get_windows().row_data(0).unwrap().accent,
            origin_accent_before
        );
        window.draw_if_needed(|r| {
            r.render(&mut fb, 1280);
        });
    }
}
