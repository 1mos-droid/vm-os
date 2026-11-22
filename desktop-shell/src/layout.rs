//! Magnetic half/quarter screen snapping and smooth ease-out window transitions.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Reserved height at the top of the output for the top bar (`ui/shell.slint` `bar-h`).
pub const TOP_RAIL_HEIGHT_PX: i32 = 36;

/// Reserved height at the bottom of the output for the floating dock (`dock-h`).
pub const BOTTOM_DOCK_HEIGHT_PX: i32 = 84;

/// Subtle breathing gap around snapped tiles (in logical pixels).
pub const TILE_GAP_PX: i32 = 8;

/// Edge proximity threshold (in logical pixels) that activates magnetic snap preview.
pub const MAGNETIC_EDGE_THRESHOLD_PX: i32 = 28;

/// Default smooth window transition duration (fast, crisp 180 ms ease-out cubic).
pub const DEFAULT_TRANSITION_DURATION: Duration = Duration::from_millis(180);

/// Axis-aligned logical rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
}

/// Magnetic half/quarter/full screen snapping targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapZone {
    LeftHalf,
    RightHalf,
    TopHalf,
    BottomHalf,
    TopLeftQuarter,
    TopRightQuarter,
    BottomLeftQuarter,
    BottomRightQuarter,
    FullWorkspace,
}

impl SnapZone {
    pub fn label(self) -> &'static str {
        match self {
            Self::LeftHalf => "Left Half",
            Self::RightHalf => "Right Half",
            Self::TopHalf => "Top Half",
            Self::BottomHalf => "Bottom Half",
            Self::TopLeftQuarter => "Top-Left Quarter",
            Self::TopRightQuarter => "Top-Right Quarter",
            Self::BottomLeftQuarter => "Bottom-Left Quarter",
            Self::BottomRightQuarter => "Bottom-Right Quarter",
            Self::FullWorkspace => "Full Workspace",
        }
    }

    /// Compute the exact target rectangle for this snap zone within an output of `(out_w, out_h)`,
    /// keeping the top bar and dock clear, with symmetric gaps.
    pub fn target_rect(self, out_w: i32, out_h: i32) -> Rect {
        let usable_x = TILE_GAP_PX;
        let usable_y = TOP_RAIL_HEIGHT_PX + TILE_GAP_PX;
        let usable_w = (out_w - TILE_GAP_PX * 2).max(200);
        let usable_h =
            (out_h - TOP_RAIL_HEIGHT_PX - BOTTOM_DOCK_HEIGHT_PX - TILE_GAP_PX * 2).max(160);

        let half_w = (usable_w - TILE_GAP_PX) / 2;
        let right_x = usable_x + half_w + TILE_GAP_PX;
        let right_w = usable_w - half_w - TILE_GAP_PX;

        let half_h = (usable_h - TILE_GAP_PX) / 2;
        let bottom_y = usable_y + half_h + TILE_GAP_PX;
        let bottom_h = usable_h - half_h - TILE_GAP_PX;

        match self {
            Self::FullWorkspace => Rect::new(usable_x, usable_y, usable_w, usable_h),
            Self::LeftHalf => Rect::new(usable_x, usable_y, half_w, usable_h),
            Self::RightHalf => Rect::new(right_x, usable_y, right_w, usable_h),
            Self::TopHalf => Rect::new(usable_x, usable_y, usable_w, half_h),
            Self::BottomHalf => Rect::new(usable_x, bottom_y, usable_w, bottom_h),
            Self::TopLeftQuarter => Rect::new(usable_x, usable_y, half_w, half_h),
            Self::TopRightQuarter => Rect::new(right_x, usable_y, right_w, half_h),
            Self::BottomLeftQuarter => Rect::new(usable_x, bottom_y, half_w, bottom_h),
            Self::BottomRightQuarter => Rect::new(right_x, bottom_y, right_w, bottom_h),
        }
    }
}

/// Detect magnetic half/quarter snap zone when dragging a window to pointer `(px, py)`
/// on an output of size `(out_w, out_h)`.
pub fn detect_magnetic_snap(px: i32, py: i32, out_w: i32, out_h: i32) -> Option<SnapZone> {
    let t = MAGNETIC_EDGE_THRESHOLD_PX;
    let top_edge = TOP_RAIL_HEIGHT_PX + t;
    let corner_band = t * 3;

    let near_left = px <= t;
    let near_right = px >= out_w - t;
    let near_top = py <= top_edge;
    let near_bottom = py >= out_h - BOTTOM_DOCK_HEIGHT_PX - t;

    let in_top_corner = py <= TOP_RAIL_HEIGHT_PX + corner_band;
    let in_bottom_corner = py >= out_h - BOTTOM_DOCK_HEIGHT_PX - corner_band;
    let in_left_corner = px <= corner_band;
    let in_right_corner = px >= out_w - corner_band;

    if (near_left && in_top_corner) || (near_top && in_left_corner) {
        Some(SnapZone::TopLeftQuarter)
    } else if (near_right && in_top_corner) || (near_top && in_right_corner) {
        Some(SnapZone::TopRightQuarter)
    } else if (near_left && in_bottom_corner) || (near_bottom && in_left_corner) {
        Some(SnapZone::BottomLeftQuarter)
    } else if (near_right && in_bottom_corner) || (near_bottom && in_right_corner) {
        Some(SnapZone::BottomRightQuarter)
    } else if near_left {
        Some(SnapZone::LeftHalf)
    } else if near_right {
        Some(SnapZone::RightHalf)
    } else if near_top {
        Some(SnapZone::FullWorkspace)
    } else if near_bottom {
        Some(SnapZone::BottomHalf)
    } else {
        None
    }
}

