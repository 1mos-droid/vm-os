//! HyperOS VM Control GUI binary (`vm-control-ui`).
//!
//! Connects to `/run/hyperos/vmm.sock` (or an in-memory mock backend with `--mock`)
//! using the exact JSON-RPC 2.0 contract defined in `ARCHITECTURE.md`.

use slint::{ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use vm_control_ui::client::{UnixSocketClient, VmmBackend};
use vm_control_ui::metrics::read_host_metrics;
use vm_control_ui::mock::MockVmmBackend;
use vm_control_ui::rpc::{
    is_valid_vm_name, CreateVmParams, NetworkMode, Vm, VmState, BASE_IMAGES_DIR,
    DEFAULT_SOCKET_PATH,
};
use vm_control_ui::templates::{discover_templates, GuestTemplate};

slint::include_modules!();

/// CLI configuration parsed from process arguments.
#[derive(Debug, Clone)]
struct CliConfig {
    mock: bool,
    socket_path: PathBuf,
    base_images_dir: PathBuf,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            mock: false,
            socket_path: PathBuf::from(DEFAULT_SOCKET_PATH),
            base_images_dir: PathBuf::from(BASE_IMAGES_DIR),
        }
    }
}

/// Client-side VM policy metadata (not transmitted over v1 `create_vm` RPC
/// because `ARCHITECTURE.md` forbids unknown keys on RPC payloads).
#[derive(Debug, Clone, Default)]
struct VmClientPolicy {
    ephemeral: bool,
    desired_min_ram_mib: u32,
}

struct AppState {
    backend: Box<dyn VmmBackend>,
    mock_mode: bool,
    templates: Vec<GuestTemplate>,
    policies: HashMap<String, VmClientPolicy>,
    cached_vms: Vec<Vm>,
}

fn parse_args() -> Result<Option<CliConfig>, String> {
    let mut cfg = CliConfig::default();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mock" => cfg.mock = true,
            "--socket" => {
                let path = args
                    .next()
                    .ok_or_else(|| "--socket requires a Unix socket path argument".to_string())?;
                cfg.socket_path = PathBuf::from(path);
            }
            "--base-images" => {
                let path = args
                    .next()
                    .ok_or_else(|| "--base-images requires a directory path argument".to_string())?;
                cfg.base_images_dir = PathBuf::from(path);
            }
            "-h" | "--help" => {
                println!(
                    "HyperOS VM Control UI\n\
                     \n\
                     USAGE:\n\
                       vm-control-ui [OPTIONS]\n\
                     \n\
                     OPTIONS:\n\
                       --mock                 Run with an in-memory mock VMM backend\n\
                       --socket <PATH>        Unix domain socket path (default: {DEFAULT_SOCKET_PATH})\n\
                       --base-images <DIR>    Base images directory (default: {BASE_IMAGES_DIR})\n\
                       -h, --help             Print this help message"
                );
                return Ok(None);
            }
            other => {
                return Err(format!("unknown argument: {other} (try --help)"));
            }
        }
    }

    Ok(Some(cfg))
}

fn vm_to_ui(vm: &Vm, policy: Option<&VmClientPolicy>) -> VmItem {
    let short_id = vm.vm_id.chars().take(8).collect::<String>();
    VmItem {
        vm_id: SharedString::from(vm.vm_id.as_str()),
        short_id: SharedString::from(short_id),
        name: SharedString::from(vm.name.as_str()),
        base_image_id: SharedString::from(vm.base_image_id.as_str()),
        vcpu_count: vm.vcpu_count as i32,
        ram_mib: vm.ram_mib as i32,
        balloon_target_mib: vm.balloon_target_mib as i32,
        network_mode: SharedString::from(vm.network_mode.as_str()),
        state: SharedString::from(vm.state.as_str()),
        ephemeral: policy.map(|p| p.ephemeral).unwrap_or(false),
        desired_min_ram_mib: policy.map(|p| p.desired_min_ram_mib as i32).unwrap_or(0),
    }
}

