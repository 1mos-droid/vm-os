//! Real Smithay 0.7 Wayland compositor core for HyperOS (`desktop-shell`).
//!
//! Manages Wayland clients (`wl_compositor`, `wl_shm`, `xdg_shell`, `wl_seat`, `wl_data_device`,
//! `wl_output`), magnetic half/quarter snapping, smooth ease-out window transitions,
//! per-window 2px colored origin borders + subtle origin badges, and `Super`-key Action Deck toggle.

use crate::action_deck::ActionDeckState;
use crate::layout::{
    detect_magnetic_snap, Rect, SnapZone, TransitionEngine, TILE_GAP_PX, TOP_RAIL_HEIGHT_PX,
};
use crate::origin::{compute_origin_chrome, OriginChromeRects, WindowOrigin};
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{Space, Window};
use smithay::input::Seat;
use smithay::input::{SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle, ListeningSocket, Resource};
use smithay::utils::{Logical, Point, Serial, Size};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, CompositorClientState, CompositorHandler, CompositorState,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Per-client Wayland state attached when a client connects to the compositor socket.
#[derive(Default)]
pub struct HyperClientData {
    pub compositor_state: CompositorClientState,
    /// Optional VM origin associated with clients bridged from a guest over virtio-vsock.
    pub origin_override: Option<WindowOrigin>,
}

impl ClientData for HyperClientData {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Metadata tracked for every window managed by the HyperOS compositor.
#[derive(Debug, Clone)]
pub struct ManagedWindowMeta {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    pub origin: WindowOrigin,
    pub rect: Rect,
    pub snap_zone: Option<SnapZone>,
    pub focused: bool,
}

impl ManagedWindowMeta {
    pub fn chrome_rects(&self) -> OriginChromeRects {
        compute_origin_chrome(
            self.rect.x,
            self.rect.y,
            self.rect.w,
            self.rect.h,
            self.origin.badge_label().len(),
        )
    }
}

/// Core Smithay compositor state for the HyperOS Workspace-First shell.
pub struct HyperShellState {
    pub display_handle: DisplayHandle,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub output_manager_state: OutputManagerState,
    pub seat: Seat<Self>,
    pub space: Space<Window>,
    pub output: Output,
    pub output_size: (i32, i32),
    pub action_deck: ActionDeckState,
    pub active_space_key: String,
    pub transitions: TransitionEngine,
    pub windows: HashMap<u64, ManagedWindowMeta>,
    surface_to_id: HashMap<u64, Window>,
    next_window_id: u64,
}

impl HyperShellState {
    /// Construct a new Smithay Wayland display and compositor state.
    pub fn new(output_w: i32, output_h: i32) -> Result<(Display<Self>, Self), String> {
        let display: Display<Self> =
            Display::new().map_err(|e| format!("failed to create Wayland display: {e}"))?;
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);

        let mut seat = seat_state.new_wl_seat(&dh, "hyperos-seat0");
        let _ = seat.add_keyboard(Default::default(), 200, 25);
        seat.add_pointer();

        let output = Output::new(
            "hyperos-out-0".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "HyperOS".into(),
                model: "Workspace-First Display".into(),
            },
        );
        let mode = Mode {
            size: (output_w, output_h).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
        output.set_preferred(mode);
        let _global = output.create_global::<Self>(&dh);

        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        let state = Self {
            display_handle: dh,
            compositor_state,
            xdg_shell_state,
            shm_state,
            seat_state,
            data_device_state,
            output_manager_state,
            seat,
            space,
            output,
            output_size: (output_w, output_h),
            action_deck: ActionDeckState::default(),
            active_space_key: "host".to_string(),
            transitions: TransitionEngine::new(),
            windows: HashMap::new(),
            surface_to_id: HashMap::new(),
            next_window_id: 1,
        };

        Ok((display, state))
    }

