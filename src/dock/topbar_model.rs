//! Pure top-bar layout and motion, kept independent of native windows.

pub use crate::animation::Spring;

#[derive(Debug, Clone, Copy)]
pub struct SurfaceVisual {
    pub scale: f32,
    pub rise: f32,
    pub opacity: f32,
}
pub fn surface_visual(progress: f32) -> SurfaceVisual {
    let p = progress.clamp(0., 1.);
    SurfaceVisual {
        scale: 1.,
        rise: 6. * (1. - p),
        opacity: p,
    }
}
pub fn selected(names: &[String], id: &str, primary: bool) -> bool {
    names
        .iter()
        .any(|name| name == "all" || name == "primary" && primary || name.eq_ignore_ascii_case(id))
}

#[derive(Debug, PartialEq, Eq)]
pub enum DragStep {
    Moving,
    Commit(bool),
    Cancel,
    Finished,
}
pub struct DragGesture {
    start: (i32, i32),
    pub moved: bool,
    finished: bool,
}
impl DragGesture {
    pub fn new(point: (i32, i32)) -> Self {
        Self {
            start: point,
            moved: false,
            finished: false,
        }
    }
    /// Samples global input, so a nonactivating source never depends on captured mouse messages.
    pub fn sample(
        &mut self,
        point: (i32, i32),
        left: bool,
        ctrl: bool,
        escape: bool,
        target: Option<bool>,
    ) -> DragStep {
        if self.finished {
            return DragStep::Finished;
        }
        self.moved |= (point.0 - self.start.0).abs() + (point.1 - self.start.1).abs() > 5;
        if escape || left && !ctrl {
            self.finished = true;
            return DragStep::Cancel;
        }
        if left {
            return DragStep::Moving;
        }
        self.finished = true;
        if self.moved {
            target.map_or(DragStep::Cancel, DragStep::Commit)
        } else {
            DragStep::Cancel
        }
    }
}
pub fn music_visible(present: bool, since_playing: Option<std::time::Duration>) -> bool {
    present && since_playing.is_some_and(|time| time < std::time::Duration::from_secs(12))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Tray(usize),
    Overflow,
    Wifi,
    Sound,
    Battery,
    Controls,
    Previous,
    PlayPause,
    Next,
    Clock,
}

/// Pointer feedback belongs only to the surface carrying that control. With no
/// current hit, repaint both to remove the previous hover highlight.
pub fn interaction_surfaces(hit: Option<Hit>) -> (bool, bool) {
    match hit {
        Some(Hit::Previous | Hit::PlayPause | Hit::Next | Hit::Clock) => (false, true),
        Some(_) => (true, false),
        None => (true, true),
    }
}

pub struct IslandLayout {
    pub clock: f32,
    pub controls: [f32; 3],
    pub button: f32,
    pub title_right: Option<f32>,
}
impl IslandLayout {
    pub fn new(width: f32, compact: bool, time_width: f32) -> Self {
        let button = if compact { 24. } else { 28. };
        let clock = width - 14. - time_width;
        let start = clock - 12. - button * 3.;
        Self {
            clock,
            controls: [start, start + button, start + 2. * button],
            button,
            title_right: (!compact).then_some((start - 10.).max(36.)),
        }
    }
    pub fn media_hit(&self, x: f32) -> Option<Hit> {
        self.controls
            .iter()
            .zip([Hit::Previous, Hit::PlayPause, Hit::Next])
            .find_map(|(left, hit)| (*left..*left + self.button).contains(&x).then_some(hit))
    }
}