fn template_to_ui(t: &GuestTemplate) -> TemplateItem {
    TemplateItem {
        image_id: SharedString::from(t.image_id.as_str()),
        label: SharedString::from(t.label.as_str()),
        description: SharedString::from(t.description.as_str()),
        recommended_min_ram_mib: t.recommended_min_ram_mib as i32,
        recommended_max_ram_mib: t.recommended_max_ram_mib as i32,
        recommended_vcpus: t.recommended_vcpus as i32,
    }
}

fn set_status(ui: &MainWindow, message: impl Into<String>, is_error: bool) {
    ui.set_status_message(SharedString::from(message.into()));
    ui.set_status_is_error(is_error);
}

fn refresh_metrics_ui(ui: &MainWindow, state: &AppState) {
    let active_ram_mib: u64 = state
        .cached_vms
        .iter()
        .filter(|vm| matches!(vm.state, VmState::Running | VmState::Paused))
        .map(|vm| u64::from(vm.balloon_target_mib))
        .sum();

    let metrics = read_host_metrics(state.mock_mode, active_ram_mib);
    let ram_sim = if metrics.ram.simulated {
        " (simulated)"
    } else {
        ""
    };
    ui.set_ram_usage_ratio(metrics.ram.usage_ratio());
    ui.set_ram_summary(SharedString::from(format!(
        "{} / {} MiB{}",
        metrics.ram.used_mib(),
        metrics.ram.total_mib(),
        ram_sim
    )));
    ui.set_ram_detail(SharedString::from(format!(
        "Available: {} MiB  •  VM Admission Budget (MemTotal − 768 MiB): {} MiB  •  {} Host CPUs",
        metrics.ram.available_mib(),
        metrics.ram.vm_admission_budget_mib(),
        metrics.online_cpus
    )));
    ui.set_max_host_cpus(metrics.online_cpus as i32);

    if metrics.zram.available {
        let zram_sim = if metrics.zram.simulated {
            " (mock fallback)"
        } else {
            ""
        };
        ui.set_zram_usage_ratio(metrics.zram.usage_ratio());
        ui.set_zram_summary(SharedString::from(format!(
            "{:.1} / {} MiB{}",
            metrics.zram.orig_data_mib(),
            metrics.zram.disksize_mib(),
            zram_sim
        )));
        ui.set_zram_detail(SharedString::from(format!(
            "Compressed: {:.1} MiB (mem_used: {:.1} MiB)  •  Ratio: {:.2}x  •  Algorithm: {}",
            metrics.zram.compr_data_mib(),
            metrics.zram.mem_used_total_mib(),
            metrics.zram.compression_ratio(),
            metrics.zram.comp_algorithm
        )));
    } else {
        ui.set_zram_usage_ratio(0.0);
        ui.set_zram_summary(SharedString::from("Not active"));
        ui.set_zram_detail(SharedString::from(
            "/sys/block/zram0/mm_stat not present on this host",
        ));
    }
}

fn sync_vms_ui(ui: &MainWindow, state: &mut AppState) {
    match state.backend.list_vms() {
        Ok(vms) => {
            state.cached_vms = vms;
            let items: Vec<VmItem> = state
                .cached_vms
                .iter()
                .map(|vm| vm_to_ui(vm, state.policies.get(&vm.vm_id)))
                .collect();
            ui.set_vms(ModelRc::new(VecModel::from(items)));
            ui.set_backend_connected(state.backend.is_connected());
        }
        Err(err) => {
            ui.set_backend_connected(false);
            set_status(ui, format!("Failed to query list_vms: {err}"), true);
        }
    }
    refresh_metrics_ui(ui, state);
}

