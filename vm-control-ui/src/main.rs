//! HyperOS Zero-Script VM Studio (`vm-control-ui`).
//!
//! Connects to `/run/hyperos/vmm.sock` (or an in-memory mock backend with `--mock`)
//! using the exact JSON-RPC 2.0 contract defined in `ARCHITECTURE.md`.

use slint::{Color, ModelRc, SharedString, Timer, TimerMode, VecModel};
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
use vm_control_ui::studio::{
    advance_store_downloads, compute_cow_storage_savings, default_store_catalog, CheckpointCard,
    DisplayMode, GuidedSettings, MemoryProfile, NetworkPreset, StoreTemplate, TemplateStatus,
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

/// Client-side VM studio preferences (never injected into v1 `create_vm` RPC
/// because `ARCHITECTURE.md` strictly rejects unknown keys).
#[derive(Debug, Clone)]
struct VmClientPolicy {
    ephemeral: bool,
    desired_min_ram_mib: u32,
    display_mode: DisplayMode,
}

impl Default for VmClientPolicy {
    fn default() -> Self {
        Self {
            ephemeral: false,
            desired_min_ram_mib: 256,
            display_mode: DisplayMode::SeamlessAppWindow,
        }
    }
}

struct AppState {
    backend: Box<dyn VmmBackend>,
    mock_mode: bool,
    templates: Vec<GuestTemplate>,
    store_catalog: Vec<StoreTemplate>,
    policies: HashMap<String, VmClientPolicy>,
    checkpoints: Vec<CheckpointCard>,
    cached_vms: Vec<Vm>,
    settings: GuidedSettings,
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
                let path = args.next().ok_or_else(|| {
                    "--base-images requires a directory path argument".to_string()
                })?;
                cfg.base_images_dir = PathBuf::from(path);
            }
            "-h" | "--help" => {
                println!(
                    "HyperOS Zero-Script VM Studio (vm-control-ui)\n\
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

fn vm_accent_color(vm: &Vm, ephemeral: bool) -> Color {
    match vm.network_mode {
        NetworkMode::Isolated => Color::from_rgb_u8(245, 158, 11), // Amber Shield
        NetworkMode::Nat if ephemeral => Color::from_rgb_u8(6, 182, 212), // Cyan Pulse
        NetworkMode::Nat => Color::from_rgb_u8(16, 185, 129),      // Emerald Mint
        NetworkMode::HostOnly => Color::from_rgb_u8(139, 92, 246), // Violet Spark
    }
}

fn vm_to_ui(vm: &Vm, policy: Option<&VmClientPolicy>, checkpoint_count: usize) -> VmItem {
    let short_id = vm.vm_id.chars().take(8).collect::<String>();
    let ephemeral = policy.map(|p| p.ephemeral).unwrap_or(false);
    let display_mode = policy
        .map(|p| p.display_mode)
        .unwrap_or(DisplayMode::SeamlessAppWindow);
    let net_preset = NetworkPreset::from_network_mode(vm.network_mode);

    VmItem {
        vm_id: SharedString::from(vm.vm_id.as_str()),
        short_id: SharedString::from(short_id),
        name: SharedString::from(vm.name.as_str()),
        base_image_id: SharedString::from(vm.base_image_id.as_str()),
        vcpu_count: vm.vcpu_count as i32,
        ram_mib: vm.ram_mib as i32,
        balloon_target_mib: vm.balloon_target_mib as i32,
        network_mode: SharedString::from(vm.network_mode.as_str()),
        network_plain_label: SharedString::from(net_preset.plain_label()),
        display_mode_label: SharedString::from(display_mode.label()),
        state: SharedString::from(vm.state.as_str()),
        ephemeral,
        desired_min_ram_mib: policy.map(|p| p.desired_min_ram_mib as i32).unwrap_or(0),
        accent: vm_accent_color(vm, ephemeral),
        checkpoint_count: checkpoint_count as i32,
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

fn store_card_to_ui(s: &StoreTemplate) -> StoreCardItem {
    StoreCardItem {
        image_id: SharedString::from(s.image_id.as_str()),
        title: SharedString::from(s.title.as_str()),
        summary: SharedString::from(s.summary.as_str()),
        download_label: SharedString::from(format!(
            "{} MB · {} GiB disk",
            s.download_mb, s.logical_disk_gib
        )),
        status_label: SharedString::from(s.status.label()),
        progress_ratio: s.progress_pct as f32 / 100.0,
        progress_pct: s.progress_pct as i32,
        installed: s.status == TemplateStatus::Installed,
        busy: matches!(
            s.status,
            TemplateStatus::Downloading | TemplateStatus::Verifying
        ),
    }
}

fn set_status(ui: &MainWindow, message: impl Into<String>, is_error: bool) {
    ui.set_status_message(SharedString::from(message.into()));
    ui.set_status_is_error(is_error);
}

fn sync_store_and_templates_ui(ui: &MainWindow, state: &AppState) {
    let cards: Vec<StoreCardItem> = state.store_catalog.iter().map(store_card_to_ui).collect();
    ui.set_store_cards(ModelRc::new(VecModel::from(cards)));

    let ui_templates: Vec<TemplateItem> = state.templates.iter().map(template_to_ui).collect();
    ui.set_templates(ModelRc::new(VecModel::from(ui_templates)));
}

fn sync_checkpoints_ui(ui: &MainWindow, state: &AppState) {
    let items: Vec<CheckpointItem> = state
        .checkpoints
        .iter()
        .map(|cp| CheckpointItem {
            snapshot_id: SharedString::from(cp.snapshot_id.as_str()),
            short_snap_id: SharedString::from(cp.snapshot_id.chars().take(8).collect::<String>()),
            vm_id: SharedString::from(cp.vm_id.as_str()),
            vm_name: SharedString::from(cp.vm_name.as_str()),
            label: SharedString::from(cp.label.as_str()),
            created_at: SharedString::from(cp.created_at.as_str()),
        })
        .collect();
    ui.set_checkpoints(ModelRc::new(VecModel::from(items)));
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
        "Available: {} MiB  •  Safe Space Budget: {} MiB  •  {} CPUs",
        metrics.ram.available_mib(),
        metrics.ram.vm_admission_budget_mib(),
        metrics.online_cpus
    )));
    ui.set_max_host_cpus(metrics.online_cpus as i32);

    if metrics.zram.available {
        let zram_sim = if metrics.zram.simulated {
            " (mock)"
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
            "Compressed: {:.1} MiB  •  Ratio: {:.2}x  •  {}",
            metrics.zram.compr_data_mib(),
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

    // Update Guided Settings Btrfs CoW storage savings readout
    let cow = compute_cow_storage_savings(&state.cached_vms, &state.store_catalog);
    ui.set_cow_headline(SharedString::from(format!(
        "{:.2} GiB saved ({}%) via Btrfs reflinks",
        cow.saved_gib, cow.savings_pct
    )));
    ui.set_cow_detail(SharedString::from(format!(
        "{} Space(s) cloned from {} base image(s) — {:.2} GiB logical vs ~{:.2} GiB actual disk used on /var/lib/hyperos",
        cow.cloned_spaces, cow.unique_base_images, cow.logical_total_gib, cow.actual_used_gib
    )));
    ui.set_cow_savings_ratio(cow.savings_pct as f32 / 100.0);
}

fn sync_vms_ui(ui: &MainWindow, state: &mut AppState) {
    match state.backend.list_vms() {
        Ok(vms) => {
            state.cached_vms = vms;
            let items: Vec<VmItem> = state
                .cached_vms
                .iter()
                .map(|vm| {
                    let cp_count = state
                        .checkpoints
                        .iter()
                        .filter(|c| c.vm_id == vm.vm_id)
                        .count();
                    vm_to_ui(vm, state.policies.get(&vm.vm_id), cp_count)
                })
                .collect();
            ui.set_vms(ModelRc::new(VecModel::from(items)));
            ui.set_backend_connected(state.backend.is_connected());
        }
        Err(err) => {
            ui.set_backend_connected(false);
            set_status(ui, err.to_string(), true);
        }
    }
    sync_checkpoints_ui(ui, state);
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

    let installed_ids: Vec<String> = templates.iter().map(|t| t.image_id.clone()).collect();
    let store_catalog = default_store_catalog(&installed_ids);

    if let Some(first) = templates.first() {
        ui.set_wizard_template_index(0);
        ui.set_wizard_min_ram_mib(first.recommended_min_ram_mib as i32);
        ui.set_wizard_max_ram_mib(first.recommended_max_ram_mib as i32);
        ui.set_wizard_vcpu_count(first.recommended_vcpus as i32);
    }

    let settings = GuidedSettings::for_runtime(mock_mode);
    ui.set_settings_wifi_enabled(settings.wifi_enabled);
    ui.set_settings_wifi_name(SharedString::from(settings.wifi_network_name.as_str()));
    ui.set_settings_sound_volume(settings.sound_volume_pct as i32);
    ui.set_settings_sound_muted(settings.sound_muted);
    ui.set_settings_guest_audio(settings.guest_audio_forwarding);

    let state = Rc::new(RefCell::new(AppState {
        backend,
        mock_mode,
        templates,
        store_catalog,
        policies: HashMap::new(),
        checkpoints: Vec::new(),
        cached_vms: Vec::new(),
        settings,
    }));

    {
        let mut st = state.borrow_mut();
        sync_store_and_templates_ui(ui, &st);
        sync_vms_ui(ui, &mut st);
    }
    if mock_mode {
        set_status(
            ui,
            "Running Zero-Script VM Studio in --mock mode (no vmm-daemon required).",
            false,
        );
    }

    // Refresh
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_refresh_requested(move || {
            if let Some(ui) = ui_weak.upgrade() {
                sync_vms_ui(&ui, &mut state.borrow_mut());
                if !ui.get_status_is_error() {
                    set_status(&ui, "Refreshed Spaces and host telemetry.", false);
                }
            }
        });
    }

    // Reclaim RAM button (deflates running VM balloons via set_balloon_ram)
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_reclaim_ram_requested(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state.borrow_mut();
                let running: Vec<(String, u32, u32)> = st
                    .cached_vms
                    .iter()
                    .filter(|v| v.state == VmState::Running)
                    .map(|v| {
                        let target = st
                            .policies
                            .get(&v.vm_id)
                            .map(|p| p.desired_min_ram_mib)
                            .unwrap_or(256)
                            .clamp(128, v.ram_mib);
                        (v.vm_id.clone(), v.balloon_target_mib, target)
                    })
                    .collect();

                let mut reclaimed = 0u32;
                let mut count = 0usize;
                for (id, current, target) in running {
                    if target < current && st.backend.set_balloon_ram(&id, target).is_ok() {
                        reclaimed += current - target;
                        count += 1;
                    }
                }
                if reclaimed > 0 {
                    set_status(
                        &ui,
                        format!("Reclaimed {reclaimed} MiB across {count} running Space(s)."),
                        false,
                    );
                } else {
                    set_status(&ui, "All running Spaces are already at target RAM.", false);
                }
                sync_vms_ui(&ui, &mut st);
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
                    Ok(vm) => set_status(
                        &ui,
                        format!(
                            "Opened Space '{}' ({} MiB ceiling).",
                            vm.name, vm.balloon_target_mib
                        ),
                        false,
                    ),
                    Err(err) => set_status(&ui, format!("start_vm failed: {err}"), true),
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
                    Ok(vm) => set_status(&ui, format!("Paused Space '{}'.", vm.name), false),
                    Err(err) => set_status(&ui, format!("pause_vm failed: {err}"), true),
                }
                sync_vms_ui(&ui, &mut st);
            }
        });
    }

    // Save Checkpoint (snapshot_vm)
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_snapshot_vm(move |vm_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state.borrow_mut();
                let vm_name = st
                    .cached_vms
                    .iter()
                    .find(|v| v.vm_id == vm_id.as_str())
                    .map(|v| v.name.clone())
                    .unwrap_or_else(|| "Space".to_string());
                match st.backend.snapshot_vm(vm_id.as_str()) {
                    Ok(snap) => {
                        let idx = st
                            .checkpoints
                            .iter()
                            .filter(|c| c.vm_id == snap.vm_id)
                            .count()
                            + 1;
                        let card = CheckpointCard::from_snapshot(snap.clone(), &vm_name, idx);
                        st.checkpoints.insert(0, card);
                        set_status(
                            &ui,
                            format!(
                                "Saved Checkpoint #{idx} for '{vm_name}' ({} at {}).",
                                snap.snapshot_id, snap.created_at
                            ),
                            false,
                        );
                    }
                    Err(err) => set_status(&ui, format!("Save Checkpoint failed: {err}"), true),
                }
                sync_vms_ui(&ui, &mut st);
            }
        });
    }

    // Visual Rewind Checkpoint
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_rewind_checkpoint(move |vm_id, snap_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let st = state.borrow();
                let cp_label = st
                    .checkpoints
                    .iter()
                    .find(|c| c.snapshot_id == snap_id.as_str())
                    .map(|c| format!("{} ({})", c.vm_name, c.label))
                    .unwrap_or_else(|| vm_id.to_string());
                if st.mock_mode {
                    set_status(
                        &ui,
                        format!("Rewound {cp_label} to read-only Btrfs checkpoint {snap_id} (mock preview)."),
                        false,
                    );
                } else {
                    set_status(
                        &ui,
                        format!(
                            "Checkpoint {snap_id} is preserved read-only under /var/lib/hyperos/instances/{vm_id}/snapshots/{snap_id}/ (restore RPC planned for contract v2)."
                        ),
                        false,
                    );
                }
            }
        });
    }

    // stop_vm (confirmed via hard power-off modal)
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
                            " Temporary CoW disk layer flagged for Self-Destruct cleanup."
                        } else {
                            ""
                        };
                        set_status(&ui, format!("Closed Space '{}'.{extra}", vm.name), false);
                    }
                    Err(err) => set_status(&ui, format!("stop_vm failed: {err}"), true),
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
                    Ok(vm) => set_status(
                        &ui,
                        format!(
                            "Adjusted '{}' active memory to {} / {} MiB.",
                            vm.name, vm.balloon_target_mib, vm.ram_mib
                        ),
                        false,
                    ),
                    Err(err) => set_status(&ui, format!("set_balloon_ram failed: {err}"), true),
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
                    Ok(vm) => set_status(
                        &ui,
                        format!(
                            "Switched '{}' to {} (takes effect on next open).",
                            vm.name,
                            NetworkPreset::from_network_mode(vm.network_mode).plain_label()
                        ),
                        false,
                    ),
                    Err(err) => set_status(&ui, format!("set_network_mode failed: {err}"), true),
                }
                sync_vms_ui(&ui, &mut st);
            }
        });
    }

    // Template picker in Create Space
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

    // Plain-English Memory Profile selector (Light / Standard / Heavy -> virtio-balloon)
    {
        let ui_weak = ui.as_weak();
        ui.on_select_memory_profile(move |idx| {
            if let Some(ui) = ui_weak.upgrade() {
                let profile = MemoryProfile::from_index(idx);
                let host_cpus = ui.get_max_host_cpus().max(1) as u32;
                ui.set_wizard_memory_profile(idx);
                ui.set_wizard_min_ram_mib(profile.min_ram_mib() as i32);
                ui.set_wizard_max_ram_mib(profile.max_ram_mib() as i32);
                ui.set_wizard_vcpu_count(profile.recommended_vcpus(host_cpus) as i32);
            }
        });
    }

    // Create Space submission -> verbatim create_vm (5 ARCHITECTURE.md fields)
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_create_vm_submitted(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let name = ui.get_wizard_name().trim().to_string();
                if !is_valid_vm_name(&name) {
                    set_status(
                        &ui,
                        "Please enter a Space name (1–64 characters, no control characters).",
                        true,
                    );
                    return;
                }

                let mut st = state.borrow_mut();
                let tpl_idx = ui.get_wizard_template_index().max(0) as usize;
                let Some(base_image_id) = st.templates.get(tpl_idx).map(|t| t.image_id.clone())
                else {
                    set_status(&ui, "Select a template first.", true);
                    return;
                };

                let max_ram_mib = ui.get_wizard_max_ram_mib().clamp(128, 3072) as u32;
                let min_ram_mib = ui.get_wizard_min_ram_mib().clamp(128, max_ram_mib as i32) as u32;
                let vcpu_count = ui.get_wizard_vcpu_count().clamp(1, 64) as u32;
                let network_preset = NetworkPreset::from_index(ui.get_wizard_network_mode());
                let network_mode = network_preset.to_network_mode();
                let display_mode = DisplayMode::from_index(ui.get_wizard_display_mode());
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
                                display_mode,
                            },
                        );
                        ui.set_wizard_name(SharedString::from(""));
                        ui.set_wizard_ephemeral(false);
                        ui.set_active_tab(0);
                        set_status(
                            &ui,
                            format!(
                                "Created Space '{}' ({} · {} · {}–{} MiB).",
                                vm.name,
                                network_preset.plain_label(),
                                display_mode.label(),
                                min_ram_mib,
                                vm.ram_mib
                            ),
                            false,
                        );
                        sync_vms_ui(&ui, &mut st);
                    }
                    Err(err) => set_status(&ui, format!("create_vm failed: {err}"), true),
                }
            }
        });
    }

    // Template Store: one-click Download & Provision
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_download_store_template(move |image_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state.borrow_mut();
                if let Some(item) = st
                    .store_catalog
                    .iter_mut()
                    .find(|c| c.image_id == image_id.as_str())
                {
                    if item.status == TemplateStatus::Available {
                        item.status = TemplateStatus::Downloading;
                        item.progress_pct = 20;
                        let title = item.title.clone();
                        set_status(
                            &ui,
                            format!("Downloading & verifying '{title}' into /var/lib/hyperos/base-images/{image_id}/…"),
                            false,
                        );
                    }
                }
                sync_store_and_templates_ui(&ui, &st);
            }
        });
    }

    // Template Store: jump directly into Create Space with that template preselected
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_use_store_template(move |image_id| {
            if let Some(ui) = ui_weak.upgrade() {
                let st = state.borrow();
                if let Some((idx, _)) = st
                    .templates
                    .iter()
                    .enumerate()
                    .find(|(_, t)| t.image_id == image_id.as_str())
                {
                    ui.set_wizard_template_index(idx as i32);
                }
                ui.set_active_tab(1);
            }
        });
    }

    // Guided Settings feedback
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_settings_changed(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state.borrow_mut();
                let mut note = None;
                let wanted_wifi = ui.get_settings_wifi_enabled();
                if wanted_wifi && !st.settings.wifi_available {
                    ui.set_settings_wifi_enabled(false);
                    note = Some("Wi-Fi unavailable in current kernel profile.");
                } else {
                    st.settings.wifi_enabled = wanted_wifi;
                }
                st.settings.airplane_isolation = ui.get_settings_airplane_isolation();
                let wanted_muted = ui.get_settings_sound_muted();
                if !wanted_muted && !st.settings.audio_available {
                    ui.set_settings_sound_muted(true);
                    ui.set_settings_sound_volume(0);
                    note = Some("Local audio unavailable in current kernel profile.");
                } else {
                    st.settings.sound_volume_pct =
                        ui.get_settings_sound_volume().clamp(0, 100) as u8;
                    st.settings.sound_muted = wanted_muted;
                }
                st.settings.guest_audio_forwarding =
                    st.settings.audio_available && ui.get_settings_guest_audio();
                if !st.settings.audio_available && ui.get_settings_guest_audio() {
                    ui.set_settings_guest_audio(false);
                }
                st.settings.display_scale_pct =
                    ui.get_settings_display_scale().clamp(100, 200) as u16;
                st.settings.smooth_transitions = ui.get_settings_smooth_transitions();
                let msg = if let Some(warning) = note {
                    warning.to_string()
                } else {
                    format!(
                        "Updated settings (Scale {}%, Volume {}%).",
                        st.settings.display_scale_pct,
                        if st.settings.sound_muted {
                            0
                        } else {
                            st.settings.sound_volume_pct
                        }
                    )
                };
                set_status(&ui, msg, note.is_some());
            }
        });
    }

    state
}