    /// Bind a real Wayland listening socket (`wayland-hyperos-*` or named socket).
    pub fn bind_socket(socket_name: Option<&str>) -> Result<ListeningSocket, String> {
        match socket_name {
            Some(name) => ListeningSocket::bind(name)
                .map_err(|e| format!("failed to bind Wayland socket '{name}': {e}")),
            None => ListeningSocket::bind_auto("wayland-hyperos", 0..32)
                .map_err(|e| format!("failed to bind auto Wayland socket: {e}")),
        }
    }

    /// Register a virtual/demo or newly mapped window with origin attribution.
    pub fn register_managed_window(
        &mut self,
        title: impl Into<String>,
        app_id: impl Into<String>,
        origin: WindowOrigin,
        initial_rect: Rect,
        wayland_window: Option<Window>,
    ) -> u64 {
        let id = self.next_window_id;
        self.next_window_id += 1;

        for meta in self.windows.values_mut() {
            meta.focused = false;
        }

        if let Some(win) = wayland_window {
            self.space
                .map_element(win.clone(), (initial_rect.x, initial_rect.y), true);
            self.surface_to_id.insert(id, win);
        }

        self.windows.insert(
            id,
            ManagedWindowMeta {
                id,
                title: title.into(),
                app_id: app_id.into(),
                origin,
                rect: initial_rect,
                snap_zone: None,
                focused: true,
            },
        );
        id
    }

    /// Snap a window into a magnetic half/quarter/full zone with a smooth ease-out transition.
    pub fn snap_window(&mut self, window_id: u64, zone: SnapZone, now: Instant) -> Option<Rect> {
        let target = zone.target_rect(self.output_size.0, self.output_size.1);
        let meta = self.windows.get_mut(&window_id)?;
        let from = meta.rect;
        meta.snap_zone = Some(zone);
        self.transitions
            .start_transition(window_id, from, target, now);

        if let Some(win) = self.surface_to_id.get(&window_id) {
            if let Some(toplevel) = win.toplevel() {
                toplevel.with_pending_state(|st| {
                    st.size = Some(Size::<i32, Logical>::from((target.w, target.h)));
                });
                toplevel.send_configure();
            }
        }
        Some(target)
    }

    /// Evaluate pointer release position `(px, py)` for magnetic snapping and animate into place.
    pub fn complete_pointer_drag(
        &mut self,
        window_id: u64,
        px: i32,
        py: i32,
        now: Instant,
    ) -> Option<SnapZone> {
        let zone = detect_magnetic_snap(px, py, self.output_size.0, self.output_size.1)?;
        self.snap_window(window_id, zone, now);
        Some(zone)
    }

    /// Advance smooth window transitions and sync updated positions into Smithay's `Space<Window>`.
    pub fn tick_transitions(&mut self, now: Instant) -> usize {
        let updates = self.transitions.step(now);
        let count = updates.len();
        for (id, rect) in updates {
            if let Some(meta) = self.windows.get_mut(&id) {
                meta.rect = rect;
            }
            if let Some(win) = self.surface_to_id.get(&id).cloned() {
                self.space
                    .map_element(win, Point::<i32, Logical>::from((rect.x, rect.y)), false);
            }
        }
        count
    }

    /// Toggle the `Super`-key Action Deck overlay.
    pub fn handle_super_key(&mut self) -> bool {
        self.action_deck.toggle()
    }

    /// Focus a window and switch the active Workspace-First Context Rail space to its owner.
    pub fn focus_window(&mut self, window_id: u64) -> Option<&ManagedWindowMeta> {
        if !self.windows.contains_key(&window_id) {
            return None;
        }
        let mut active_key = None;
        for (&id, meta) in &mut self.windows {
            meta.focused = id == window_id;
            if meta.focused {
                active_key = Some(meta.origin.space_key().to_string());
            }
        }
        if let Some(key) = active_key {
            self.active_space_key = key;
        }
        if let Some(win) = self.surface_to_id.get(&window_id).cloned() {
            self.space.raise_element(&win, true);
        }
        self.windows.get(&window_id)
    }