fn wire_ui(
    ui: &MainWindow,
    mock_mode: bool,
    backend: Box<dyn VmmBackend>,
    templates: Vec<GuestTemplate>,
) -> Rc<RefCell<AppState>> {
    ui.set_mock_mode(mock_mode);
    ui.set_backend_label(SharedString::from(backend.backend_label()));
    ui.set_backend_connected(backend.is_connected());

    let ui_templates: Vec<TemplateItem> = templates.iter().map(template_to_ui).collect();
    ui.set_templates(ModelRc::new(VecModel::from(ui_templates)));
    if let Some(first) = templates.first() {
        ui.set_wizard_template_index(0);
        ui.set_wizard_min_ram_mib(first.recommended_min_ram_mib as i32);
        ui.set_wizard_max_ram_mib(first.recommended_max_ram_mib as i32);
        ui.set_wizard_vcpu_count(first.recommended_vcpus as i32);
    }

    let state = Rc::new(RefCell::new(AppState {
        backend,
        mock_mode,
        templates,
        policies: HashMap::new(),
        cached_vms: Vec::new(),
    }));

    // Initial load
    sync_vms_ui(ui, &mut state.borrow_mut());
    if mock_mode {
        set_status(
            ui,
            "Running in --mock mode with in-memory VMM backend and sample VMs.",
            false,
        );
    }

    // Refresh button
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_refresh_requested(move || {
            if let Some(ui) = ui_weak.upgrade() {
                sync_vms_ui(&ui, &mut state.borrow_mut());
                if !ui.get_status_is_error() {
                    set_status(&ui, "Refreshed VM list and host telemetry.", false);
                }
            }
        });
    }

    // start_vm
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_start_vm(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state.borrow_mut();
                match st.backend.start_vm(vm_id.as_str()) {
                    Ok(vm) => {
                        set_status(
                            &ui,
                            format!(
                                "VM '{}' is now {} (balloon target: {} MiB).",
                                vm.name, vm.state, vm.balloon_target_mib
                            ),
                            false,
                        );
                    }
                    Err(err) => {
                        set_status(&ui, format!("start_vm failed: {err}"), true);
                    }
                }
                sync_vms_ui(&ui, &mut st);
            }
        });
    }

    // pause_vm
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_pause_vm(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state.borrow_mut();
                match st.backend.pause_vm(vm_id.as_str()) {
                    Ok(vm) => {
                        set_status(&ui, format!("Paused VM '{}'.", vm.name), false);
                    }
                    Err(err) => {
                        set_status(&ui, format!("pause_vm failed: {err}"), true);
                    }
                }
                sync_vms_ui(&ui, &mut st);
            }
        });
    }

    // snapshot_vm
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_snapshot_vm(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state.borrow_mut();
                match st.backend.snapshot_vm(vm_id.as_str()) {
                    Ok(snap) => {
                        set_status(
                            &ui,
                            format!(
                                "Cold Btrfs snapshot created: {} (at {}).",
                                snap.snapshot_id, snap.created_at
                            ),
                            false,
                        );
                    }
                    Err(err) => {
                        set_status(&ui, format!("snapshot_vm failed: {err}"), true);
                    }
                }
                sync_vms_ui(&ui, &mut st);
            }
        });
    }

    // stop_vm (called only after hard power-off modal confirmation)
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_confirm_stop_vm(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state.borrow_mut();
                let is_ephemeral = st
                    .policies
                    .get(vm_id.as_str())
                    .map(|p| p.ephemeral)
                    .unwrap_or(false);
                match st.backend.stop_vm(vm_id.as_str()) {
                    Ok(vm) => {
                        let extra = if is_ephemeral {
                            " Ephemeral CoW overlay flagged for post-shutdown disposal."
                        } else {
                            ""
                        };
                        set_status(&ui, format!("Stopped VM '{}'.{extra}", vm.name), false);
                    }
                    Err(err) => {
                        set_status(&ui, format!("stop_vm failed: {err}"), true);
                    }
                }
                sync_vms_ui(&ui, &mut st);
            }
        });
    }

    // set_balloon_ram
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_adjust_balloon_ram(move |vm_id, target_mib| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state.borrow_mut();
                let target = target_mib.max(128) as u32;
                match st.backend.set_balloon_ram(vm_id.as_str(), target) {
                    Ok(vm) => {
                        set_status(
                            &ui,
                            format!(
                                "Updated balloon target for '{}' to {} / {} MiB.",
                                vm.name, vm.balloon_target_mib, vm.ram_mib
                            ),
                            false,
                        );
                    }
                    Err(err) => {
                        set_status(&ui, format!("set_balloon_ram failed: {err}"), true);
                    }
                }
                sync_vms_ui(&ui, &mut st);
            }
        });
    }

    // set_network_mode
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_change_network_mode(move |vm_id, mode_str| {
            if let Some(ui) = ui_weak.upgrade() {
                let mode = match mode_str.as_str() {
                    "nat" => NetworkMode::Nat,
                    "host-only" => NetworkMode::HostOnly,
                    _ => NetworkMode::Isolated,
                };
                let mut st = state.borrow_mut();
                match st.backend.set_network_mode(vm_id.as_str(), mode) {
                    Ok(vm) => {
                        set_status(
                            &ui,
                            format!(
                                "Changed '{}' network_mode to {} (applies on next start).",
                                vm.name, vm.network_mode
                            ),
                            false,
                        );
                    }
                    Err(err) => {
                        set_status(&ui, format!("set_network_mode failed: {err}"), true);
                    }
                }
                sync_vms_ui(&ui, &mut st);
            }
        });
    }

    // Template picker in Create Controlled VM Wizard
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_select_template(move |idx| {
            if let Some(ui) = ui_weak.upgrade() {
                let st = state.borrow();
                if let Some(tpl) = st.templates.get(idx as usize) {
                    ui.set_wizard_template_index(idx);
                    let max_cpus = ui.get_max_host_cpus().max(1);
                    ui.set_wizard_max_ram_mib(tpl.recommended_max_ram_mib as i32);
                    ui.set_wizard_min_ram_mib(tpl.recommended_min_ram_mib as i32);
                    ui.set_wizard_vcpu_count((tpl.recommended_vcpus as i32).min(max_cpus));
                }
            }
        });
    }

    // create_vm submission
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_create_vm_submitted(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let name = ui.get_wizard_name().trim().to_string();
                if !is_valid_vm_name(&name) {
                    set_status(
                        &ui,
                        "VM name must be 1–64 Unicode characters without control characters.",
                        true,
                    );
                    return;
                }

                let mut st = state.borrow_mut();
                let tpl_idx = ui.get_wizard_template_index().max(0) as usize;
                let Some(base_image_id) = st.templates.get(tpl_idx).map(|t| t.image_id.clone())
                else {
                    set_status(&ui, "Select a guest template before creating a VM.", true);
                    return;
                };

                let max_ram_mib = ui.get_wizard_max_ram_mib().clamp(128, 3072) as u32;
                let min_ram_mib = ui.get_wizard_min_ram_mib().clamp(128, max_ram_mib as i32) as u32;
                let vcpu_count = ui.get_wizard_vcpu_count().clamp(1, 64) as u32;
                let network_mode = NetworkMode::from_index(ui.get_wizard_network_mode());
                let ephemeral = ui.get_wizard_ephemeral();

                let params = CreateVmParams {
                    name,
                    base_image_id,
                    vcpu_count,
                    ram_mib: max_ram_mib,
                    network_mode,
                };

                match st.backend.create_vm(params) {
                    Ok(vm) => {
                        st.policies.insert(
                            vm.vm_id.clone(),
                            VmClientPolicy {
                                ephemeral,
                                desired_min_ram_mib: min_ram_mib,
                            },
                        );
                        ui.set_wizard_name(SharedString::from(""));
                        ui.set_wizard_ephemeral(false);
                        ui.set_active_tab(0);
                        set_status(
                            &ui,
                            format!(
                                "Created VM '{}' ({}) in stopped state (Max {} MiB, Min target {} MiB, {}).",
                                vm.name, vm.vm_id, vm.ram_mib, min_ram_mib, vm.network_mode
                            ),
                            false,
                        );
                        sync_vms_ui(&ui, &mut st);
                    }
                    Err(err) => {
                        set_status(&ui, format!("create_vm failed: {err}"), true);
                    }
                }
            }
        });
    }

    state
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(cfg) = parse_args().map_err(|e| format!("CLI error: {e}"))? else {
        return Ok(());
    };

    let backend: Box<dyn VmmBackend> = if cfg.mock {
        Box::new(MockVmmBackend::with_sample_vms())
    } else {
        Box::new(UnixSocketClient::new(&cfg.socket_path))
    };

    let templates = discover_templates(&cfg.base_images_dir, cfg.mock);
    let ui = MainWindow::new()?;
    let state = wire_ui(&ui, cfg.mock, backend, templates);

    // Periodic host RAM / ZRAM telemetry refresh every 2 seconds
    let metrics_timer = Timer::default();
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        metrics_timer.start(TimerMode::Repeated, Duration::from_secs(2), move || {
            if let Some(ui) = ui_weak.upgrade() {
                refresh_metrics_ui(&ui, &state.borrow());
            }
        });
    }

    ui.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
    use slint::platform::{Platform, WindowAdapter};
    use slint::{Model, PhysicalSize};
    use vm_control_ui::templates::default_mock_templates;

    struct HeadlessPlatform {
        window: Rc<MinimalSoftwareWindow>,
    }

    impl Platform for HeadlessPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }

    #[test]
    fn headless_mock_ui_renders_and_exercises_wizard_and_lifecycle() {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        window.set_size(PhysicalSize::new(1024, 768));
        slint::platform::set_platform(Box::new(HeadlessPlatform {
            window: window.clone(),
        }))
        .unwrap();

        let ui = MainWindow::new().unwrap();
        let backend = Box::new(MockVmmBackend::with_sample_vms());
        let _state = wire_ui(&ui, true, backend, default_mock_templates());

        assert!(ui.get_mock_mode());
        assert!(ui.get_backend_connected());
        assert_eq!(ui.get_vms().row_count(), 2);

        // Render Dashboard into software RGB565 buffer
        let mut frame_buf = vec![Rgb565Pixel(0); 1024 * 768];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut frame_buf, 1024);
        });

        // Switch to Create Controlled VM Wizard, render, and submit a new ephemeral VM
        ui.set_active_tab(1);
        ui.set_wizard_name(SharedString::from("wizard-vm"));
        ui.set_wizard_max_ram_mib(768);
        ui.set_wizard_min_ram_mib(256);
        ui.set_wizard_vcpu_count(1);
        ui.set_wizard_network_mode(2); // host-only
        ui.set_wizard_ephemeral(true);
        window.draw_if_needed(|renderer| {
            renderer.render(&mut frame_buf, 1024);
        });

        ui.invoke_create_vm_submitted();
        assert!(!ui.get_status_is_error(), "{}", ui.get_status_message());
        assert_eq!(ui.get_active_tab(), 0);
        assert_eq!(ui.get_vms().row_count(), 3);

        // Find newly created VM and exercise start -> balloon -> pause -> stop -> snapshot
        let created = (0..ui.get_vms().row_count())
            .filter_map(|i| ui.get_vms().row_data(i))
            .find(|v| v.name == "wizard-vm")
            .expect("created VM must appear in Dashboard list");
        assert_eq!(created.state, "stopped");
        assert_eq!(created.ram_mib, 768);
        assert_eq!(created.network_mode, "host-only");
        assert!(created.ephemeral);
        assert_eq!(created.desired_min_ram_mib, 256);

        let vm_id = created.vm_id.clone();
        ui.invoke_start_vm(vm_id.clone());
        assert!(!ui.get_status_is_error());
        ui.invoke_adjust_balloon_ram(vm_id.clone(), 256);
        assert!(!ui.get_status_is_error());
        ui.invoke_pause_vm(vm_id.clone());
        assert!(!ui.get_status_is_error());
        ui.invoke_confirm_stop_vm(vm_id.clone());
        assert!(!ui.get_status_is_error());
        ui.invoke_snapshot_vm(vm_id);
        assert!(!ui.get_status_is_error());
    }
}
