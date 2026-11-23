//! Shared animation math. Native surfaces own their timers and presentation.
//! Do not create native resources or perform I/O while advancing a transition.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Hidden,
    Opening,
    Open,
    Closing,
}

pub const LOCAL_INTERVAL: u32 = 15;
fn elapsed(seconds: f32) -> f32 {
    if seconds.is_nan() { 0. } else { seconds.max(0.) }
}
pub fn ease(progress: f32) -> f32 {
    let t = progress.clamp(0., 1.);
    1. - (1. - t).powi(3)
}

/// Shared, bounded visibility transition. Retargeting retains position and velocity.
#[derive(Clone, Copy, Debug)]
pub struct Motion {
    pub value: f32,
    pub(crate) velocity: f32,
    target: f32,
    omega: f32,
    pub(crate) remaining: f32,
}
impl Default for Motion {
    fn default() -> Self {
        Self {
            value: 0.,
            velocity: 0.,
            target: 0.,
            omega: 0.,
            remaining: 0.,
        }
    }
}
impl Motion {
    pub fn active(&self) -> bool {
        self.remaining > 0.
    }
    pub fn closing(&self) -> bool {
        matches!(self.phase(), Phase::Closing)
    }
    pub fn phase(&self) -> Phase {
        match (self.active(), self.target == 1.) {
            (true, true) => Phase::Opening,
            (true, false) => Phase::Closing,
            (false, true) => Phase::Open,
            (false, false) => Phase::Hidden,
        }
    }
    pub fn at(value: f32) -> Self {
        Self {
            value,
            target: value,
            ..Self::default()
        }
    }
    pub fn render_opacity(&self) -> f32 {
        self.value.clamp(0., 1.)
    }
    pub fn retarget(&mut self, opening: bool, animate: bool) {
        let milliseconds = if opening { 220 } else { 180 };
        self.retarget_duration(opening, if animate { milliseconds } else { 0 });
    }
    pub(crate) fn retarget_duration(&mut self, opening: bool, milliseconds: u32) {
        self.target = if opening { 1. } else { 0. };
        self.remaining = milliseconds as f32 / 1000.;
        // Preserve the existing 220/180 ms Start curves, scaled to the requested
        // timeline. Velocity remains in value/second when direction changes.
        self.omega = if milliseconds == 0 {
            0.
        } else {
            (if opening { 8.8 } else { 9. }) / self.remaining
        };
        if milliseconds == 0 || (self.value == self.target && self.velocity == 0.) {
            self.settle();
        }
    }
    pub fn settle(&mut self) {
        self.value = self.target;
        self.velocity = 0.;
        self.remaining = 0.;
    }
    pub fn tick(&mut self, seconds: f32) -> bool {
        if !self.active() {
            return false;
        }
        let dt = elapsed(seconds);
        // Successive f32 frame samples can leave a sub-microsecond residue at
        // the exact requested deadline. Do not retain a timer for that residue.
        if dt + f32::EPSILON >= self.remaining {
            self.settle();
            return false;
        }
        self.remaining -= dt;
        let omega = self.omega;
        let offset = self.value - self.target;
        let c = self.velocity + omega * offset;
        let decay = (-omega * dt).exp();
        self.value = self.target + (offset + c * dt) * decay;
        self.velocity = (self.velocity - omega * c * dt) * decay;
        if !(0. ..=1.).contains(&self.value) {
            self.value = self.value.clamp(0., 1.);
            self.velocity = 0.;
        }
        true
    }
    /// Complete presentation before deciding native lifetime. A failed native
    /// update settles the intended target; the returned phase includes that
    /// failure, so a closing owner can retire its window and timer immediately.
    pub fn advance_present(&mut self, seconds: f32, present: impl FnOnce(f32) -> bool) -> Phase {
        self.tick(seconds);
        if !present(self.render_opacity()) {
            self.settle();
        }
        self.phase()
    }
    pub fn offset_dip(&self) -> f32 {
        10. * (1. - self.value)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
}

impl Spring {
    pub fn new(value: f32) -> Self {
        Self {
            value,
            velocity: 0.,
            target: value,
        }
    }
    pub fn tick(&mut self, dt: f32) -> bool {
        // Analytic critically damped spring: retarget without discarding velocity.
        let dt = elapsed(dt);
        let omega = 22.;
        let offset = self.value - self.target;
        let c = self.velocity + omega * offset;
        let decay = (-omega * dt).exp();
        self.value = self.target + (offset + c * dt) * decay;
        self.velocity = (self.velocity - omega * c * dt) * decay;
        if (self.value - self.target).abs() < 0.015 && self.velocity.abs() < 0.04 {
            self.value = self.target;
            self.velocity = 0.;
            false
        } else {
            true
        }
    }
    pub fn settle(&mut self) {
        self.value = self.target;
        self.velocity = 0.;
    }
    /// A small, bounded overshoot for the island's outline only. Retargeting
    /// keeps velocity; hover and text continue to use critically damped motion.
    pub fn tick_outline(&mut self, dt: f32) -> bool {
        let dt = elapsed(dt);
        let damping = 14.76;
        let frequency = 10.3;
        let offset = self.value - self.target;
        let velocity = self.velocity;
        let phase = frequency * dt;
        let decay = (-damping * dt).exp();
        let c = (velocity + damping * offset) / frequency;
        self.value = self.target + decay * (offset * phase.cos() + c * phase.sin());
        self.velocity = decay
            * (velocity * phase.cos()
                - (damping * velocity + (damping * damping + frequency * frequency) * offset) / frequency
                    * phase.sin());
        if (self.value - self.target).abs() < 0.015 && self.velocity.abs() < 0.04 {
            self.settle();
            false
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requested_duration_changes_intermediate_open_and_close_progress() {
        for opening in [true, false] {
            let mut fast = Motion::at(if opening { 0. } else { 1. });
            let mut slow = fast;
            fast.retarget_duration(opening, 180);
            slow.retarget_duration(opening, 500);
            fast.tick(0.1);
            slow.tick(0.1);
            let progress = |m: Motion| if opening { m.value } else { 1. - m.value };
            assert!(progress(fast) > progress(slow) + 0.3, "opening={opening}");
            let mut equal_fraction = Motion::at(if opening { 0. } else { 1. });
            equal_fraction.retarget_duration(opening, 500);
            equal_fraction.tick(0.5 * 0.1 / 0.18);
            assert!((progress(fast) - progress(equal_fraction)).abs() < 0.00001);
            fast.tick(0.08);
            slow.tick(0.4);
            assert!(!fast.active() && !slow.active());
            assert_eq!(fast.value, if opening { 1. } else { 0. });
            assert_eq!(fast.value, slow.value);
        }
    }
    #[test]
    fn default_start_trajectory_and_velocity_survive_duration_retargeting() {
        let mut m = Motion::default();
        m.retarget_duration(true, 220);
        m.tick(0.1);
        assert!((m.value - (1. - 5. * (-4_f32).exp())).abs() < 0.00001);
        let before = (m.value, m.velocity);
        m.retarget_duration(false, 500);
        assert_eq!((m.value, m.velocity), before);
        m.tick(0.02);
        let before = (m.value, m.velocity);
        m.retarget_duration(true, 180);
        assert_eq!((m.value, m.velocity), before);
        m.tick(1.);
        assert_eq!(m.phase(), Phase::Open);
        m.retarget_duration(false, 180);
        m.tick(0.1);
        assert!((m.value - 6. * (-5_f32).exp()).abs() < 0.00001);
    }
    #[test]
    fn presentation_failure_after_advancing_close_releases_owner_on_the_same_tick() {
        let mut m = Motion::at(1.);
        m.retarget_duration(false, 180);
        let mut owns_window = true;
        let mut timer_running = true;
        let mut attempts = 0;
        let phase = m.advance_present(0.015, |opacity| {
            attempts += 1;
            assert!(opacity > 0. && opacity < 1.); // failure occurs during active close
            false // injected SetLayeredWindowAttributes rejection
        });
        if !matches!(phase, Phase::Opening | Phase::Closing) {
            timer_running = false;
        }
        if phase == Phase::Hidden {
            owns_window = false;
        }
        assert_eq!(attempts, 1);
        assert_eq!(phase, Phase::Hidden);
        assert!(!timer_running && !owns_window);
        assert!(!m.active());
        assert!(!m.tick(0.015));
        // Opening failures retain the intended visible endpoint instead.
        let phase = m.advance_present(0., |_| false);
        assert_eq!(phase, Phase::Hidden);
        m.retarget_duration(true, 220);
        assert_eq!(m.advance_present(0.015, |_| false), Phase::Open);
        assert_eq!(m.value, 1.);
    }
    #[test]
    fn visibility_lifecycle_reverses_and_retires() {
        let mut m = Motion::default();
        assert_eq!(m.phase(), Phase::Hidden);
        m.retarget_duration(true, 220);
        assert_eq!(m.phase(), Phase::Opening);
        m.tick(0.07);
        let before = (m.value, m.velocity);
        m.retarget_duration(false, 180);
        assert_eq!((m.value, m.velocity), before);
        assert_eq!(m.phase(), Phase::Closing);
        m.tick(0.18);
        assert_eq!(m.phase(), Phase::Hidden);
        assert!(!m.tick(0.01));
        m.retarget_duration(true, 0);
        assert_eq!(m.phase(), Phase::Open);
    }
    #[test]
    fn elapsed_time_and_invalid_samples_cannot_strand_animation() {
        let mut m = Motion::default();
        m.retarget_duration(true, 220);
        m.tick(f32::NAN);
        assert!(m.value.is_finite());
        m.tick(1.);
        assert_eq!(m.phase(), Phase::Open);
        assert_eq!(m.render_opacity(), 1.);
        m.retarget_duration(false, 180);
        m.tick(f32::INFINITY);
        assert_eq!(m.phase(), Phase::Hidden);
        assert_eq!(m.render_opacity(), 0.);
    }
}
