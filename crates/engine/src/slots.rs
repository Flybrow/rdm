use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::*},
    },
    time::{Duration, Instant},
};

use domain::Segment;
use tokio_util::sync::CancellationToken;

/// Below this, splitting a segment costs more (new TCP + TLS handshake) than it saves.
pub(crate) const MIN_SPLIT: u64 = 512 * 1024;
/// A connection this many times slower than the typical one is a straggler (a bad path: a lossy
/// route, a busy server behind the balancer)…
const STRAGGLER_RATIO: u64 = 4;
/// …judged once it had this long to get going (TCP slow start is slow on distant servers)…
const STRAGGLER_AFTER: Duration = Duration::from_secs(2);
/// …and replaced when its work would still take it this long.
const TAIL_ETA: Duration = Duration::from_millis(1500);
/// This long without a single byte while the others receive: a dead connection.
const DEAD_AFTER: Duration = Duration::from_secs(2);
/// A connection's speed is trusted after this long (handshake, TCP slow start).
const WARM: Duration = Duration::from_secs(1);
/// A finished piece tells its connection's speed if it took at least this long, for this long.
const MEASURABLE: Duration = Duration::from_millis(250);
const RECENT: Duration = Duration::from_secs(3);

/// Live segment shared between its worker (advances `pos`, `head`) and the stealer (shrinks `end`).
pub(crate) struct Slot {
    start: u64,
    /// Next byte to write: everything before it is on disk.
    pub pos: AtomicU64,
    pub end: AtomicU64,
    /// Next byte to arrive: `pos` plus what the worker holds in its buffer. Splits start here, so a
    /// stolen piece never asks again for bytes already received.
    pub head: AtomicU64,
    /// Bytes per second over the last ticks (0: not measured yet), and the best it reached.
    rate: AtomicU64,
    peak: AtomicU64,
    /// `head` at the previous tick, and where the current request started.
    seen: AtomicU64,
    from: AtomicU64,
    /// Milliseconds since a worker took the slot, and without a byte since.
    age: AtomicU64,
    idle: AtomicU64,
    /// A worker is on it (a released slot waits for one).
    attached: AtomicBool,
    /// Cancels the current request of the slot's worker (a dead connection).
    kill: Mutex<CancellationToken>,
}

impl Slot {
    fn new(s: Segment) -> Arc<Self> {
        Arc::new(Self {
            start: s.start,
            pos: s.pos.into(),
            end: s.end.into(),
            head: s.pos.into(),
            rate: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            seen: s.pos.into(),
            from: s.pos.into(),
            age: AtomicU64::new(0),
            idle: AtomicU64::new(0),
            attached: AtomicBool::new(false),
            kill: Mutex::new(CancellationToken::new()),
        })
    }

    pub fn snapshot(&self) -> Segment {
        Segment { start: self.start, pos: self.pos.load(Acquire), end: self.end.load(Acquire) }
    }

    /// A worker starts a request on this slot: measured afresh; the token cancels that request.
    pub fn attach(&self) -> CancellationToken {
        let pos = self.pos.load(Acquire);
        self.head.store(pos, Release);
        self.seen.store(pos, Relaxed);
        self.from.store(pos, Relaxed);
        self.rate.store(0, Relaxed);
        self.peak.store(0, Relaxed);
        self.age.store(0, Relaxed);
        self.idle.store(0, Relaxed);
        self.attached.store(true, Release);
        let token = CancellationToken::new();
        *self.kill.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = token.clone();
        token
    }

    /// Cancels the worker's current request.
    fn cancel(&self) {
        self.kill.lock().unwrap_or_else(std::sync::PoisonError::into_inner).cancel();
    }

    /// Where the bytes still to fetch start (after what already arrived), and how many they are.
    fn ahead(&self) -> (u64, u64) {
        let head = self.head.load(Acquire).max(self.pos.load(Acquire));
        (head, self.end.load(Acquire).saturating_add(1).saturating_sub(head))
    }
}

#[derive(Default)]
struct Inner {
    all: Vec<Arc<Slot>>,
    /// Unfinished slots whose worker gave up (server connection limit): next in line for idle workers.
    orphans: Vec<Arc<Slot>>,
}

pub(crate) struct Slots {
    inner: Mutex<Inner>,
    /// Median speed of the measured connections, bytes per second (0: unknown yet).
    typical: AtomicU64,
    /// Speeds of the pieces finished lately (a small file's connections finish before they are
    /// measured, and must still count), with when they finished.
    finished: Mutex<Vec<(Instant, u64)>>,
}