/// Smooth ease-out cubic interpolation (`1 - (1 - t)^3`) for window transitions.
pub fn ease_out_cubic(t: f32) -> f32 {
    let clamped = t.clamp(0.0, 1.0);
    let inv = 1.0 - clamped;
    1.0 - inv * inv * inv
}

/// Active animated window geometry transition.
#[derive(Debug, Clone)]
pub struct WindowTransition {
    pub from: Rect,
    pub to: Rect,
    pub started_at: Instant,
    pub duration: Duration,
}

impl WindowTransition {
    pub fn new(from: Rect, to: Rect, started_at: Instant, duration: Duration) -> Self {
        Self {
            from,
            to,
            started_at,
            duration,
        }
    }

    pub fn sample(&self, now: Instant) -> (Rect, bool) {
        if self.duration.is_zero() {
            return (self.to, true);
        }
        let elapsed = now.saturating_duration_since(self.started_at);
        let raw_t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        if raw_t >= 1.0 {
            return (self.to, true);
        }
        let k = ease_out_cubic(raw_t);
        let lerp = |a: i32, b: i32| -> i32 { (a as f32 + (b - a) as f32 * k).round() as i32 };
        (
            Rect::new(
                lerp(self.from.x, self.to.x),
                lerp(self.from.y, self.to.y),
                lerp(self.from.w, self.to.w),
                lerp(self.from.h, self.to.h),
            ),
            false,
        )
    }
}

/// Manages active smooth transitions for all compositor windows keyed by window ID.
#[derive(Debug, Default)]
pub struct TransitionEngine {
    active: HashMap<u64, WindowTransition>,
}

impl TransitionEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start_transition(&mut self, window_id: u64, from: Rect, to: Rect, now: Instant) {
        self.active.insert(
            window_id,
            WindowTransition::new(from, to, now, DEFAULT_TRANSITION_DURATION),
        );
    }

    /// Advance all active transitions, returning current rectangles for animated windows
    /// and pruning completed transitions.
    pub fn step(&mut self, now: Instant) -> Vec<(u64, Rect)> {
        let mut updates = Vec::with_capacity(self.active.len());
        let mut completed = Vec::new();
        for (&id, tr) in &self.active {
            let (rect, done) = tr.sample(now);
            updates.push((id, rect));
            if done {
                completed.push(id);
            }
        }
        for id in completed {
            self.active.remove(&id);
        }
        updates
    }

    pub fn has_active(&self) -> bool {
        !self.active.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnetic_snap_detects_halves_quarters_and_full_workspace() {
        let (w, h) = (1920, 1080);

        // Corners -> Quarters
        assert_eq!(
            detect_magnetic_snap(5, TOP_RAIL_HEIGHT_PX + 10, w, h),
            Some(SnapZone::TopLeftQuarter)
        );
        assert_eq!(
            detect_magnetic_snap(w - 5, TOP_RAIL_HEIGHT_PX + 10, w, h),
            Some(SnapZone::TopRightQuarter)
        );
        assert_eq!(
            detect_magnetic_snap(5, h - 10, w, h),
            Some(SnapZone::BottomLeftQuarter)
        );
        assert_eq!(
            detect_magnetic_snap(w - 5, h - 10, w, h),
            Some(SnapZone::BottomRightQuarter)
        );

        // Edges -> Halves & Full
        assert_eq!(
            detect_magnetic_snap(5, h / 2, w, h),
            Some(SnapZone::LeftHalf)
        );
        assert_eq!(
            detect_magnetic_snap(w - 5, h / 2, w, h),
            Some(SnapZone::RightHalf)
        );
        assert_eq!(
            detect_magnetic_snap(w / 2, TOP_RAIL_HEIGHT_PX + 5, w, h),
            Some(SnapZone::FullWorkspace)
        );
        assert_eq!(
            detect_magnetic_snap(w / 2, h - 5, w, h),
            Some(SnapZone::BottomHalf)
        );

        // Center -> None
        assert_eq!(detect_magnetic_snap(w / 2, h / 2, w, h), None);
    }

    #[test]
    fn snap_rects_never_overlap_top_rail_and_fit_symmetrically() {
        let left = SnapZone::LeftHalf.target_rect(1280, 720);
        let right = SnapZone::RightHalf.target_rect(1280, 720);
        assert!(left.y >= TOP_RAIL_HEIGHT_PX + TILE_GAP_PX);
        assert_eq!(left.x + left.w + TILE_GAP_PX, right.x);
        assert_eq!(right.x + right.w + TILE_GAP_PX, 1280);

        let tl = SnapZone::TopLeftQuarter.target_rect(1280, 720);
        let br = SnapZone::BottomRightQuarter.target_rect(1280, 720);
        assert_eq!(tl.y + tl.h + TILE_GAP_PX, br.y);
        assert_eq!(br.y + br.h + TILE_GAP_PX, 720 - BOTTOM_DOCK_HEIGHT_PX);
    }

    #[test]
    fn smooth_transition_interpolates_and_completes() {
        let mut engine = TransitionEngine::new();
        let t0 = Instant::now();
        let from = Rect::new(100, 100, 600, 400);
        let to = SnapZone::RightHalf.target_rect(1280, 720);
        engine.start_transition(1, from, to, t0);
        assert!(engine.has_active());

        let mid = engine.step(t0 + Duration::from_millis(90));
        assert_eq!(mid.len(), 1);
        // Ease-out cubic at t=0.5 is 0.875 => much closer to `to` than `from`.
        assert!(mid[0].1.x > (from.x + to.x) / 2);
        assert!(engine.has_active());

        let end = engine.step(t0 + Duration::from_millis(200));
        assert_eq!(end, vec![(1, to)]);
        assert!(!engine.has_active());
    }
}
