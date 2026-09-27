use serde::{Deserialize, Serialize};

/// Inclusive byte range `[start, end]`, with `pos` = next byte to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub pos: u64,
    pub end: u64,
}

impl Segment {
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, pos: start, end }
    }

    pub const fn remaining(&self) -> u64 {
        self.end.saturating_add(1).saturating_sub(self.pos)
    }

    pub const fn is_done(&self) -> bool {
        self.pos > self.end
    }
}

/// Splits `size` bytes into at most `n` segments, none smaller than `min_len`.
pub fn plan_segments(size: u64, n: u8, min_len: u64) -> Vec<Segment> {
    if size == 0 {
        return Vec::new();
    }
    let n = u64::from(n.max(1)).min(size.div_ceil(min_len.max(1)));
    let chunk = size.div_ceil(n);
    (0..n)
        .map(|i| i * chunk)
        .take_while(|&s| s < size)
        .map(|s| Segment::new(s, (s + chunk).min(size) - 1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_whole_range_without_gaps() {
        let segs = plan_segments(10_000_001, 32, 1);
        assert_eq!(segs.first().unwrap().start, 0);
        assert_eq!(segs.last().unwrap().end, 10_000_000);
        assert!(segs.windows(2).all(|w| w[0].end + 1 == w[1].start));
    }

    #[test]
    fn respects_min_len() {
        assert_eq!(plan_segments(1000, 32, 500).len(), 2);
    }
}