impl Slots {
    pub fn new(segments: impl IntoIterator<Item = Segment>) -> Self {
        let all = segments.into_iter().map(Slot::new).collect();
        Self { inner: Mutex::new(Inner { all, orphans: Vec::new() }), typical: AtomicU64::new(0), finished: Mutex::default() }
    }

    pub fn pending(&self) -> Vec<Arc<Slot>> {
        self.lock().all.iter().filter(|s| !s.snapshot().is_done()).cloned().collect()
    }

    pub fn segments(&self) -> Vec<Segment> {
        self.lock().all.iter().map(|s| s.snapshot()).collect()
    }

    /// A connection got `bytes` in `took` for a piece now finished.
    pub fn finished(&self, bytes: u64, took: Duration) {
        // Too short to say anything (a piece ending right away).
        if took < MEASURABLE {
            return;
        }
        let mut finished = self.finished.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        finished.retain(|(at, _)| at.elapsed() < RECENT);
        if finished.len() < 64 {
            finished.push((Instant::now(), (bytes as f64 / took.as_secs_f64()) as u64));
        }
    }

    pub fn release(&self, slot: Arc<Slot>) {
        slot.attached.store(false, Release);
        self.lock().orphans.push(slot);
    }

    pub fn downloaded(&self) -> u64 {
        self.segments().iter().map(|s| s.pos.min(s.end.saturating_add(1)).saturating_sub(s.start)).sum()
    }

    pub fn all_done(&self) -> bool {
        self.segments().iter().all(Segment::is_done)
    }

    /// At each tick (`dt` since the last one): each connection's speed and the typical one; then
    /// dead connections and stragglers get their request cancelled — the piece goes back to the
    /// others (see the worker) and the connection is closed, never reused for another piece.
    pub fn measure(&self, dt: Duration) {
        let inner = self.lock();
        let secs = dt.as_secs_f64().max(0.001);
        let ms = millis(dt);
        let live: Vec<&Arc<Slot>> = inner.all.iter().filter(|s| s.attached.load(Acquire) && !s.snapshot().is_done()).collect();
        // (slot, smoothed speed, speed during this tick)
        let mut speeds = Vec::with_capacity(live.len());
        let mut moving = false;
        for slot in &live {
            let head = slot.head.load(Acquire);
            let delta = head.saturating_sub(slot.seen.swap(head, Relaxed));
            let now = (delta as f64 / secs) as u64;
            let first = slot.age.fetch_add(ms, Relaxed) == 0;
            let rate = if first { now } else { (slot.rate.load(Relaxed) + now) / 2 };
            slot.rate.store(rate, Relaxed);
            slot.peak.fetch_max(rate, Relaxed);
            if delta == 0 {
                slot.idle.fetch_add(ms, Relaxed);
            } else {
                slot.idle.store(0, Relaxed);
                moving = true;
            }
            speeds.push((*slot, rate, now));
        }
        let mut rates: Vec<u64> =
            speeds.iter().filter(|(s, rate, _)| s.age.load(Relaxed) >= millis(WARM) && *rate > 0).map(|&(_, rate, _)| rate).collect();
        let finished = self.finished.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        rates.extend(finished.iter().filter(|(at, _)| at.elapsed() < RECENT).map(|&(_, rate)| rate));
        rates.sort_unstable();
        // The median of the connections receiving now and of the pieces just finished (one
        // outlier cannot make every other connection look slow), fading slowly after a better
        // past: near the end, the fast connections are done and the ones left may all be stragglers.
        let fading = self.typical.load(Relaxed);
        let typical = rates.get(rates.len() / 2).copied().unwrap_or(0).max(fading - fading / 8);
        self.typical.store(typical, Relaxed);
        // Only while the others receive: in a network outage every connection is idle, and none
        // of them is to blame. And only once data flowed: a server slow to answer is not dead.
        if !moving {
            return;
        }
        let flowed = |s: &Slot| s.head.load(Acquire) > s.from.load(Relaxed);
        for (slot, ..) in speeds.iter().filter(|(s, ..)| s.idle.load(Relaxed) >= millis(DEAD_AFTER) && s.age.load(Relaxed) >= millis(DEAD_AFTER) && flowed(s)) {
            slot.idle.store(0, Relaxed);
            slot.cancel();
        }
        // Connections that never came near the others' speed — a bad path from the start, not a
        // network slowing down for everyone (their best speed was fine) — no longer speeding up,
        // with work left for a while.
        if typical == 0 || rates.is_empty() {
            return;
        }
        let stragglers = speeds.iter().filter(|&&(s, rate, now)| {
            let left = s.ahead().1;
            s.age.load(Relaxed) >= millis(STRAGGLER_AFTER)
                && flowed(s)
                && rate > 0
                && s.peak.load(Relaxed).saturating_mul(STRAGGLER_RATIO) < typical
                && now <= rate + rate / 4
                && u128::from(left) * 1000 > u128::from(rate) * TAIL_ETA.as_millis()
        });
        for (slot, ..) in stragglers {
            slot.cancel();
        }
    }