fn step_store_provisioning(ui: &MainWindow, st: &mut AppState) {
    let completed = advance_store_downloads(&mut st.store_catalog, 35);
    if !completed.is_empty() {
        for img_id in &completed {
            st.backend.register_template_image(img_id);
            if !st.templates.iter().any(|t| t.image_id == *img_id) {
                if let Some(card) = st.store_catalog.iter().find(|c| c.image_id == *img_id) {
                    st.templates.push(GuestTemplate {
                        image_id: card.image_id.clone(),
                        label: card.title.clone(),
                        description: card.summary.clone(),
                        recommended_min_ram_mib: card.recommended_profile.min_ram_mib(),
                        recommended_max_ram_mib: card.recommended_profile.max_ram_mib(),
                        recommended_vcpus: card.recommended_profile.recommended_vcpus(4),
                    });
                }
            }
        }
        set_status(
            ui,
            format!(
                "Template verified (SHA-256) and ready: {}",
                completed.join(", ")
            ),
            false,
        );
    }
    sync_store_and_templates_ui(ui, st);
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

    // Template Store progress bar ticker (400 ms)
    let store_timer = Timer::default();
    {
        let ui_weak = ui.as_weak();
        let state = Rc::clone(&state);
        store_timer.start(TimerMode::Repeated, Duration::from_millis(400), move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state.borrow_mut();
                let any_busy = st.store_catalog.iter().any(|c| {
                    matches!(
                        c.status,
                        TemplateStatus::Downloading | TemplateStatus::Verifying
                    )
                });
                if any_busy {
                    step_store_provisioning(&ui, &mut st);
                }
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
    fn headless_zero_script_vm_studio_exercises_all_four_views_and_mock_lifecycle() {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        window.set_size(PhysicalSize::new(1100, 780));
        slint::platform::set_platform(Box::new(HeadlessPlatform {
            window: window.clone(),
        }))
        .unwrap();

        let ui = MainWindow::new().unwrap();
        let backend = Box::new(MockVmmBackend::with_sample_vms());
        let state = wire_ui(&ui, true, backend, default_mock_templates());

        assert!(ui.get_mock_mode());
        assert!(ui.get_backend_connected());
        assert_eq!(ui.get_vms().row_count(), 2);
        assert_eq!(ui.get_store_cards().row_count(), 4);

        // 1. Render Spaces view
        let mut frame_buf = vec![Rgb565Pixel(0); 1100 * 780];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut frame_buf, 1100);
        });

        // 2. Template Store: download & provision "browser-sandbox-virtio"
        ui.set_active_tab(2);
        ui.invoke_download_store_template(SharedString::from("browser-sandbox-virtio"));
        for _ in 0..4 {
            step_store_provisioning(&ui, &mut state.borrow_mut());
        }
        window.draw_if_needed(|renderer| {
            renderer.render(&mut frame_buf, 1100);
        });
        let provisioned = (0..ui.get_store_cards().row_count())
            .filter_map(|i| ui.get_store_cards().row_data(i))
            .find(|c| c.image_id == "browser-sandbox-virtio")
            .unwrap();
        assert!(provisioned.installed);

        // 3. Create Space: Plain-English Flow (Safe Web + Heavy + Full Environment + Self-Destruct)
        ui.invoke_use_store_template(SharedString::from("browser-sandbox-virtio"));
        assert_eq!(ui.get_active_tab(), 1);
        ui.set_wizard_name(SharedString::from("Banking Sandbox"));
        ui.set_wizard_network_mode(1); // Safe Web -> nat
        ui.invoke_select_memory_profile(0); // Light -> 256..512 MiB
        ui.set_wizard_display_mode(1); // Full Environment
        ui.set_wizard_ephemeral(true); // Self-Destruct on Close
        window.draw_if_needed(|renderer| {
            renderer.render(&mut frame_buf, 1100);
        });

        ui.invoke_create_vm_submitted();
        assert!(!ui.get_status_is_error(), "{}", ui.get_status_message());
        assert_eq!(ui.get_active_tab(), 0);
        assert_eq!(ui.get_vms().row_count(), 3);

        let created = (0..ui.get_vms().row_count())
            .filter_map(|i| ui.get_vms().row_data(i))
            .find(|v| v.name == "Banking Sandbox")
            .expect("created Space must appear in Spaces view");
        assert_eq!(created.network_mode, "nat");
        assert_eq!(created.network_plain_label, "Safe Web");
        assert_eq!(created.display_mode_label, "Full Environment");
        assert!(created.ephemeral);
        assert_eq!(created.ram_mib, 512);

        // 4. Lifecycle + Visual Checkpoint Save & Rewind
        let vm_id = created.vm_id.clone();
        ui.invoke_start_vm(vm_id.clone());
        assert!(!ui.get_status_is_error());
        ui.invoke_reclaim_ram_requested();
        ui.invoke_pause_vm(vm_id.clone());
        assert!(!ui.get_status_is_error());
        ui.invoke_confirm_stop_vm(vm_id.clone());
        assert!(!ui.get_status_is_error());
        ui.invoke_snapshot_vm(vm_id.clone());
        assert!(!ui.get_status_is_error());
        assert_eq!(ui.get_checkpoints().row_count(), 1);

        let cp = ui.get_checkpoints().row_data(0).unwrap();
        ui.invoke_rewind_checkpoint(cp.vm_id, cp.snapshot_id);
        assert!(!ui.get_status_is_error());

        // 5. Guided Settings view + Btrfs CoW storage savings readout
        ui.set_active_tab(3);
        ui.set_settings_display_scale(125);
        ui.invoke_settings_changed();
        assert!(ui.get_cow_headline().contains("GiB saved"));
        window.draw_if_needed(|renderer| {
            renderer.render(&mut frame_buf, 1100);
        });
    }
}
