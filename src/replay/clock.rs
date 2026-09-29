//! Replay timing.
//!
//! The whole job of this module is turning "this happened at recorded time
//! `t`" into "sleep for a bit, then write", and doing it in a way that stays
//! correct when the user pauses, seeks or changes speed mid-session. The
//! arithmetic is deliberately free of side effects and takes the wall clock as
//! a parameter, so the interesting cases can be tested without sleeping.

use std::time::{Duration, Instant};

/// How much faster than real time to play. Clamped to a range where both
/// "too fast to read" and "too slow to sit through" are unreachable.
pub const MIN_SPEED: f64 = 0.05;
pub const MAX_SPEED: f64 = 64.0;

/// Maps wall-clock time onto recorded time.
///
/// The mapping is `recorded = anchor_record + (wall - anchor_wall) * speed`,
/// rebased every time speed, position or pause state changes. Rebasing rather
/// than accumulating is what keeps a two-hour session from drifting, and what
/// makes seeking exact instead of approximate.
#[derive(Clone, Debug)]
pub struct Clock {
    anchor_record: f64,
    anchor_wall: Instant,
    speed: f64,
    paused: bool,
}

impl Clock {
    /// Start at recorded time `from`, playing `speed` times faster than real.
    pub fn new(from: f64, speed: f64) -> Self {
        Clock {
            anchor_record: from,
            anchor_wall: Instant::now(),
            speed: clamp_speed(speed),
            paused: false,
        }
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Where the recording is right now.
    pub fn position(&self) -> f64 {
        self.record_at(Instant::now())
    }

    /// Where the recording would be at `wall`. The seam that makes this
    /// testable without a real clock.
    pub fn record_at(&self, wall: Instant) -> f64 {
        if self.paused {
            return self.anchor_record;
        }
        self.anchor_record + wall.saturating_duration_since(self.anchor_wall).as_secs_f64() * self.speed
    }

    /// Change speed without jumping the playhead.
    pub fn set_speed(&mut self, speed: f64, now: Instant) {
        self.rebase(self.record_at(now), now);
        self.speed = clamp_speed(speed);
    }

    pub fn pause(&mut self, now: Instant) {
        if !self.paused {
            self.rebase(self.record_at(now), now);
            self.paused = true;
        }
    }

    pub fn resume(&mut self, now: Instant) {
        if self.paused {
            self.anchor_wall = now;
            self.paused = false;
        }
    }

    pub fn toggle_pause(&mut self, now: Instant) -> bool {
        if self.paused {
            self.resume(now);
        } else {
            self.pause(now);
        }
        self.paused
    }

    /// Move the playhead. Seeking while paused stays paused.
    pub fn seek(&mut self, to: f64, now: Instant) {
        self.rebase(to.max(0.0), now);
    }

    /// Advance the playhead by `dt` recorded seconds while paused, drawing
    /// whatever that reveals. A no-op while playing.
    pub fn step(&mut self, dt: f64, now: Instant) -> bool {
        if !self.paused {
            return false;
        }
        self.rebase(self.anchor_record + dt, now);
        true
    }

    /// Nudge the playhead backwards, e.g. `r` to restart.
    pub fn rewind_to(&mut self, to: f64, now: Instant) {
        self.rebase(to.max(0.0), now);
    }

    fn rebase(&mut self, record: f64, now: Instant) {
        self.anchor_record = record;
        self.anchor_wall = now;
    }

    /// How long to wait before writing an event recorded at `target`.
    ///
    /// `None` means "do not wait at all": the event is already due, or the
    /// clock is paused and the caller is stepping by hand.
    pub fn delay_until(&self, target: f64, now: Instant) -> Option<Duration> {
        if self.paused {
            return None;
        }
        let remaining = target - self.record_at(now);
        (remaining > 0.0).then(|| Duration::from_secs_f64(remaining / self.speed))
    }

    /// How far the playhead can move in `wall` time from here.
    pub fn reach_in(&self, wall: Duration) -> f64 {
        if self.paused {
            self.anchor_record
        } else {
            self.anchor_record + wall.as_secs_f64() * self.speed
        }
    }
}

fn clamp_speed(speed: f64) -> f64 {
    if speed.is_finite() && speed > 0.0 {
        speed.clamp(MIN_SPEED, MAX_SPEED)
    } else {
        1.0
    }
}

/// The playback speed ladder: each step is a factor, not an absolute value, so
/// the user can go fast and come back without losing their place on the scale.
pub const SPEED_STEPS: &[f64] = &[0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0];

/// The next speed up the ladder, wrapping at the top.
pub fn faster(speed: f64) -> f64 {
    match SPEED_STEPS.iter().find(|s| **s > speed + 1e-9) {
        Some(s) => *s,
        None => *SPEED_STEPS.last().expect("ladder is not empty"),
    }
}

/// The next speed down the ladder, stopping at the bottom rather than wrapping:
/// a control that silently jumps from 10x to 64x is a bug waiting to happen.
pub fn slower(speed: f64) -> f64 {
    let mut best = SPEED_STEPS[0];
    for s in SPEED_STEPS {
        if *s < speed - 1e-9 {
            best = *s;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: f64) -> Instant {
        base + Duration::from_secs_f64(secs)
    }

    #[test]
    fn real_time_is_real_time_at_one_x() {
        let base = Instant::now();
        let clock = Clock { anchor_record: 10.0, anchor_wall: base, speed: 1.0, paused: false };
        assert!((clock.record_at(at(base, 2.5)) - 12.5).abs() < 1e-9);
    }

    #[test]
    fn speed_scales_elapsed_time() {
        let base = Instant::now();
        let clock = Clock { anchor_record: 0.0, anchor_wall: base, speed: 4.0, paused: false };
        assert!((clock.record_at(at(base, 1.0)) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn slow_down_still_advances() {
        let base = Instant::now();
        let clock = Clock { anchor_record: 0.0, anchor_wall: base, speed: 0.25, paused: false };
        assert!((clock.record_at(at(base, 4.0)) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pausing_freezes_the_playhead() {
        let mut clock = Clock::new(0.0, 1.0);
        let base = clock.anchor_wall;
        clock.pause(at(base, 3.0));
        assert!(clock.is_paused());
        assert!((clock.position() - 3.0).abs() < 1e-6);
        // Time passing must not move it.
        assert!((clock.record_at(at(base, 300.0)) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn resuming_continues_from_where_it_stopped() {
        let mut clock = Clock::new(0.0, 1.0);
        let base = clock.anchor_wall;
        clock.pause(at(base, 3.0));
        clock.resume(at(base, 10.0));
        assert!((clock.record_at(at(base, 12.0)) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn changing_speed_does_not_jump_the_playhead() {
        let mut clock = Clock::new(0.0, 1.0);
        let base = clock.anchor_wall;
        let before = clock.record_at(at(base, 7.0));
        clock.set_speed(8.0, at(base, 7.0));
        assert_eq!(clock.speed(), 8.0);
        let after = clock.record_at(at(base, 7.0));
        assert!((before - after).abs() < 1e-9, "{before} vs {after}");
    }

    #[test]
    fn speed_is_clamped_to_something_usable() {
        assert_eq!(clamp_speed(0.0), 1.0);
        assert_eq!(clamp_speed(-3.0), 1.0);
        assert_eq!(clamp_speed(f64::NAN), 1.0);
        assert_eq!(clamp_speed(1000.0), MAX_SPEED);
        assert_eq!(clamp_speed(0.0001), MIN_SPEED);
    }

    #[test]
    fn seeking_lands_exactly_on_the_target() {
        let mut clock = Clock::new(0.0, 2.0);
        let base = clock.anchor_wall;
        clock.seek(120.0, at(base, 4.0));
        assert!((clock.record_at(at(base, 4.0)) - 120.0).abs() < 1e-9);
    }

    #[test]
    fn seeking_cannot_go_before_the_start() {
        let mut clock = Clock::new(0.0, 1.0);
        let base = clock.anchor_wall;
        clock.seek(-30.0, at(base, 1.0));
        assert!((clock.position()).abs() < 1e-9);
    }

    #[test]
    fn seeking_while_paused_stays_paused() {
        let mut clock = Clock::new(0.0, 1.0);
        let base = clock.anchor_wall;
        clock.pause(at(base, 1.0));
        clock.seek(60.0, at(base, 2.0));
        assert!(clock.is_paused());
        assert!((clock.position() - 60.0).abs() < 1e-9);
    }

    #[test]
    fn stepping_only_moves_when_paused() {
        let mut clock = Clock::new(10.0, 1.0);
        let base = clock.anchor_wall;
        assert!(!clock.step(1.0, at(base, 1.0)), "stepping while playing is a no-op");
        // Pausing rebases the playhead to where it actually is at that instant,
        // so the step lands on top of that, not on top of the starting point.
        clock.pause(at(base, 0.0));
        assert!(clock.step(1.0, at(base, 0.0)));
        assert!((clock.position() - 11.0).abs() < 1e-9);
        clock.step(4.0, at(base, 0.0));
        assert!((clock.position() - 15.0).abs() < 1e-9);
    }

    #[test]
    fn delay_is_measured_in_wall_time_not_recorded_time() {
        let base = Instant::now();
        let clock = Clock { anchor_record: 0.0, anchor_wall: base, speed: 4.0, paused: false };
        // Ten recorded seconds take two and a half real ones at 4x.
        let d = clock.delay_until(10.0, at(base, 0.0)).unwrap();
        assert!((d.as_secs_f64() - 2.5).abs() < 1e-9);
        // Already-due events must not sleep.
        assert!(clock.delay_until(-1.0, at(base, 0.0)).is_none());
    }

    #[test]
    fn a_paused_clock_never_sleeps() {
        let mut clock = Clock::new(0.0, 1.0);
        clock.pause(Instant::now());
        assert!(clock.delay_until(999.0, Instant::now()).is_none());
    }

    #[test]
    fn the_speed_ladder_climbs_and_stops() {
        assert_eq!(faster(1.0), 2.0);
        assert_eq!(faster(2.0), 4.0);
        assert_eq!(faster(64.0), 64.0, "the top of the ladder is a wall");
        assert_eq!(slower(4.0), 2.0);
        assert_eq!(slower(0.1), 0.1, "the bottom of the ladder is a wall");
        // Walking all the way down lands on the slowest step, not back at the top.
        let mut s = 16.0;
        for _ in 0..20 {
            s = slower(s);
        }
        assert_eq!(s, 0.1);
    }
}
