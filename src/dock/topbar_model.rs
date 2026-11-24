//! Pure top-bar layout and motion, kept independent of native windows.

pub use crate::animation::Spring;
use std::time::{Duration, Instant};

pub const TITLE_DURATION: Duration = Duration::from_secs(5);
pub const EQUALIZER_BAR_COUNT: usize = 4;
pub const EQUALIZER_BAR_WIDTH: f32 = 2.;
pub const EQUALIZER_BAR_STEP: f32 = 4.;
pub const EQUALIZER_IDLE: [f32; EQUALIZER_BAR_COUNT] = [4.; EQUALIZER_BAR_COUNT];

#[derive(Default)]
pub struct TrackTitle {
    pending: bool,
    pub deadline: Option<Instant>,
}
impl TrackTitle {
    pub fn reset(&mut self, has_track: bool) {
        self.pending = has_track;
        self.deadline = None;
    }
    pub fn refresh(&mut self, now: Instant, visible: bool) -> bool {
        if self.pending && visible {
            self.pending = false;
            self.deadline = Some(now + TITLE_DURATION);
            return true;
        }
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.deadline = None;
            return true;
        }
        false
    }
}

pub fn equalizer_bars(playing: bool, levels: Option<[f32; EQUALIZER_BAR_COUNT]>) -> [f32; EQUALIZER_BAR_COUNT] {
    if !playing {
        return EQUALIZER_IDLE;
    }
    levels.map_or([6., 11., 8., 5.], |levels| {
        levels.map(|level| 4. + (32. * level.clamp(0., 1.)).round() / 4.)
    })
}

pub fn artwork_color(bgra: &[u8]) -> Option<[f32; 3]> {
    let (pixels, _) = bgra.as_chunks::<4>();
    let step = (pixels.len() / 256).max(1);
    let [b, g, r, a] = pixels.iter().step_by(step).fold([0_u32; 4], |[b, g, r, a], pixel| {
        [
            b + u32::from(pixel[0]),
            g + u32::from(pixel[1]),
            r + u32::from(pixel[2]),
            a + u32::from(pixel[3]),
        ]
    });
    (a > 0).then(|| [r, g, b].map(|channel| (channel as f32 / a as f32).clamp(0., 1.)))
}

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
    media_right: f32,
}
pub struct MediaLayout {
    pub artwork: bool,
    pub equalizer_left: Option<f32>,
    pub title: Option<(f32, f32)>,
}
impl IslandLayout {
    const LEFT: f32 = 8.;
    const ARTWORK_SLOT: f32 = 26.;
    const EQUALIZER_WIDTH: f32 = (EQUALIZER_BAR_COUNT - 1) as f32 * EQUALIZER_BAR_STEP + EQUALIZER_BAR_WIDTH;
    const TITLE_GAP: f32 = 6.;
    const MEDIA_GAP: f32 = 8.;
    const MIN_TITLE_WIDTH: f32 = 32.;

    fn button_width(compact: bool) -> f32 {
        if compact { 24. } else { 28. }
    }
    pub fn music_width(compact: bool, time_width: f32, artwork: bool, title_width: Option<f32>) -> f32 {
        14. + time_width
            + 12.
            + Self::button_width(compact) * 3.
            + Self::MEDIA_GAP
            + Self::LEFT
            + Self::EQUALIZER_WIDTH
            + if artwork { Self::ARTWORK_SLOT } else { 0. }
            + title_width.map_or(0., |width| Self::TITLE_GAP + width)
            + 1. // Allow for rounding to the physical window width.
    }
    pub fn new(width: f32, compact: bool, time_width: f32) -> Self {
        let button = Self::button_width(compact);
        let clock = width - 14. - time_width;
        let start = clock - 12. - button * 3.;
        Self {
            clock,
            controls: [start, start + button, start + 2. * button],
            button,
            media_right: start - Self::MEDIA_GAP,
        }
    }
    pub fn media(&self, has_artwork: bool) -> MediaLayout {
        let artwork = has_artwork && Self::LEFT + Self::ARTWORK_SLOT + Self::EQUALIZER_WIDTH <= self.media_right;
        let left = Self::LEFT + if artwork { Self::ARTWORK_SLOT } else { 0. };
        let title_left = left + Self::EQUALIZER_WIDTH + Self::TITLE_GAP;
        MediaLayout {
            artwork,
            equalizer_left: (left + Self::EQUALIZER_WIDTH <= self.media_right).then_some(left),
            title: (title_left + Self::MIN_TITLE_WIDTH <= self.media_right).then_some((title_left, self.media_right)),
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
    fn compact_island_uses_available_space_for_title() {
        let layout = IslandLayout::new(285., true, 48.);
        let media = layout.media(true);
        assert!(media.artwork);
        assert!(media.title.unwrap().1 < layout.controls[0]);

        let width = IslandLayout::music_width(true, 48., true, None);
        let compact = IslandLayout::new(width, true, 48.).media(true);
        assert!(compact.artwork && compact.equalizer_left.is_some());
        assert!(compact.title.is_none());
        let narrow = IslandLayout::new(180., true, 48.).media(true);
        assert!(!narrow.artwork && narrow.equalizer_left.is_some());
        let too_narrow = IslandLayout::new(174., true, 48.).media(true);
        assert!(too_narrow.equalizer_left.is_none());
    }
    #[test]
    fn equalizer_maps_each_band_independently_and_has_static_accessible_states() {
        assert_eq!(equalizer_bars(false, Some([1.; 4])), [4.; 4]);
        assert_eq!(equalizer_bars(true, None), [6., 11., 8., 5.]);
        assert_eq!(equalizer_bars(true, Some([0.; 4])), [4.; 4]);
        assert_eq!(equalizer_bars(true, Some([0., 0.25, 0.5, 1.])), [4., 6., 8., 12.]);
        assert_eq!(equalizer_bars(true, Some([0., 1., 0.5, 1.])), [4., 12., 8., 12.]);
    }
    #[test]
    fn title_expires_five_seconds_after_first_visible_frame() {
        let mut title = TrackTitle::default();
        let start = Instant::now();
        title.reset(true);
        assert!(!title.refresh(start, false));
        assert!(title.refresh(start, true));
        assert_eq!(title.deadline, Some(start + Duration::from_secs(5)));
        assert!(!title.refresh(start + Duration::from_millis(4999), true));
        assert!(title.refresh(start + Duration::from_secs(5), true));
        assert!(title.deadline.is_none());
        assert!(!title.refresh(start + Duration::from_secs(6), true));

        title.reset(true);
        assert!(title.refresh(start + Duration::from_secs(6), true));
        assert_eq!(title.deadline, Some(start + Duration::from_secs(11)));
    }
    #[test]
    fn artwork_color_uses_bgra_and_ignores_transparent_pixels() {
        assert_eq!(artwork_color(&[0, 64, 128, 128, 0, 0, 0, 0]), Some([1., 0.5, 0.]));
        assert_eq!(artwork_color(&[0, 0, 0, 0]), None);
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
