//! HyperOS Workspace-First Wayland Desktop Shell binary (`desktop-shell`).
//!
//! Combines a Smithay 0.7 Wayland compositor with a lightweight software-rendered Slint
//! Top Context Rail and `Super`-key Action Deck targeting <100 MB total RAM usage.

use desktop_shell::compositor::{accept_pending_clients, HyperShellState};
use desktop_shell::layout::{Rect, SnapZone, TOP_RAIL_HEIGHT_PX};
use desktop_shell::origin::{Rgb, WindowOrigin};
use desktop_shell::sys_status::SystemStatus;
use desktop_shell::vmm_bridge::{
    launch_environment_preset, reclaim_idle_ram, EnvironmentPreset, MockShellVmmBridge,
    ShellVmmBridge, UnixVmmBridge, VmSummary, DEFAULT_VMM_SOCKET,
};
use slint::{Color, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

slint::include_modules!();

struct ShellCli {
    mock: bool,
    headless: bool,
    vmm_socket: PathBuf,
    wayland_socket: Option<String>,
}

fn parse_cli() -> Result<Option<ShellCli>, String> {
    let mut mock = false;
    let mut headless = false;
    let mut vmm_socket = PathBuf::from(DEFAULT_VMM_SOCKET);
    let mut wayland_socket = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mock" => mock = true,
            "--headless" => headless = true,
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
            "-h" | "--help" => {
                println!(
                    "HyperOS Workspace-First Desktop Shell (Smithay + Slint)\n\
                     \n\
                     USAGE:\n\
                       desktop-shell [OPTIONS]\n\
                     \n\
                     OPTIONS:\n\
                       --mock                    Run with in-memory mock VMs and telemetry fallback\n\
                       --headless                Run Smithay Wayland compositor without opening a nested window\n\
                       --socket <PATH>           vmm-daemon Unix socket (default: {DEFAULT_VMM_SOCKET})\n\
                       --wayland-socket <NAME>   Wayland listening socket name (default: auto wayland-hyperos-*)\n\
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
        vmm_socket,
        wayland_socket,
    }))
}

struct ShellRuntime {
    compositor: HyperShellState,
    bridge: Box<dyn ShellVmmBridge>,
    mock_mode: bool,
    vms: Vec<VmSummary>,
    sys: SystemStatus,
}

fn rgb_to_slint(c: Rgb) -> Color {
    Color::from_rgb_u8(c.r, c.g, c.b)
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
    if let Ok(vms) = rt.bridge.list_vms() {
        rt.vms = vms;
    }
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
                ephemeral: vm.name.to_ascii_lowercase().contains("disposable"),
            };
            SpacePill {
                vm_id: SharedString::from(vm.vm_id.as_str()),
                name: SharedString::from(vm.name.as_str()),
                ram_label: SharedString::from(format!("{}M", vm.balloon_target_mib)),
                accent: rgb_to_slint(origin.accent_color()),
                active: rt.compositor.active_space_key == vm.vm_id,
                running: vm.state == "running",
            }
        })
        .collect();
    ui.set_space_pills(ModelRc::new(VecModel::from(pills)));

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
    if let Some(work) = vms.first() {
        let origin = WindowOrigin::Vm {
            vm_id: work.vm_id.clone(),
            vm_name: work.name.clone(),
            network_mode: work.network_mode.clone(),
            ephemeral: false,
        };
        let rect = SnapZone::LeftHalf.target_rect(1280, 760);
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
            ephemeral: true,
        };
        let rect = SnapZone::RightHalf.target_rect(1280, 760);
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
                let mut st = rt.borrow_mut();
                match launch_environment_preset(st.bridge.as_mut(), preset) {
                    Ok(vm) => {
                        let origin = WindowOrigin::Vm {
                            vm_id: vm.vm_id.clone(),
                            vm_name: vm.name.clone(),
                            network_mode: vm.network_mode.clone(),
                            ephemeral: preset.is_ephemeral(),
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
                        ui.set_rail_toast(SharedString::from(format!(
                            "Launched {} ({} MiB)",
                            preset.title(),
                            vm.ram_mib
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
                    .tick_transitions(Instant::now() + Duration::from_millis(200));
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(cli) = parse_cli().map_err(|e| format!("CLI error: {e}"))? else {
        return Ok(());
    };

    let (mut display, mut compositor) = HyperShellState::new(1280, 760)?;
    let listener = HyperShellState::bind_socket(cli.wayland_socket.as_deref()).ok();

    let mut bridge: Box<dyn ShellVmmBridge> = if cli.mock {
        Box::new(MockShellVmmBridge::with_sample_spaces())
    } else {
        Box::new(UnixVmmBridge::new(&cli.vmm_socket))
    };

    let vms = bridge.list_vms().unwrap_or_default();
    if cli.mock {
        seed_mock_workspace_windows(&mut compositor, &vms);
    }

    if cli.headless {
        println!(
            "desktop-shell: running Smithay Wayland compositor headlessly (mock={})",
            cli.mock
        );
        let mut dh = display.handle();
        if let Some(ref sock) = listener {
            let _ = accept_pending_clients(sock, &mut dh, Some(WindowOrigin::Host));
        }
        let _ = display.dispatch_clients(&mut compositor);
        return Ok(());
    }

    let ui = HyperShellWindow::new()?;
    let rt = Rc::new(RefCell::new(ShellRuntime {
        compositor,
        bridge,
        mock_mode: cli.mock,
        vms,
        sys: SystemStatus::read(cli.mock, None),
    }));

    wire_shell(&ui, Rc::clone(&rt));

    // Low-overhead 60Hz transition + Wayland client pump when animations are active,
    // and 2s telemetry refresh.
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
                sync_shell_ui(&ui, &mut rt.borrow_mut());
            }
        });
    }

    ui.run()?;
    Ok(())
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
            sys: SystemStatus::read(true, None),
        }));
        wire_shell(&ui, Rc::clone(&rt));

        assert_eq!(ui.get_space_pills().row_count(), 3);
        assert_eq!(ui.get_windows().row_count(), 3);
        assert!(!ui.get_action_deck_visible());

        // Render Workspace + Top Context Rail into software buffer
        let mut fb = vec![Rgb565Pixel(0); 1280 * 760];
        window.draw_if_needed(|r| {
            r.render(&mut fb, 1280);
        });

        // Click "Reclaim RAM" and confirm balloon deflation toast
        ui.invoke_reclaim_ram_clicked();
        assert!(ui.get_rail_toast().contains("Reclaimed "));

        // Toggle Super-key Action Deck, search "vault", and launch Offline Vault
        ui.invoke_toggle_action_deck();
        assert!(ui.get_action_deck_visible());
        ui.invoke_deck_query_changed(SharedString::from("vault"));
        assert!(ui.get_deck_results().row_count() >= 2);
        window.draw_if_needed(|r| {
            r.render(&mut fb, 1280);
        });

        ui.invoke_launch_preset(SharedString::from("offline-vault"));
        assert!(!ui.get_action_deck_visible());
        assert_eq!(ui.get_space_pills().row_count(), 4);
        assert_eq!(ui.get_windows().row_count(), 4);
    }
}
