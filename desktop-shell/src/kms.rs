//! Bare-metal Linux DRM/KMS + udev + libinput (+ optional libseat) backend for `desktop-shell`.
//!
//! Renders Slint's CPU `SoftwareRenderer` directly into double-buffered `XRGB8888` DRM dumb
//! buffers with a software cursor overlay and drives Wayland client dispatch, compositor
//! animations, and input events through a single `calloop` event loop.

use crate::framebuffer::{
    copy_xrgb8888_to_dumb_buffer, draw_software_cursor, is_super_keycode,
    linux_keycode_to_slint_key, vt_switch_target, Xrgb8888Pixel,
};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, TargetPixel};
use slint::platform::{Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{LogicalPosition, PhysicalSize};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, InputEvent, KeyState, KeyboardKeyEvent,
    MouseButton, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::libinput::LibinputInputBackend;
use smithay::backend::udev::{all_gpus, UdevBackend};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::buffer::Buffer as _;
use smithay::reexports::drm::control::{
    connector, crtc, dumbbuffer::DumbBuffer, framebuffer, Device as ControlDevice, Mode,
    ModeTypeFlags, PageFlipFlags,
};
use smithay::reexports::input::{Libinput, LibinputInterface};
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::DeviceFd;
use std::cell::RefCell;
#[cfg(feature = "kms")]
use std::os::fd::AsFd;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

#[cfg(feature = "kms")]
use smithay::backend::session::libseat::{LibSeatSession, LibSeatSessionNotifier};
#[cfg(feature = "kms")]
use smithay::backend::session::{AsErrno, Event as SessionEvent, Session};

#[cfg(all(feature = "kms-direct", not(feature = "kms")))]
const ENOENT_ERRNO: i32 = 2;
#[cfg(feature = "kms")]
const EACCES_ERRNO: i32 = 13;
const EBUSY_ERRNO: i32 = 16;

/// Device & VT session manager (`libseat` when `kms` is enabled; direct device open for `kms-direct`).
struct SessionManager {
    seat_name: String,
    active: bool,
    #[cfg(feature = "kms")]
    libseat_session: LibSeatSession,
    #[cfg(feature = "kms")]
    libseat_notifier: Option<LibSeatSessionNotifier>,
}

impl SessionManager {
    fn open() -> Result<Self, String> {
        #[cfg(feature = "kms")]
        {
            let (session, notifier) = LibSeatSession::new().map_err(|e| {
                format!("failed to open seatd/libseat session (is seatd running?): {e}")
            })?;
            let seat_name = session.seat();
            let active = session.is_active();
            Ok(Self {
                seat_name,
                active,
                libseat_session: session,
                libseat_notifier: Some(notifier),
            })
        }
        #[cfg(all(feature = "kms-direct", not(feature = "kms")))]
        {
            let seat_name = std::env::var("XDG_SEAT").unwrap_or_else(|_| "seat0".to_string());
            Ok(Self {
                seat_name,
                active: true,
            })
        }
    }

    fn seat(&self) -> &str {
        &self.seat_name
    }

    fn open_drm_device(&mut self, path: &Path) -> Result<OwnedFd, String> {
        #[cfg(feature = "kms")]
        {
            self.libseat_session
                .open(path, OFlags::RDWR | OFlags::CLOEXEC)
                .map_err(|e| format!("libseat open {}: {e}", path.display()))
        }
        #[cfg(all(feature = "kms-direct", not(feature = "kms")))]
        {
            use std::fs::OpenOptions;
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(OFlags::CLOEXEC.bits() as i32)
                .open(path)
                .map(OwnedFd::from)
                .map_err(|e| format!("open {}: {e}", path.display()))
        }
    }

    fn change_vt(&mut self, vt: i32) {
        #[cfg(feature = "kms")]
        {
            if let Err(err) = self.libseat_session.change_vt(vt) {
                eprintln!("desktop-shell: failed to switch to VT {vt}: {err}");
            }
        }
        #[cfg(all(feature = "kms-direct", not(feature = "kms")))]
        {
            let _ = vt;
        }
    }

    fn libinput_interface(&self) -> ShellLibinputInterface {
        #[cfg(feature = "kms")]
        {
            ShellLibinputInterface {
                session: self.libseat_session.clone(),
            }
        }
        #[cfg(all(feature = "kms-direct", not(feature = "kms")))]
        {
            ShellLibinputInterface {}
        }
    }

    #[cfg(feature = "kms")]
    fn take_notifier(&mut self) -> Option<LibSeatSessionNotifier> {
        self.libseat_notifier.take()
    }
}

struct ShellLibinputInterface {
    #[cfg(feature = "kms")]
    session: LibSeatSession,
}

impl LibinputInterface for ShellLibinputInterface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        #[cfg(feature = "kms")]
        {
            let oflags = OFlags::from_bits_retain(flags as u32);
            let fd = self
                .session
                .open(path, oflags)
                .map_err(|e| e.as_errno().unwrap_or(EACCES_ERRNO))?;
            // Ensure libinput receives non-blocking flags if requested.
            let _ = smithay::reexports::rustix::fs::fcntl_setfl(fd.as_fd(), oflags);
            Ok(fd)
        }
        #[cfg(all(feature = "kms-direct", not(feature = "kms")))]
        {
            use std::fs::OpenOptions;
            use std::os::unix::fs::OpenOptionsExt;
            let read = (flags & 0o3) == 0 || (flags & 0o3) == 2;
            let write = (flags & 0o3) == 1 || (flags & 0o3) == 2;
            OpenOptions::new()
                .read(read)
                .write(write)
                .custom_flags(flags)
                .open(path)
                .map(OwnedFd::from)
                .map_err(|e| e.raw_os_error().unwrap_or(ENOENT_ERRNO))
        }
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        #[cfg(feature = "kms")]
        {
            let _ = self.session.close(fd);
        }
        #[cfg(all(feature = "kms-direct", not(feature = "kms")))]
        {
            drop(fd);
        }
    }
}