    /// Work for an idle connection (IDM-style dynamic segmentation): a released piece first; else
    /// the piece expected to finish last is split — in half, or by speed when its connection is
    /// measured, so both parts end together. A racing worker may overshoot the new `end` by one
    /// buffer: it rewrites identical bytes, which is harmless.
    ///
    /// `merge` (a server refusing connections): the first released piece in file order, joined
    /// with the untouched released pieces that follow it — one request instead of several.
    /// Without `split` (a server counting requests), live pieces are never split.
    pub fn steal(&self, merge: bool, split: bool) -> Option<Arc<Slot>> {
        let mut inner = self.lock();
        inner.orphans.retain(|o| !o.snapshot().is_done());
        if merge && let Some(first) = inner.orphans.iter().min_by_key(|o| o.start).cloned() {
            inner.orphans.retain(|o| !Arc::ptr_eq(o, &first));
            while Self::absorb(&mut inner, &first) {}
            return Some(first);
        }
        if let Some(orphan) = inner.orphans.pop() {
            return Some(orphan);
        }
        if !split {
            return None;
        }
        let typical = self.typical.load(Relaxed);
        let measured = |s: &Slot| (s.age.load(Relaxed) >= millis(WARM)).then(|| s.rate.load(Relaxed));
        // Time left (scaled): bytes over the connection's speed, or the typical one.
        let eta = |s: &Slot, left: u64| -> u128 {
            match (measured(s), typical) {
                (_, 0) => u128::from(left),
                (Some(rate), _) => u128::from(left) * 1024 / u128::from(rate.max(1)),
                (None, typical) => u128::from(left) * 1024 / u128::from(typical),
            }
        };
        let slots = &mut inner.all;
        // One reading of each slot for both the choice and the split point; only pieces worth a
        // new request (a small one is left to its connection, or to the straggler check).
        let (victim, (head, left)) =
            slots.iter().map(|s| (s, s.ahead())).filter(|(_, (_, left))| *left >= 2 * MIN_SPLIT).max_by_key(|(s, (_, left))| eta(s, *left))?;
        // What the victim keeps: both parts end together, the new connection expected at the
        // typical speed.
        let keep = match (measured(victim), typical) {
            (Some(r), t) if t > 0 => u64::try_from(u128::from(left) * u128::from(r) / (u128::from(r) + u128::from(t))).unwrap_or(left / 2),
            _ => left / 2,
        }
        .clamp(MIN_SPLIT, left - MIN_SPLIT);
        let mid = head + keep; // > head, and ≤ the end the victim had when read
        let end = victim.end.swap(mid - 1, AcqRel);
        if end < mid {
            // The worker got there first: nothing left to share.
            victim.end.store(end, Release);
            return None;
        }
        let slot = Slot::new(Segment::new(mid, end));
        slots.push(slot.clone());
        Some(slot)
    }

    /// A refused connection's next piece: from the first missing byte, joined with the untouched
    /// free pieces that follow (see `steal`). Its own piece, if not done, goes among the free ones
    /// first — in one step: no other connection can take it in between (two connections writing
    /// one piece would mark bytes as written that were not).
    pub fn restart_lowest(&self, current: &Arc<Slot>) -> Arc<Slot> {
        let mut inner = self.lock();
        if !current.snapshot().is_done() && !inner.orphans.iter().any(|o| Arc::ptr_eq(o, current)) {
            current.attached.store(false, Release);
            inner.orphans.push(current.clone());
        }
        inner.orphans.retain(|o| !o.snapshot().is_done());
        let Some(first) = inner.orphans.iter().min_by_key(|o| o.start).cloned() else { return current.clone() };
        inner.orphans.retain(|o| !Arc::ptr_eq(o, &first));
        while Self::absorb(&mut inner, &first) {}
        first
    }

