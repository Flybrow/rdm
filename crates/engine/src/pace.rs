//! Adaptive connection count, the way TCP adapts its window (AIMD): a download starts with a few
//! connections, doubles them every second while all of them stay healthy, then adds one at a time
//! once trouble has been seen; when connections fail (timeouts, resets, a server refusing more
//! with 429/503) it drops a quarter of them — half for an explicit refusal. A weak Wi-Fi, a
//! saturated router or a picky server thus settle on what they can take, instead of every
//! connection timing out ("délai dépassé") and the download failing.

use std::{
    sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed},
    time::{Duration, Instant},
};

/// Connections opened at first; more join while the server and the network keep up.
pub(crate) const INITIAL_CONNECTIONS: usize = 8;
/// After a connection failed, no connection is added for this long.
const COOLDOWN: Duration = Duration::from_secs(8);
/// A burst of failures (the network dropped) counts once, not once per connection.
const CUT_SPACING: Duration = Duration::from_secs(2);
/// The last connection gives up only once nothing has arrived for this long (network down,
/// server gone); the download then fails with its resume point kept, and the app retries later.
pub(crate) const STALL_LIMIT: Duration = Duration::from_secs(180);

pub(crate) struct Pace {
    max: usize,
    limit: AtomicUsize,
    epoch: Instant,
    /// Milliseconds since `epoch`, plus one (0 = never).
    last_trouble: AtomicU64,
    last_cut: AtomicU64,
    last_progress: AtomicU64,
}

impl Pace {
    pub fn new(max: usize) -> Self {
        let max = max.max(1);
        let pace = Self {
            max,
            limit: AtomicUsize::new(max.min(INITIAL_CONNECTIONS)),
            epoch: Instant::now(),
            last_trouble: AtomicU64::new(0),
            last_cut: AtomicU64::new(0),
            last_progress: AtomicU64::new(0),
        };
        pace.progressed(); // the stall clock starts now
        pace
    }

    fn now(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX - 1) + 1
    }

    /// Connections allowed right now.
    pub fn limit(&self) -> usize {
        self.limit.load(Relaxed)
    }

    /// A connection failed while `active` were open.
    pub fn trouble(&self, active: usize, refused: bool) {
        let now = self.now();
        self.last_trouble.store(now, Relaxed);
        let last = self.last_cut.load(Relaxed);
        if last != 0 && now.saturating_sub(last) < millis(CUT_SPACING) {
            return;
        }
        if self.last_cut.compare_exchange(last, now, Relaxed, Relaxed).is_ok() {
            let target = if refused { active / 2 } else { active * 3 / 4 };
            self.limit.fetch_min(target.max(1), Relaxed);
        }
    }

    /// Called once per second: raises the limit if the connections have been healthy, never above
    /// `cap` (see [`speed_cap`]).
    pub fn ramp(&self, cap: usize) -> usize {
        let trouble = self.last_trouble.load(Relaxed);
        let limit = self.limit.load(Relaxed);
        let next = if trouble == 0 {
            limit.saturating_mul(2) // slow start: nothing went wrong yet
        } else if self.now().saturating_sub(trouble) >= millis(COOLDOWN) {
            limit + 1
        } else {
            limit
        }
        .clamp(1, self.max.min(cap).max(1));
        self.limit.store(next, Relaxed);
        next
    }

    /// Lowers the limit to `cap` right away (a speed limit was set).
    pub fn cap(&self, cap: usize) {
        self.limit.fetch_min(cap.max(1), Relaxed);
    }

    /// Bytes arrived.
    pub fn progressed(&self) {
        self.last_progress.store(self.now(), Relaxed);
    }

    /// Nothing has arrived for `STALL_LIMIT`.
    pub fn stalled(&self) -> bool {
        self.now().saturating_sub(self.last_progress.load(Relaxed)) > millis(STALL_LIMIT)
    }
}

/// Under a speed limit (bytes per second, 0 = none), more connections only share the same bytes
/// and wait longer for their turn — long enough, with many of them, for the server to time them
/// out: one connection per 256 KiB/s allowed.
pub(crate) fn speed_cap(limit: u64) -> usize {
    if limit == 0 { usize::MAX } else { usize::try_from(limit / (256 << 10)).unwrap_or(usize::MAX).max(1) }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_small_and_doubles_while_healthy() {
        let pace = Pace::new(32);
        assert_eq!(pace.limit(), INITIAL_CONNECTIONS);
        assert_eq!(pace.ramp(usize::MAX), 16);
        assert_eq!(pace.ramp(usize::MAX), 32);
        assert_eq!(pace.ramp(usize::MAX), 32, "never above the configured maximum");
        assert_eq!(Pace::new(3).limit(), 3);
    }

    #[test]
    fn backs_off_on_trouble_and_holds_during_cooldown() {
        let pace = Pace::new(32);
        pace.ramp(usize::MAX);
        pace.trouble(16, false);
        assert_eq!(pace.limit(), 12, "a quarter fewer");
        pace.trouble(12, false);
        assert_eq!(pace.limit(), 12, "a burst of failures is one event");
        assert_eq!(pace.ramp(usize::MAX), 12, "no new connection right after trouble");
    }

    #[test]
    fn a_refusal_halves_and_never_reaches_zero() {
        let pace = Pace::new(8);
        pace.trouble(8, true);
        assert_eq!(pace.limit(), 4);
        let single = Pace::new(1);
        single.trouble(1, true);
        assert_eq!(single.limit(), 1);
    }

    #[test]
    fn a_speed_limit_caps_connections() {
        assert_eq!(speed_cap(0), usize::MAX);
        assert_eq!(speed_cap(10 << 10), 1, "10 KiB/s: one connection");
        assert_eq!(speed_cap(1 << 20), 4);
        let pace = Pace::new(32);
        pace.cap(speed_cap(512 << 10));
        assert_eq!(pace.limit(), 2);
        assert_eq!(pace.ramp(speed_cap(512 << 10)), 2);
        assert_eq!(pace.ramp(usize::MAX), 4, "limit lifted: growing again");
    }

    #[test]
    fn not_stalled_at_start() {
        assert!(!Pace::new(4).stalled());
    }
}