struct DumbSlot {
    fb: framebuffer::Handle,
    buf: DumbBuffer,
}

/// Double-buffered DRM/KMS dumb-buffer scanout display.
struct KmsDisplay {
    drm: DrmDeviceFd,
    connector: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    width: u32,
    height: u32,
    slots: [DumbSlot; 2],
    front_idx: usize,
    needs_modeset: bool,
    active: bool,
}

impl KmsDisplay {
    fn open(session: &mut SessionManager) -> Result<Self, String> {
        let mut candidates = all_gpus(session.seat()).unwrap_or_default();
        if candidates.is_empty() {
            for idx in 0..8 {
                let path = PathBuf::from(format!("/dev/dri/card{idx}"));
                if path.exists() {
                    candidates.push(path);
                }
            }
        }
        if candidates.is_empty() {
            return Err("no DRM display nodes found under /dev/dri/card*".to_string());
        }

        let mut errors = Vec::new();
        for path in candidates {
            match Self::try_open_card(session, &path) {
                Ok(disp) => return Ok(disp),
                Err(err) => errors.push(format!("{}: {err}", path.display())),
            }
        }
        Err(format!(
            "failed to initialize any DRM output:\n  {}",
            errors.join("\n  ")
        ))
    }

    fn try_open_card(session: &mut SessionManager, path: &Path) -> Result<Self, String> {
        let fd = session.open_drm_device(path)?;
        let drm = DrmDeviceFd::new(DeviceFd::from(fd));
        let (connector, crtc, mode) = find_connected_output(&drm)?;
        let (mode_w, mode_h) = mode.size();
        let width = u32::from(mode_w);
        let height = u32::from(mode_h);
        let slots = allocate_double_buffers(&drm, width, height)?;

        Ok(Self {
            drm,
            connector,
            crtc,
            mode,
            width,
            height,
            slots,
            front_idx: 0,
            needs_modeset: true,
            active: session.active,
        })
    }

    #[cfg(feature = "kms")]
    fn set_active(&mut self, active: bool) {
        self.active = active;
        if active {
            self.needs_modeset = true;
        }
    }