#[derive(Clone, Copy)]
pub struct Layout {
    pub width: f32,
    pub height: f32,
    pub inline: usize,
    pub battery: bool,
}
impl Layout {
    pub fn inline_capacity(self) -> usize {
        ((self.width / 2. - 240. - 40.).max(0.) / 28.) as usize
    }
    pub fn right(self) -> f32 {
        self.width - 12. - if self.battery { 152. } else { 104. }
    }
    pub fn hit(self, x: f32, y: f32) -> Option<Hit> {
        if !(0. ..self.height).contains(&y) {
            return None;
        }
        if (12. ..12. + self.inline as f32 * 28.).contains(&x) {
            return Some(Hit::Tray(((x - 12.) / 28.) as usize));
        }
        if (12. + self.inline as f32 * 28. ..40. + self.inline as f32 * 28.).contains(&x) {
            return Some(Hit::Overflow);
        }
        let offset = x - self.right();
        if (0. ..32.).contains(&offset) {
            Some(Hit::Wifi)
        } else if (32. ..64.).contains(&offset) {
            Some(Hit::Sound)
        } else if self.battery && (64. ..112.).contains(&offset) {
            Some(Hit::Battery)
        } else if (if self.battery { 112. } else { 64. }..if self.battery { 152. } else { 104. }).contains(&offset) {
            Some(Hit::Controls)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analytic_motion_advances_by_elapsed_time_instead_of_frame_count() {
        for outline in [false, true] {
            let tick = |spring: &mut Spring, seconds| {
                if outline {
                    spring.tick_outline(seconds)
                } else {
                    spring.tick(seconds)
                }
            };
            let mut regular = Spring::new(0.);
            regular.target = 1.;
            let mut stalled = regular;
            for _ in 0..30 {
                tick(&mut regular, 0.01);
            }
            tick(&mut stalled, 0.3);
            assert!((regular.value - stalled.value).abs() < 0.001);
            assert!((regular.velocity - stalled.velocity).abs() < 0.001);
            assert!(!tick(&mut stalled, 2.));
            assert_eq!((stalled.value, stalled.velocity), (1., 0.));
        }
    }

    #[test]
    fn pointer_feedback_targets_only_its_surface_and_leave_clears_both() {
        assert_eq!(interaction_surfaces(Some(Hit::Sound)), (true, false));
        assert_eq!(interaction_surfaces(Some(Hit::Clock)), (false, true));
        assert_eq!(interaction_surfaces(Some(Hit::PlayPause)), (false, true));
        assert_eq!(interaction_surfaces(None), (true, true));
    }
    #[test]
    fn popup_reversal_preserves_motion_and_stops_at_rest() {
        let mut p = Spring::new(0.);
        p.target = 1.;
        for _ in 0..7 {
            p.tick(0.01);
        }
        let before = (p.value, p.velocity);
        p.target = 0.;
        assert_eq!((p.value, p.velocity), before);
        p.tick(0.01);
        let before = (p.value, p.velocity);
        p.target = 1.;
        assert_eq!((p.value, p.velocity), before);
        for _ in 0..120 {
            p.tick(0.01);
        }
        assert_eq!((p.value, p.velocity), (1., 0.));
        assert!(!p.tick(0.01));
        let full = surface_visual(p.value);
        assert_eq!((full.scale, full.rise, full.opacity), (1., 0., 1.));
        p.target = 0.;
        p.settle();
        let hidden = surface_visual(p.value);
        assert_eq!((hidden.scale, hidden.rise, hidden.opacity), (1., 6., 0.));
    }
    #[test]
    fn background_drag_finishes_from_global_release_without_any_window_mousemove() {
        let mut drag = DragGesture::new((20, 10));
        assert_eq!(drag.sample((25, 60), true, true, false, Some(false)), DragStep::Moving);
        assert_eq!(
            drag.sample((25, 60), false, true, false, Some(false)),
            DragStep::Commit(false)
        );
        assert_eq!(
            drag.sample((25, 60), false, true, false, Some(false)),
            DragStep::Finished
        );
    }
    #[test]
    fn coalesced_mouse_and_ctrl_release_keeps_a_valid_held_drag() {
        let mut drag = DragGesture::new((20, 10));
        assert_eq!(drag.sample((25, 60), true, true, false, Some(false)), DragStep::Moving);
        assert_eq!(
            drag.sample((25, 60), false, false, false, Some(false)),
            DragStep::Commit(false)
        );
    }
    #[test]
    fn overflow_to_background_bar_and_cancel_sequences_are_safe() {
        let mut drag = DragGesture::new((40, 70));
        assert_eq!(
            drag.sample((40, 10), false, true, false, Some(true)),
            DragStep::Commit(true)
        );
        for (ctrl, escape) in [(false, false), (true, true)] {
            let mut drag = DragGesture::new((0, 0));
            assert_eq!(drag.sample((40, 10), true, ctrl, escape, Some(true)), DragStep::Cancel);
            assert_eq!(
                drag.sample((40, 10), false, true, false, Some(true)),
                DragStep::Finished
            );
        }
    }
    #[test]
    fn paused_player_remains_resumable_then_returns_to_idle() {
        use std::time::Duration;
        assert!(music_visible(true, Some(Duration::from_secs(11))));
        assert!(!music_visible(true, Some(Duration::from_secs(12))));
        assert!(!music_visible(false, Some(Duration::ZERO)));
        assert!(!music_visible(true, None));
    }
    #[test]
    fn interrupted_spring_preserves_velocity_and_settles() {
        let mut s = Spring::new(0.);
        s.target = 1.;
        for _ in 0..5 {
            s.tick(0.016);
        }
        let (position, velocity) = (s.value, s.velocity);
        s.target = 0.;
        assert_eq!((s.value, s.velocity), (position, velocity));
        for _ in 0..100 {
            s.tick(0.016);
        }
        assert_eq!((s.value, s.velocity), (0., 0.));
    }
    #[test]
    fn selector_keeps_secondary_choices_and_explicit_empty_selection() {
        assert!(selected(&["all".into()], "display2", false));
        assert!(!selected(&["primary".into()], "display2", false));
        assert!(selected(&["DISPLAY2".into()], "display2", false));
        assert!(!selected(&[], "display1", true));
    }
    #[test]
    fn narrow_bars_keep_tray_out_of_center() {
        for width in [640., 1280., 3840.] {
            let layout = Layout {
                width,
                height: 24.,
                inline: 0,
                battery: true,
            };
            let capacity = layout.inline_capacity();
            assert!(40. + capacity as f32 * 28. <= width / 2. - 200.);
            assert_eq!(layout.hit(layout.right() + 1., 12.), Some(Hit::Wifi));
            assert_eq!(layout.hit(layout.right() + 33., 12.), Some(Hit::Sound));
            assert_eq!(layout.hit(width - 20., 12.), Some(Hit::Controls));
            assert_eq!(layout.hit(width - 20., 24.), None);
        }
    }
    #[test]
    fn music_controls_fit_narrow_and_wide_islands() {
        for width in [180., 240., 288., 320., 500.] {
            let l = IslandLayout::new(width, width < 320., 48.);
            for (x, hit) in l.controls.into_iter().zip([Hit::Previous, Hit::PlayPause, Hit::Next]) {
                assert!(x >= 0. && x + l.button <= width);
                assert_eq!(l.media_hit(x + l.button / 2.), Some(hit));
            }
            assert!(l.clock >= 0. && l.clock + 48. <= width);
        }
    }
    #[test]
    fn layout_anchors_are_continuous_through_music_morph() {
        let a = IslandLayout::new(319.99, false, 48.);
        let b = IslandLayout::new(320.01, false, 48.);
        assert!((a.clock - b.clock).abs() < 0.1);
        for (a, b) in a.controls.into_iter().zip(b.controls) {
            assert!((a - b).abs() < 0.1);
        }
        let initial = IslandLayout::new(320., false, 48.);
        assert_eq!(initial.clock, 258.);
    }
}