    /// Software/Pixman-compatible ARGB8888 renderer for per-window 2px colored origin borders
    /// and subtle top-left origin badges (used by both the Pixman compositor backend and tests).
    pub fn render_origin_chrome_argb8888(&self, width: usize, height: usize, fb: &mut [u32]) {
        if fb.len() < width * height {
            return;
        }
        for meta in self.windows.values() {
            let color = meta.origin.accent_color().to_argb8888(0xff);
            let badge_bg = meta.origin.accent_color().to_argb8888(0xe6);
            let chrome = meta.chrome_rects();

            // Draw exact 2px outer border (top, bottom, left, right strips).
            fill_rect_argb(
                fb,
                width,
                height,
                chrome.border_x,
                chrome.border_y,
                chrome.border_w,
                chrome.border_thickness,
                color,
            );
            fill_rect_argb(
                fb,
                width,
                height,
                chrome.border_x,
                chrome.border_y + chrome.border_h - chrome.border_thickness,
                chrome.border_w,
                chrome.border_thickness,
                color,
            );
            fill_rect_argb(
                fb,
                width,
                height,
                chrome.border_x,
                chrome.border_y,
                chrome.border_thickness,
                chrome.border_h,
                color,
            );
            fill_rect_argb(
                fb,
                width,
                height,
                chrome.border_x + chrome.border_w - chrome.border_thickness,
                chrome.border_y,
                chrome.border_thickness,
                chrome.border_h,
                color,
            );

            // Draw subtle top-left origin badge pill background.
            fill_rect_argb(
                fb,
                width,
                height,
                chrome.badge_x,
                chrome.badge_y,
                chrome.badge_w,
                chrome.badge_h,
                badge_bg,
            );
        }
    }

    fn default_spawn_rect(&self) -> Rect {
        let idx = self.windows.len() as i32;
        let offset = (idx % 6) * 28;
        let w = (self.output_size.0 * 55 / 100).clamp(480, 920);
        let h = (self.output_size.1 * 62 / 100).clamp(320, 640);
        Rect::new(
            TILE_GAP_PX + 40 + offset,
            TOP_RAIL_HEIGHT_PX + TILE_GAP_PX + 32 + offset,
            w,
            h,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_rect_argb(
    fb: &mut [u32],
    fb_w: usize,
    fb_h: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    pixel: u32,
) {
    let x0 = x.max(0) as usize;
    let y0 = y.max(0) as usize;
    let x1 = (x + w).clamp(0, fb_w as i32) as usize;
    let y1 = (y + h).clamp(0, fb_h as i32) as usize;
    for row in y0..y1 {
        let start = row * fb_w + x0;
        let end = row * fb_w + x1;
        if start < end && end <= fb.len() {
            fb[start..end].fill(pixel);
        }
    }
}

impl BufferHandler for HyperShellState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl CompositorHandler for HyperShellState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client
            .get_data::<HyperClientData>()
            .expect("client must have HyperClientData")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if let Some(window) = self
            .space
            .elements()
            .find(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == surface)
                    .unwrap_or(false)
            })
            .cloned()
        {
            window.on_commit();
            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .map(|d| d.lock().unwrap().initial_configure_sent)
                    .unwrap_or(true)
            });
            if !initial_configure_sent {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_configure();
                }
            }
        }
    }
}

impl ShmHandler for HyperShellState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for HyperShellState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&Self::KeyboardFocus>) {}
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

impl SelectionHandler for HyperShellState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for HyperShellState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for HyperShellState {}
impl ServerDndGrabHandler for HyperShellState {}

impl OutputHandler for HyperShellState {}

impl XdgShellHandler for HyperShellState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let initial_rect = self.default_spawn_rect();
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
            state.size = Some(Size::<i32, Logical>::from((initial_rect.w, initial_rect.h)));
        });
        surface.send_configure();

        let origin = surface
            .wl_surface()
            .client()
            .and_then(|c| {
                c.get_data::<HyperClientData>()
                    .and_then(|d| d.origin_override.clone())
            })
            .unwrap_or(WindowOrigin::Host);

        let window = Window::new_wayland_window(surface);
        self.register_managed_window(
            "Wayland App",
            "wayland-client",
            origin,
            initial_rect,
            Some(window),
        );
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