    fn present(&mut self, pixels: &[Xrgb8888Pixel]) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }
        let back_idx = 1 - self.front_idx;
        let width = self.width as usize;
        let height = self.height as usize;

        {
            let back = &mut self.slots[back_idx];
            let pitch = back.buf.pitch() as usize;
            let mut mapping = self
                .drm
                .map_dumb_buffer(&mut back.buf)
                .map_err(|e| format!("DRM map_dumb_buffer failed: {e}"))?;
            copy_xrgb8888_to_dumb_buffer(pixels, width, height, mapping.as_mut(), pitch)?;
        }

        let fb = self.slots[back_idx].fb;
        if self.needs_modeset {
            self.drm
                .set_crtc(
                    self.crtc,
                    Some(fb),
                    (0, 0),
                    &[self.connector],
                    Some(self.mode),
                )
                .map_err(|e| format!("DRM set_crtc failed: {e}"))?;
            self.needs_modeset = false;
            self.front_idx = back_idx;
            return Ok(());
        }

        match self
            .drm
            .page_flip(self.crtc, fb, PageFlipFlags::empty(), None)
        {
            Ok(()) => {
                self.front_idx = back_idx;
            }
            Err(err) if err.raw_os_error() == Some(EBUSY_ERRNO) => {
                // Previous flip still queued; next tick will present the latest framebuffer.
            }
            Err(_) => {
                // SimpleDRM or legacy drivers without async page-flip fall back to set_crtc.
                self.drm
                    .set_crtc(
                        self.crtc,
                        Some(fb),
                        (0, 0),
                        &[self.connector],
                        Some(self.mode),
                    )
                    .map_err(|e| format!("DRM set_crtc fallback failed: {e}"))?;
                self.front_idx = back_idx;
            }
        }
        Ok(())
    }
}

impl Drop for KmsDisplay {
    fn drop(&mut self) {
        // Replace slots cleanly using dummy handles is not possible without Option,
        // so destroy in place; ignore errors during teardown.
        for slot in &self.slots {
            let _ = self.drm.destroy_framebuffer(slot.fb);
            let _ = self.drm.destroy_dumb_buffer(slot.buf);
        }
    }
}

fn find_connected_output(
    drm: &DrmDeviceFd,
) -> Result<(connector::Handle, crtc::Handle, Mode), String> {
    let resources = drm
        .resource_handles()
        .map_err(|e| format!("failed to read DRM resource handles: {e}"))?;
    let wanted = std::env::var("HYPEROS_DRM_OUTPUT").ok();

    let mut chosen_conn = None;
    let mut available_names = Vec::new();
    for &handle in resources.connectors() {
        let Ok(info) = drm.get_connector(handle, false) else {
            continue;
        };
        let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
        let connected = info.state() == connector::State::Connected && !info.modes().is_empty();
        available_names.push(format!("{name} (connected={connected})"));

        if let Some(ref target) = wanted {
            if &name == target && connected {
                chosen_conn = Some(info);
                break;
            }
        } else if connected {
            chosen_conn = Some(info);
            break;
        }
    }

    let conn = chosen_conn.ok_or_else(|| {
        format!(
            "no connected DRM output matching {:?} (found: [{}])",
            wanted,
            available_names.join(", ")
        )
    })?;

    let mode = conn
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .copied()
        .or_else(|| conn.modes().first().copied())
        .ok_or_else(|| "selected DRM connector exposes no display modes".to_string())?;

    let crtc = resolve_crtc_for_connector(drm, &resources, &conn)?;
    Ok((conn.handle(), crtc, mode))
}

fn resolve_crtc_for_connector(
    drm: &DrmDeviceFd,
    resources: &smithay::reexports::drm::control::ResourceHandles,
    conn: &connector::Info,
) -> Result<crtc::Handle, String> {
    if let Some(enc_handle) = conn.current_encoder() {
        if let Ok(enc) = drm.get_encoder(enc_handle) {
            if let Some(crtc) = enc.crtc() {
                return Ok(crtc);
            }
        }
    }
    for &enc_handle in conn.encoders() {
        if let Ok(enc) = drm.get_encoder(enc_handle) {
            if let Some(crtc) = resources
                .filter_crtcs(enc.possible_crtcs())
                .into_iter()
                .next()
            {
                return Ok(crtc);
            }
        }
    }
    resources
        .crtcs()
        .first()
        .copied()
        .ok_or_else(|| "no DRM CRTC available for connector".to_string())
}

