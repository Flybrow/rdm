//! Global token-bucket speed limiter shared by every connection of every download.

use std::{
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct RateLimit {
    /// Bytes per second; 0 = unlimited (fast path, no lock).
    rate: AtomicU64,
    bucket: Mutex<(Instant, f64)>,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self { rate: AtomicU64::new(0), bucket: Mutex::new((Instant::now(), 0.0)) }
    }
}

impl RateLimit {
    pub fn set(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.rate.load(Relaxed)
    }

    /// Accounts for `n` received bytes, sleeping just enough to hold the configured rate.
    pub async fn take(&self, n: usize) {
        let wait = self.debit(n, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    fn debit(&self, n: usize, now: Instant) -> Duration {
        let rate = self.rate.load(Relaxed);
        if rate == 0 {
            return Duration::ZERO;
        }
        let rate = rate as f64;
        let mut bucket = self.bucket.lock().unwrap_or_else(PoisonError::into_inner);
        let (last, tokens) = *bucket;
        // Burst capped at 1/4 s so the limit is smooth, not bursty.
        let tokens = (tokens + now.saturating_duration_since(last).as_secs_f64() * rate).min(rate / 4.0) - n as f64;
        *bucket = (now, tokens);
        if tokens < 0.0 { Duration::from_secs_f64(-tokens / rate) } else { Duration::ZERO }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        assert_eq!(RateLimit::default().debit(1 << 30, Instant::now()), Duration::ZERO);
    }

    #[test]
    fn debt_turns_into_proportional_wait() {
        let limit = RateLimit::default();
        limit.set(1_000_000);
        let now = Instant::now();
        let wait = limit.debit(2_000_000, now);
        assert!((wait.as_secs_f64() - 2.0).abs() < 0.3, "{wait:?}");
    }
}