delegate_compositor!(HyperShellState);
delegate_xdg_shell!(HyperShellState);
delegate_shm!(HyperShellState);
delegate_seat!(HyperShellState);
delegate_data_device!(HyperShellState);
delegate_output!(HyperShellState);

/// Helper to accept any pending Wayland clients on a non-blocking `ListeningSocket`.
pub fn accept_pending_clients(
    socket: &ListeningSocket,
    dh: &mut DisplayHandle,
    origin_override: Option<WindowOrigin>,
) -> usize {
    let mut accepted = 0;
    while let Ok(Some(stream)) = socket.accept() {
        let data = Arc::new(HyperClientData {
            compositor_state: CompositorClientState::default(),
            origin_override: origin_override.clone(),
        });
        if dh.insert_client(stream, data).is_ok() {
            accepted += 1;
        }
    }
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::palette;
    use std::time::Duration;

    #[test]
    fn smithay_compositor_initializes_snaps_animates_and_draws_2px_origin_borders() {
        let (_display, mut state) = HyperShellState::new(1280, 720).unwrap();

        // Register a VM-owned window and a Host-owned window
        let vm_origin = WindowOrigin::Vm {
            vm_id: "10000000-0000-4000-8000-000000000003".into(),
            vm_name: "Offline Vault".into(),
            network_mode: "isolated".into(),
            disposable: false,
        };
        let win_id = state.register_managed_window(
            "Key Manager",
            "vault-keys",
            vm_origin,
            Rect::new(120, 120, 560, 420),
            None,
        );

        // Super key toggles Action Deck
        assert!(!state.action_deck.visible);
        assert!(state.handle_super_key());
        assert!(state.action_deck.visible);
        assert!(!state.handle_super_key());

        // Magnetic drag release at top-left corner snaps to TopLeftQuarter with smooth transition
        let t0 = Instant::now();
        let snapped = state.complete_pointer_drag(win_id, 6, TOP_RAIL_HEIGHT_PX + 10, t0);
        assert_eq!(snapped, Some(SnapZone::TopLeftQuarter));
        assert!(state.transitions.has_active());

        // Mid-transition and final transition tick
        state.tick_transitions(t0 + Duration::from_millis(90));
        assert!(state.transitions.has_active());
        state.tick_transitions(t0 + Duration::from_millis(200));
        assert!(!state.transitions.has_active());

        let expected = SnapZone::TopLeftQuarter.target_rect(1280, 720);
        assert_eq!(state.windows[&win_id].rect, expected);

        // Render 2px Amber border & badge into ARGB8888 buffer and verify exact pixels
        let mut fb = vec![0u32; 1280 * 720];
        state.render_origin_chrome_argb8888(1280, 720, &mut fb);
        let amber = palette::VAULT_AMBER.to_argb8888(0xff);
        let top_border_x = (expected.x - 2) as usize;
        let top_border_y = (expected.y - 2) as usize;
        assert_eq!(fb[top_border_y * 1280 + top_border_x], amber);
        assert_eq!(fb[(top_border_y + 1) * 1280 + top_border_x], amber);
    }

    #[test]
    fn smithay_compositor_binds_real_wayland_socket_and_accepts_client() {
        let (mut display, mut state) = HyperShellState::new(1280, 720).unwrap();
        let short = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let sock_name = format!("wl-{short}");
        let tmp_dir = std::env::temp_dir().join(format!("hwl-{short}"));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", &tmp_dir);

        let listener = HyperShellState::bind_socket(Some(&sock_name)).unwrap();
        let client_path = tmp_dir.join(&sock_name);
        assert!(client_path.exists());

        let _client = std::os::unix::net::UnixStream::connect(&client_path).unwrap();
        let mut dh = display.handle();
        let count = accept_pending_clients(&listener, &mut dh, Some(WindowOrigin::Host));
        assert_eq!(count, 1);
        let _ = display.dispatch_clients(&mut state);

        drop(listener);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