fn allocate_double_buffers(
    drm: &DrmDeviceFd,
    width: u32,
    height: u32,
) -> Result<[DumbSlot; 2], String> {
    let s0 = create_dumb_slot(drm, width, height)?;
    match create_dumb_slot(drm, width, height) {
        Ok(s1) => Ok([s0, s1]),
        Err(err) => {
            let _ = drm.destroy_framebuffer(s0.fb);
            let _ = drm.destroy_dumb_buffer(s0.buf);
            Err(err)
        }
    }
}

fn create_dumb_slot(drm: &DrmDeviceFd, width: u32, height: u32) -> Result<DumbSlot, String> {
    let buf = drm
        .create_dumb_buffer((width, height), Fourcc::Xrgb8888, 32)
        .map_err(|e| format!("DRM create_dumb_buffer ({width}x{height}) failed: {e}"))?;
    match drm.add_framebuffer(&buf, 24, 32) {
        Ok(fb) => Ok(DumbSlot { fb, buf }),
        Err(err) => {
            let _ = drm.destroy_dumb_buffer(buf);
            Err(format!("DRM add_framebuffer failed: {err}"))
        }
    }
}

/// Slint `Platform` backed by a persistent `MinimalSoftwareWindow` sized to the DRM mode.
struct KmsSlintPlatform {
    window: Rc<MinimalSoftwareWindow>,
    start: Instant,
}

impl Platform for KmsSlintPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }
}

#[derive(Default)]
struct InputModifiers {
    shift: bool,
    ctrl: bool,
    alt: bool,
}

impl InputModifiers {
    fn update(&mut self, evdev_code: u32, pressed: bool) {
        match evdev_code {
            42 | 54 => self.shift = pressed,
            29 | 97 => self.ctrl = pressed,
            56 | 100 => self.alt = pressed,
            _ => {}
        }
    }
}

/// Opened DRM/KMS session and Slint software window handle.
pub struct KmsSession {
    session: SessionManager,
    display: KmsDisplay,
    window: Rc<MinimalSoftwareWindow>,
}

impl KmsSession {
    /// Open the seat session and primary DRM connector, and install the Slint software platform.
    pub fn open_and_install_platform() -> Result<Self, String> {
        let mut session = SessionManager::open()?;
        let display = KmsDisplay::open(&mut session)?;
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        window.set_size(PhysicalSize::new(display.width, display.height));

        slint::platform::set_platform(Box::new(KmsSlintPlatform {
            window: Rc::clone(&window),
            start: Instant::now(),
        }))
        .map_err(|e| format!("failed to install Slint KMS platform: {e}"))?;

        Ok(Self {
            session,
            display,
            window,
        })
    }

