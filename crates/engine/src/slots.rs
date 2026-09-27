use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering::*},
};

use domain::Segment;

/// Below this, splitting a segment costs more (new TCP + TLS handshake) than it saves.
pub(crate) const MIN_SPLIT: u64 = 512 * 1024;

/// Live segment shared between its worker (advances `pos`) and the stealer (shrinks `end`).
pub(crate) struct Slot {
    start: u64,
    pub pos: AtomicU64,
    pub end: AtomicU64,
}

impl Slot {
    fn new(s: Segment) -> Arc<Self> {
        Arc::new(Self { start: s.start, pos: s.pos.into(), end: s.end.into() })
    }

    pub fn snapshot(&self) -> Segment {
        Segment { start: self.start, pos: self.pos.load(Acquire), end: self.end.load(Acquire) }
    }
}

#[derive(Default)]
struct Inner {
    all: Vec<Arc<Slot>>,
    /// Unfinished slots whose worker gave up (server connection limit): next in line for idle workers.
    orphans: Vec<Arc<Slot>>,
}

pub(crate) struct Slots(Mutex<Inner>);

impl Slots {
    pub fn new(segments: impl IntoIterator<Item = Segment>) -> Self {
        Self(Mutex::new(Inner { all: segments.into_iter().map(Slot::new).collect(), orphans: Vec::new() }))
    }

    pub fn pending(&self) -> Vec<Arc<Slot>> {
        self.lock().all.iter().filter(|s| !s.snapshot().is_done()).cloned().collect()
    }

    pub fn segments(&self) -> Vec<Segment> {
        self.lock().all.iter().map(|s| s.snapshot()).collect()
    }

    pub fn release(&self, slot: Arc<Slot>) {
        self.lock().orphans.push(slot);
    }

    pub fn downloaded(&self) -> u64 {
        self.segments().iter().map(|s| s.pos.min(s.end.saturating_add(1)).saturating_sub(s.start)).sum()
    }

    pub fn all_done(&self) -> bool {
        self.segments().iter().all(Segment::is_done)
    }

    /// IDM-style dynamic segmentation: hand the second half of the largest remaining segment to an idle worker.
    /// A racing worker may overshoot the new `end` by one buffer: it rewrites identical bytes, so it is harmless.
    pub fn steal(&self) -> Option<Arc<Slot>> {
        let mut inner = self.lock();
        while let Some(orphan) = inner.orphans.pop() {
            if !orphan.snapshot().is_done() {
                return Some(orphan);
            }
        }
        let slots = &mut inner.all;
        // One snapshot for both the choice and the split point: re-reading `pos` after `remaining`
        // could put `mid` past the segment's end if the worker advanced in between.
        let (victim, seg) = slots.iter().map(|s| (s, s.snapshot())).max_by_key(|(_, seg)| seg.remaining())?;
        let remaining = seg.remaining();
        if remaining < 2 * MIN_SPLIT {
            return None;
        }
        let mid = seg.pos + remaining / 2; // ≤ seg.end; a worker already past it just overshoots
        let end = victim.end.swap(mid - 1, AcqRel);
        let slot = Slot::new(Segment::new(mid, end));
        slots.push(slot.clone());
        Some(slot)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steal_splits_largest_in_half() {
        let slots = Slots::new([Segment::new(0, 9 * MIN_SPLIT - 1)]);
        let stolen = slots.steal().unwrap().snapshot();
        let segs = slots.segments();
        assert_eq!(segs[0].end + 1, stolen.start);
        assert_eq!(stolen.end, 9 * MIN_SPLIT - 1);
    }

    #[test]
    fn refuses_tiny_split() {
        assert!(Slots::new([Segment::new(0, MIN_SPLIT)]).steal().is_none());
    }
}