    /// An `open` request (asked to the end of the file) reaching the end of `slot` may run on into
    /// the next piece — no new request: when nobody works on it and nothing of it was fetched yet;
    /// or, with `through` (a server counting requests), when it is already done: its bytes are read
    /// again. `Some(bytes)`: taken over, of which `bytes` were already counted as downloaded.
    pub fn run_on(&self, slot: &Slot, open: bool, through: bool) -> Option<u64> {
        if !open {
            return None;
        }
        let mut inner = self.lock();
        if Self::absorb(&mut inner, slot) {
            return Some(0);
        }
        if !through {
            return None;
        }
        let next = slot.end.load(Acquire).saturating_add(1);
        let i = inner.all.iter().position(|o| o.start == next && o.snapshot().is_done())?;
        let done = inner.all.swap_remove(i);
        inner.orphans.retain(|o| !Arc::ptr_eq(o, &done));
        let end = done.end.load(Acquire);
        slot.end.store(end, Release);
        Some(end + 1 - done.start)
    }

    /// Extends `slot` over the released, untouched piece right after it.
    fn absorb(inner: &mut Inner, slot: &Slot) -> bool {
        let next = slot.end.load(Acquire).saturating_add(1);
        let untouched = |o: &Arc<Slot>| o.start == next && o.pos.load(Acquire) == o.start && !o.attached.load(Acquire);
        let Some(i) = inner.orphans.iter().position(untouched) else { return false };
        let taken = inner.orphans.swap_remove(i);
        inner.all.retain(|s| !Arc::ptr_eq(s, &taken));
        slot.end.store(taken.end.load(Acquire), Release);
        true
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_secs(1);

    /// Ticks of `TICK` in `d`.
    fn ticks(d: Duration) -> u64 {
        d.as_secs()
    }

    #[test]
    fn steal_splits_largest_in_half() {
        let slots = Slots::new([Segment::new(0, 9 * MIN_SPLIT - 1)]);
        let stolen = slots.steal(false, true).unwrap().snapshot();
        let segs = slots.segments();
        assert_eq!(segs[0].end + 1, stolen.start);
        assert_eq!(stolen.end, 9 * MIN_SPLIT - 1);
        assert_eq!(stolen.start, 9 * MIN_SPLIT / 2);
    }

    #[test]
    fn refuses_tiny_split() {
        assert!(Slots::new([Segment::new(0, MIN_SPLIT)]).steal(false, true).is_none());
    }

    /// Advances `slot` by `bytes` as its worker would (received, not yet written).
    fn receive(slot: &Slot, bytes: u64) {
        slot.head.fetch_add(bytes, AcqRel);
    }

    #[test]
    fn splits_start_after_what_already_arrived() {
        let slots = Slots::new([Segment::new(0, 8 * MIN_SPLIT - 1)]);
        let slot = slots.pending().remove(0);
        slot.attach();
        receive(&slot, 2 * MIN_SPLIT);
        let stolen = slots.steal(false, true).unwrap().snapshot();
        assert_eq!(stolen.start, 2 * MIN_SPLIT + 3 * MIN_SPLIT, "half of what is left after the received bytes");
    }

    /// A server counting requests refused the other connections: their untouched pieces, released,
    /// make one request — and a request reaching its end runs on into the next free piece.
    #[test]
    fn free_pieces_make_one_request_for_a_server_counting_them() {
        let piece = 4 * MIN_SPLIT;
        let slots = Slots::new((0..5).map(|i| Segment::new(i * piece, (i + 1) * piece - 1)));
        let all = slots.pending();
        for slot in &all {
            slot.attach();
        }
        // Pieces 1, 2 and 4 were refused (released untouched); 3 keeps its connection.
        for i in [4, 1, 2] {
            slots.release(all[i].clone());
        }
        let merged = slots.steal(true, true).unwrap().snapshot();
        assert_eq!((merged.start, merged.end), (piece, 3 * piece - 1), "1 and 2 joined, not 4 (3 is taken)");
        assert_eq!(slots.segments().len(), 4, "the pieces still tile the file");
        // Piece 3 ends; piece 4 is free and untouched: its open request runs on.
        assert_eq!(slots.run_on(&all[3], false, true), None, "a request asked to its own end stops there");
        assert_eq!(slots.run_on(&all[3], true, false), Some(0));
        assert_eq!(all[3].end.load(Acquire), 5 * piece - 1);
        assert!(slots.steal(true, false).is_none(), "nothing free, and live pieces are not split");
        assert!(slots.steal(true, true).is_some(), "unless splitting is allowed");
        assert_eq!(slots.run_on(&all[3], true, true), None, "the end of the file");
        assert_eq!(slots.run_on(&all[0], true, true), None, "piece 1 is taken now");
        let mut segs = slots.segments();
        segs.sort_by_key(|s| s.start);
        assert!(segs.windows(2).all(|w| w[0].end + 1 == w[1].start), "{segs:?}");
    }

    /// A refused connection starts again from the first missing byte, over the free pieces after
    /// it; its own piece stays reachable by the others, never taken twice.
    #[test]
    fn a_refused_connection_restarts_from_the_first_missing_byte() {
        let piece = 4 * MIN_SPLIT;
        let slots = Slots::new((0..4).map(|i| Segment::new(i * piece, (i + 1) * piece - 1)));
        let all = slots.pending();
        for slot in &all {
            slot.attach();
        }
        slots.release(all[0].clone()); // refused earlier, untouched
        slots.release(all[1].clone());
        // The connection on piece 2 is refused in turn, before its first byte: one request for
        // pieces 0, 1 and its own 2 (piece 3 keeps its connection).
        let next = slots.restart_lowest(&all[2]);
        assert!(Arc::ptr_eq(&next, &all[0]));
        assert_eq!(next.end.load(Acquire), 3 * piece - 1);
        assert!(slots.steal(true, false).is_none(), "nothing is handed out twice");
        assert_eq!(slots.segments().len(), 2, "the pieces still tile the file");
        // A refused connection that had started its piece: the part left goes back to the others
        // unless it is the first missing one.
        all[3].pos.store(3 * piece + 10, Release);
        slots.release(all[0].clone());
        let next = slots.restart_lowest(&all[3]);
        assert!(Arc::ptr_eq(&next, &all[0]), "from the first missing byte");
        let again = slots.steal(true, false).expect("piece 3 is free for another connection");
        assert!(Arc::ptr_eq(&again, &all[3]));
        // Its piece done meanwhile, nothing free: it goes on with nothing to fetch.
        all[3].pos.store(4 * piece, Release);
        assert!(Arc::ptr_eq(&slots.restart_lowest(&all[3]), &all[3]));
    }

    /// A server counting requests: a request reaching a piece already downloaded reads it again
    /// rather than stopping — and those bytes are not counted twice.
    #[test]
    fn reads_through_a_done_piece_only_for_a_server_counting_requests() {
        let slots = Slots::new([Segment::new(0, 99), Segment::new(100, 199), Segment::new(200, 299)]);
        let all = slots.pending();
        all[1].pos.store(200, Release); // done by another connection
        assert_eq!(slots.run_on(&all[0], true, false), None, "otherwise a new request is cheaper");
        assert_eq!(slots.run_on(&all[0], true, true), Some(100));
        assert_eq!(all[0].end.load(Acquire), 199);
        assert_eq!(slots.segments().len(), 2);
    }

    #[test]
    fn a_straggler_is_replaced() {
        // Three connections at 1 MiB/s, one at 16 KiB/s with 8 MiB to go (minutes).
        let piece = 16 * MIN_SPLIT;
        let slots = Slots::new((0..4).map(|i| Segment::new(i * piece, (i + 1) * piece - 1)));
        let all = slots.pending();
        let tokens: Vec<CancellationToken> = all.iter().map(|s| s.attach()).collect();
        for tick in 0..ticks(STRAGGLER_AFTER) {
            for (i, slot) in all.iter().enumerate() {
                receive(slot, if i == 3 { 16 << 10 } else { 1 << 20 });
            }
            slots.measure(TICK);
            let judged = tick + 1 >= ticks(STRAGGLER_AFTER);
            assert_eq!(tokens[3].is_cancelled(), judged, "tick {tick}: given time to get going first");
        }
        assert!(tokens[..3].iter().all(|t| !t.is_cancelled()), "the fast ones keep going");
    }

    /// A small file: the fast connections are done before being measured; their finished pieces
    /// still tell what a good speed is, and the one crawling is replaced.
    #[test]
    fn finished_pieces_tell_the_typical_speed() {
        let piece = 16 * MIN_SPLIT;
        let slots = Slots::new([Segment::new(0, piece - 1)]);
        let slow = slots.pending().remove(0);
        let token = slow.attach();
        slots.finished(2 << 20, Duration::from_millis(500)); // another piece, at 4 MiB/s
        for _ in 0..ticks(STRAGGLER_AFTER) {
            receive(&slow, 16 << 10);
            slots.measure(TICK);
        }
        assert!(token.is_cancelled());
    }

    #[test]
    fn one_fast_piece_does_not_make_the_others_stragglers() {
        let piece = 16 * MIN_SPLIT;
        let slots = Slots::new((0..3).map(|i| Segment::new(i * piece, (i + 1) * piece - 1)));
        let all = slots.pending();
        let tokens: Vec<CancellationToken> = all.iter().map(|s| s.attach()).collect();
        slots.finished(64 << 20, Duration::from_millis(500)); // one piece from a hot cache: 128 MiB/s
        for _ in 0..ticks(STRAGGLER_AFTER) + 1 {
            for slot in &all {
                receive(slot, 4 << 20);
            }
            slots.measure(TICK);
        }
        assert!(tokens.iter().all(|t| !t.is_cancelled()));
    }

    #[test]
    fn a_network_slowing_down_for_everyone_replaces_nobody() {
        let piece = 64 * MIN_SPLIT;
        let slots = Slots::new((0..4).map(|i| Segment::new(i * piece, (i + 1) * piece - 1)));
        let all = slots.pending();
        let tokens: Vec<CancellationToken> = all.iter().map(|s| s.attach()).collect();
        for tick in 0..12 {
            // Everyone at 1 MiB/s, then everyone at 32 KiB/s (the Wi-Fi got bad).
            let speed = if tick < 4 { 1 << 20 } else { 32 << 10 };
            for slot in &all {
                receive(slot, speed);
            }
            slots.measure(TICK);
        }
        assert!(tokens.iter().all(|t| !t.is_cancelled()));
    }

    #[test]
    fn a_connection_still_speeding_up_is_not_a_straggler() {
        let piece = 16 * MIN_SPLIT;
        let slots = Slots::new((0..3).map(|i| Segment::new(i * piece, (i + 1) * piece - 1)));
        let all = slots.pending();
        let tokens: Vec<CancellationToken> = all.iter().map(|s| s.attach()).collect();
        let mut distant = 4u64 << 10;
        for _ in 0..ticks(STRAGGLER_AFTER) + 2 {
            for (i, slot) in all.iter().enumerate() {
                receive(slot, if i == 2 { distant } else { 1 << 20 });
            }
            distant *= 2; // TCP slow start on a distant server
            slots.measure(TICK);
        }
        assert!(!tokens[2].is_cancelled());
    }

    #[test]
    fn a_healthy_tail_is_left_alone() {
        let slots = Slots::new([Segment::new(0, MIN_SPLIT), Segment::new(MIN_SPLIT + 1, 2 * MIN_SPLIT)]);
        for slot in slots.pending() {
            slot.attach();
        }
        for _ in 0..ticks(WARM) + 1 {
            for slot in slots.pending() {
                receive(&slot, 64 << 10);
            }
            slots.measure(TICK);
        }
        assert!(slots.steal(false, true).is_none(), "same speed everywhere: splitting a small piece would not help");
    }

    #[test]
    fn a_dead_connection_is_cancelled_only_while_others_move() {
        let slots = Slots::new([Segment::new(0, 8 * MIN_SPLIT - 1), Segment::new(8 * MIN_SPLIT, 16 * MIN_SPLIT - 1)]);
        let [alive, dead] = <[Arc<Slot>; 2]>::try_from(slots.pending()).ok().unwrap();
        let (_, dead_token) = (alive.attach(), dead.attach());
        // Still waiting for the server's first byte while the other one receives: slow, not dead.
        for _ in 0..ticks(DEAD_AFTER) {
            receive(&alive, 100 << 10);
            slots.measure(TICK);
        }
        assert!(!dead_token.is_cancelled(), "a server slow to answer");
        receive(&dead, 10 << 10);
        slots.measure(TICK);
        for _ in 0..ticks(DEAD_AFTER) {
            slots.measure(TICK); // nobody moves: an outage, not a dead connection
        }
        assert!(!dead_token.is_cancelled(), "an outage");
        for _ in 0..ticks(DEAD_AFTER) {
            receive(&alive, 100 << 10);
            slots.measure(TICK);
        }
        assert!(dead_token.is_cancelled());
    }
}