    /// Physical display width and height in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.display.width, self.display.height)
    }

    /// Run the `calloop` event loop driving DRM scanout, `libinput`, seat switching,
    /// Slint animations/timers, Wayland client dispatch, and periodic telemetry refresh.
    pub fn run_event_loop(
        self,
        mut on_super_key: impl FnMut() + 'static,
        mut on_tick: impl FnMut(Instant) + 'static,
        mut on_telemetry: impl FnMut() + 'static,
    ) -> Result<(), String> {
        let width = self.display.width as usize;
        let height = self.display.height as usize;

        let mut libinput_ctx = Libinput::new_with_udev(self.session.libinput_interface());
        libinput_ctx
            .udev_assign_seat(self.session.seat())
            .map_err(|_| {
                format!(
                    "failed to assign libinput to seat '{}'",
                    self.session.seat()
                )
            })?;
        let libinput_ctrl = libinput_ctx.clone();
        let libinput_backend = LibinputInputBackend::new(libinput_ctx);

        let udev_backend = UdevBackend::new(self.session.seat())
            .map_err(|e| format!("failed to initialize udev monitor: {e}"))?;

        let mut event_loop: EventLoop<()> =
            EventLoop::try_new().map_err(|e| format!("failed to create calloop EventLoop: {e}"))?;
        let handle = event_loop.handle();

        #[cfg(feature = "kms")]
        let session_rc = Rc::new(RefCell::new(self.session));
        #[cfg(all(feature = "kms-direct", not(feature = "kms")))]
        let session_rc = Rc::new(RefCell::new(self.session));
        let display_rc = Rc::new(RefCell::new(self.display));

        #[cfg(feature = "kms")]
        if let Some(notifier) = session_rc.borrow_mut().take_notifier() {
            let disp = Rc::clone(&display_rc);
            let li = libinput_ctrl.clone();
            let win = Rc::clone(&self.window);
            handle
                .insert_source(notifier, move |event, _, _| match event {
                    SessionEvent::PauseSession => {
                        li.suspend();
                        disp.borrow_mut().set_active(false);
                    }
                    SessionEvent::ActivateSession => {
                        if li.resume().is_err() {
                            eprintln!("desktop-shell: libinput resume failed after VT switch");
                        }
                        disp.borrow_mut().set_active(true);
                        win.request_redraw();
                    }
                })
                .map_err(|e| format!("failed to register libseat notifier: {e}"))?;
        }

        // Ignore udev hotplug events cleanly for now; keep source registered for kernel udev socket.
        handle
            .insert_source(udev_backend, |_event, _, _| {})
            .map_err(|e| format!("failed to register udev source: {e}"))?;

        let cursor_pos = Rc::new(RefCell::new(LogicalPosition::new(
            width as f32 / 2.0,
            height as f32 / 2.0,
        )));
        let cursor_dirty = Rc::new(RefCell::new(true));
        let modifiers = Rc::new(RefCell::new(InputModifiers::default()));

        {
            let win = Rc::clone(&self.window);
            let pos_rc = Rc::clone(&cursor_pos);
            let dirty_rc = Rc::clone(&cursor_dirty);
            let mods_rc = Rc::clone(&modifiers);
            let sess = Rc::clone(&session_rc);
            let _keep_li = libinput_ctrl;

            handle
                .insert_source(libinput_backend, move |event, _, _| {
                    handle_libinput_event(
                        event,
                        width as f32,
                        height as f32,
                        &win,
                        &pos_rc,
                        &dirty_rc,
                        &mods_rc,
                        &sess,
                        &mut on_super_key,
                    );
                })
                .map_err(|e| format!("failed to register libinput event source: {e}"))?;
        }

        // Periodic 16ms tick drives Wayland accept/dispatch + compositor snap transitions.
        handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(16)),
                move |_, _, _| {
                    on_tick(Instant::now());
                    TimeoutAction::ToDuration(Duration::from_millis(16))
                },
            )
            .map_err(|e| format!("failed to register animation timer: {e}"))?;

        // Periodic 2s telemetry refresh tick.
        handle
            .insert_source(
                Timer::from_duration(Duration::from_secs(2)),
                move |_, _, _| {
                    on_telemetry();
                    TimeoutAction::ToDuration(Duration::from_secs(2))
                },
            )
            .map_err(|e| format!("failed to register telemetry timer: {e}"))?;

        let mut slint_fb = vec![Xrgb8888Pixel::background(); width * height];
        let mut present_fb = vec![Xrgb8888Pixel::background(); width * height];

        self.window.request_redraw();

        loop {
            slint::platform::update_timers_and_animations();

            let redrawn = self.window.draw_if_needed(|renderer| {
                renderer.render(&mut slint_fb, width);
            });
            let c_dirty = cursor_dirty.replace(false);
            if redrawn || c_dirty {
                present_fb.copy_from_slice(&slint_fb);
                let cp = *cursor_pos.borrow();
                draw_software_cursor(&mut present_fb, width, height, cp.x as i32, cp.y as i32);
                if let Err(err) = display_rc.borrow_mut().present(&present_fb) {
                    eprintln!("desktop-shell: DRM present error: {err}");
                }
            }

            let timeout = if self.window.has_active_animations() {
                Some(Duration::from_millis(16))
            } else {
                slint::platform::duration_until_next_timer_update()
                    .map(|d| d.min(Duration::from_millis(16)))
                    .or(Some(Duration::from_millis(16)))
            };
            event_loop
                .dispatch(timeout, &mut ())
                .map_err(|e| format!("calloop dispatch error: {e}"))?;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_libinput_event(
    event: InputEvent<LibinputInputBackend>,
    width: f32,
    height: f32,
    window: &MinimalSoftwareWindow,
    cursor_pos: &RefCell<LogicalPosition>,
    cursor_dirty: &RefCell<bool>,
    modifiers: &RefCell<InputModifiers>,
    session: &RefCell<SessionManager>,
    on_super_key: &mut impl FnMut(),
) {
    match event {
        InputEvent::PointerMotion { event } => {
            let mut pos = cursor_pos.borrow_mut();
            pos.x = (pos.x + event.delta_x() as f32).clamp(0.0, (width - 1.0).max(0.0));
            pos.y = (pos.y + event.delta_y() as f32).clamp(0.0, (height - 1.0).max(0.0));
            *cursor_dirty.borrow_mut() = true;
            window.dispatch_event(WindowEvent::PointerMoved { position: *pos });
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let mut pos = cursor_pos.borrow_mut();
            pos.x = (event.x_transformed(width as i32) as f32).clamp(0.0, (width - 1.0).max(0.0));
            pos.y = (event.y_transformed(height as i32) as f32).clamp(0.0, (height - 1.0).max(0.0));
            *cursor_dirty.borrow_mut() = true;
            window.dispatch_event(WindowEvent::PointerMoved { position: *pos });
        }
        InputEvent::PointerButton { event } => {
            let Some(btn) = PointerButtonEvent::button(&event).and_then(map_mouse_button) else {
                return;
            };
            let pos = *cursor_pos.borrow();
            match event.state() {
                ButtonState::Pressed => {
                    window.dispatch_event(WindowEvent::PointerPressed {
                        position: pos,
                        button: btn,
                    });
                }
                ButtonState::Released => {
                    window.dispatch_event(WindowEvent::PointerReleased {
                        position: pos,
                        button: btn,
                    });
                }
            }
        }
        InputEvent::PointerAxis { event } => {
            let (dx, dy) = match event.source() {
                AxisSource::Wheel | AxisSource::WheelTilt => (
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) / 120.0 * 60.0,
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) / 120.0 * 60.0,
                ),
                AxisSource::Finger | AxisSource::Continuous => (
                    event.amount(Axis::Horizontal).unwrap_or(0.0),
                    event.amount(Axis::Vertical).unwrap_or(0.0),
                ),
            };
            if dx != 0.0 || dy != 0.0 {
                window.dispatch_event(WindowEvent::PointerScrolled {
                    position: *cursor_pos.borrow(),
                    delta_x: -(dx as f32),
                    delta_y: -(dy as f32),
                });
            }
        }
        InputEvent::Keyboard { event } => {
            let evdev_code = event.key_code().raw().saturating_sub(8);
            let pressed = event.state() == KeyState::Pressed;
            {
                let mut mods = modifiers.borrow_mut();
                mods.update(evdev_code, pressed);
                if pressed {
                    if let Some(vt) = vt_switch_target(evdev_code, mods.ctrl, mods.alt) {
                        session.borrow_mut().change_vt(vt);
                        return;
                    }
                }
            }
            if pressed && is_super_keycode(evdev_code) {
                on_super_key();
                return;
            }
            let shift = modifiers.borrow().shift;
            if let Some(text) = linux_keycode_to_slint_key(evdev_code, shift) {
                if pressed {
                    window.dispatch_event(WindowEvent::KeyPressed { text });
                } else {
                    window.dispatch_event(WindowEvent::KeyReleased { text });
                }
            }
        }
        _ => {}
    }
}

fn map_mouse_button(btn: MouseButton) -> Option<PointerEventButton> {
    match btn {
        MouseButton::Left => Some(PointerEventButton::Left),
        MouseButton::Right => Some(PointerEventButton::Right),
        MouseButton::Middle => Some(PointerEventButton::Middle),
        MouseButton::Back => Some(PointerEventButton::Back),
        MouseButton::Forward => Some(PointerEventButton::Forward),
        _ => None,
    }
}
