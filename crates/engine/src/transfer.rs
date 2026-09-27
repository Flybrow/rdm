use std::{
    cmp::min,
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering::*},
    time::Duration,
};

use domain::{Segment, plan_segments};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, header::{HeaderMap, RANGE}};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt},
    task::JoinSet,
    time::{Instant, MissedTickBehavior, interval_at},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    EngineError, Job, Outcome, Progress, RateLimit,
    pace::{Pace, speed_cap},
    probe,
    slots::{MIN_SPLIT, Slot, Slots},
    state_path,
};

/// Per-connection write buffer: 64 connections × 256 KiB = 16 MiB at most, same throughput as 1 MiB.
const BUF: usize = 256 << 10;
/// Resume state written this often while downloading (data synced first).
const CHECKPOINT: Duration = Duration::from_secs(20);
/// How often the pacer may add connections.
const RAMP_EVERY: Duration = Duration::from_secs(1);
/// Longest wait between two attempts of the last connection (network down, busy server).
const MAX_BACKOFF: Duration = Duration::from_secs(15);
/// A server without range support restarts the file from zero after a cut: at most this often.
const MAX_RESTARTS: u32 = 5;

struct Ctx {
    client: Client,
    url: Url,
    headers: HeaderMap,
    path: PathBuf,
    slots: Slots,
    ranges: bool,
    size: Option<u64>,
    stop: CancellationToken,
    progress: Arc<Progress>,
    limit: Arc<RateLimit>,
    pace: Pace,
}

pub(crate) async fn run(
    client: &Client,
    job: &Job,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<Outcome, EngineError> {
    // The first request retries for a while: a pause must not wait for it.
    let info = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(Outcome::Paused),
        info = probe(client, &job.url, &job.headers) => info?,
    };
    if info.hls {
        return crate::hls::run(client, job, progress, cancel).await;
    }
    let state = state_path(&job.target);
    let pace = Pace::new(usize::from(job.connections));
    pace.cap(speed_cap(job.limit.get()));

    // An empty body is how expired/blocked media links (e.g. YouTube) answer: never report it as done.
    if info.size == Some(0) {
        return Err(EngineError::Empty);
    }
    let segments = match info.size {
        Some(size) if info.ranges => match load_state(&state, &job.target, size).await {
            Some(saved) => saved,
            None => {
                preallocate(&job.target, size).await?;
                // A few big pieces: connections added later split the largest remaining one.
                let first = u8::try_from(pace.limit()).unwrap_or(u8::MAX);
                plan_segments(size, first, MIN_SPLIT)
            }
        },
        size => {
            preallocate(&job.target, 0).await?;
            vec![Segment::new(0, size.map_or(u64::MAX - 1, |s| s.saturating_sub(1)))]
        }
    };

    let ctx = Arc::new(Ctx {
        client: client.clone(),
        url: job.url.clone(),
        headers: job.headers.clone(),
        path: job.target.clone(),
        slots: Slots::new(segments),
        ranges: info.ranges,
        size: info.size,
        stop: cancel.child_token(),
        progress,
        limit: job.limit.clone(),
        pace,
    });
    ctx.progress.total.store(info.size.unwrap_or(0), Relaxed);
    ctx.progress.downloaded.store(ctx.slots.downloaded(), Relaxed);

    let mut workers = JoinSet::new();
    // Resumed with more pieces than connections allowed: the rest wait for a free connection.
    let mut pending = ctx.slots.pending().into_iter();
    for slot in pending.by_ref().take(ctx.pace.limit()) {
        spawn(&mut workers, &ctx, slot);
    }
    for slot in pending {
        ctx.slots.release(slot);
    }
    let mut failure = None;
    let mut checkpoint = interval_at(Instant::now() + CHECKPOINT, CHECKPOINT);
    let mut ramp = interval_at(Instant::now() + RAMP_EVERY, RAMP_EVERY);
    ramp.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            joined = workers.join_next() => {
                let Some(res) = joined else { break };
                if let Err(e) = res.map_err(|e| EngineError::Io(e.into())).and_then(|r| r) {
                    ctx.stop.cancel();
                    failure.get_or_insert(e);
                }
            }
            // A crash, a kill or a session closing mid-download loses at most this much.
            _ = checkpoint.tick(), if info.ranges => {
                let _ = persist(&job.target, &state, &ctx.slots.segments()).await;
            }
            // More connections while they help: waiting pieces first, then halves of the largest.
            _ = ramp.tick(), if info.ranges && failure.is_none() && !ctx.stop.is_cancelled() => {
                let limit = ctx.pace.ramp(speed_cap(ctx.limit.get()));
                while ctx.progress.active.load(Acquire) < limit {
                    let Some(slot) = ctx.slots.steal() else { break };
                    spawn(&mut workers, &ctx, slot);
                }
            }
        }
    }

    let done = ctx.slots.all_done();
    if done && failure.is_none() {
        let _ = fs::remove_file(&state).await;
        ctx.progress.downloaded.store(ctx.progress.total.load(Relaxed), Relaxed);
        return Ok(Outcome::Completed);
    }
    // The data must be on disk before a state file claims it is: a power cut must not leave a
    // resume point ahead of the bytes actually stored.
    let saved = if info.ranges { persist(&job.target, &state, &ctx.slots.segments()).await } else { Ok(()) };
    match failure {
        Some(e) => Err(e), // the real cause wins over a secondary save error
        None => {
            saved?;
            if cancel.is_cancelled() { Ok(Outcome::Paused) } else { Err(EngineError::Truncated) }
        }
    }
}

/// A new connection on `slot`, counted as active from now on (the pacer reads the count).
fn spawn(workers: &mut JoinSet<Result<(), EngineError>>, ctx: &Arc<Ctx>, slot: Arc<Slot>) {
    let active = Active::enter(ctx.clone());
    workers.spawn(worker(ctx.clone(), slot, active));
}

async fn persist(target: &Path, state: &Path, segs: &[Segment]) -> std::io::Result<()> {
    OpenOptions::new().write(true).open(target).await?.sync_data().await?;
    save_state(state, segs).await
}

async fn worker(ctx: Arc<Ctx>, mut slot: Arc<Slot>, mut active: Active) -> Result<(), EngineError> {
    loop {
        if !fetch_with_retry(&ctx, &slot, &mut active).await? {
            return Ok(()); // this connection closed: its piece went back to the others
        }
        if ctx.stop.is_cancelled() || !ctx.ranges {
            return Ok(());
        }
        // Connection trouble lowered the limit: this one closes instead of taking more work.
        if ctx.progress.active.load(Acquire) > ctx.pace.limit() && active.try_leave() {
            return Ok(());
        }
        match ctx.slots.steal() {
            Some(next) => slot = next,
            None => return Ok(()),
        }
    }
}

/// `Ok(true)`: the piece is done (or the download stopped). `Ok(false)`: this connection failed
/// while others keep going; the piece was handed back to them.
async fn fetch_with_retry(ctx: &Ctx, slot: &Arc<Slot>, active: &mut Active) -> Result<bool, EngineError> {
    let (mut attempt, mut restarts) = (0u32, 0u32);
    loop {
        let before = slot.pos.load(Acquire);
        let err = match fetch(ctx, slot).await {
            Ok(()) => return Ok(true),
            // A pause racing a network error is still a pause, not a failure.
            Err(_) if ctx.stop.is_cancelled() => return Ok(true),
            Err(e) if e.is_permanent() => return Err(e),
            Err(e) => e,
        };
        if slot.pos.load(Acquire) > before {
            attempt = 0;
        }
        ctx.pace.trouble(ctx.progress.active.load(Acquire), err.is_throttled());
        if ctx.ranges && active.try_leave() {
            ctx.slots.release(slot.clone());
            return Ok(false);
        }
        // Last connection standing: rides out Wi-Fi drops, sleeping laptops and busy servers, and
        // gives up only when nothing has arrived for a long while.
        if ctx.pace.stalled() {
            return Err(err);
        }
        if !ctx.ranges {
            restarts += 1;
            if restarts > MAX_RESTARTS {
                return Err(err);
            }
            restart(ctx, slot).await?;
        }
        attempt += 1;
        let base: u64 = if err.is_throttled() { 2000 } else { 500 };
        let wait = Duration::from_millis(base << attempt.min(6)).min(MAX_BACKOFF);
        if pause(&ctx.stop, wait).await {
            return Ok(true);
        }
    }
}

/// No range support: the only way on after a cut is from the start.
async fn restart(ctx: &Ctx, slot: &Slot) -> std::io::Result<()> {
    slot.pos.store(0, Release);
    ctx.progress.downloaded.store(0, Relaxed);
    OpenOptions::new().write(true).open(&ctx.path).await?.set_len(0).await
}

/// Sleeps `d`; `true` if cancelled meanwhile.
async fn pause(stop: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        () = stop.cancelled() => true,
        () = tokio::time::sleep(d) => false,
    }
}

async fn fetch(ctx: &Ctx, slot: &Slot) -> Result<(), EngineError> {
    let pos = slot.pos.load(Acquire);
    let end = slot.end.load(Acquire);
    if pos > end {
        return Ok(());
    }

    let mut req = ctx.client.get(ctx.url.clone()).headers(ctx.headers.clone());
    if ctx.ranges {
        req = req.header(RANGE, format!("bytes={pos}-{end}"));
    }
    // Connecting can take seconds: a pause must not wait for it.
    let res = tokio::select! {
        biased;
        () = ctx.stop.cancelled() => return Ok(()),
        res = req.send() => res?.error_for_status()?,
    };
    // A 206 for another range (broken proxy/CDN) would silently corrupt the file.
    if ctx.ranges && (res.status() != StatusCode::PARTIAL_CONTENT || range_start(res.headers()) != Some(pos)) {
        return Err(EngineError::RangeIgnored);
    }

    let mut file = OpenOptions::new().write(true).open(&ctx.path).await?;
    file.seek(SeekFrom::Start(pos)).await?;
    let mut stream = res.bytes_stream();
    let mut buf = Vec::with_capacity(BUF);
    // Hot loop (thousands of chunks per second per connection): the stop token — shared by every
    // connection of the job — is subscribed to once, not re-registered under its lock per chunk.
    let stopped = ctx.stop.cancelled();
    tokio::pin!(stopped);

    let result = loop {
        let chunk = tokio::select! {
            biased;
            () = &mut stopped => break Ok(false),
            chunk = stream.next() => chunk,
        };
        let chunk = match chunk {
            Some(Ok(c)) => c,
            Some(Err(e)) => break Err(EngineError::from(e)),
            None => break Ok(true),
        };
        let written = slot.pos.load(Acquire) + buf.len() as u64;
        let room = slot.end.load(Acquire).saturating_add(1).saturating_sub(written);
        let n = min(chunk.len() as u64, room) as usize;
        buf.extend_from_slice(&chunk[..n]);
        ctx.progress.downloaded.fetch_add(n as u64, Relaxed);
        ctx.pace.progressed(); // something arrived: the connection is alive, however slow
        // Throttling sleeps can be long: they must not delay a pause/shutdown.
        if ctx.limit.get() > 0 {
            tokio::select! {
                biased;
                () = &mut stopped => {}
                () = ctx.limit.take(n) => {}
            }
        }
        if n < chunk.len() {
            break Ok(false);
        }
        if buf.len() >= BUF {
            flush(&mut file, &mut buf, slot).await?;
        }
    };
    if !buf.is_empty() {
        flush(&mut file, &mut buf, slot).await?;
    }
    file.flush().await?;

    let eof = result?;
    let written = slot.pos.load(Acquire);
    match (eof, ctx.ranges, ctx.size) {
        (false, ..) => Ok(()),
        (true, false, _) if written == 0 => Err(EngineError::Empty),
        // Unknown length: the end of the stream is the end of the file.
        (true, false, None) => {
            slot.end.store(written - 1, Release);
            Ok(())
        }
        // Known length: stopping short is a truncated file, never a success.
        (true, ..) if !slot.snapshot().is_done() => Err(EngineError::Truncated),
        (true, ..) => Ok(()),
    }
}

/// Writes the buffer, then advances the segment: `pos` only ever covers bytes the OS has, so a
/// checkpoint (sync + state) never claims data still in flight.
async fn flush(file: &mut File, buf: &mut Vec<u8>, slot: &Slot) -> std::io::Result<()> {
    if !buf.is_empty() {
        file.write_all(buf).await?;
        file.flush().await?; // tokio completes the write in the background otherwise
        slot.pos.fetch_add(buf.len() as u64, Release);
        buf.clear();
    }
    Ok(())
}

async fn preallocate(path: &Path, size: u64) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).await?;
    }
    let file = File::create(path).await?;
    sparse(&file);
    file.set_len(size).await
}

/// Windows: a sparse file. On a plain NTFS file, writing far past the data written so far first
/// makes the file system fill the gap with zeros — every segment but the first would wait for
/// gigabytes of zeros before its first byte lands, and the disk would write the file twice.
/// (Linux file systems create sparse files by themselves.) Best effort: FAT/exFAT refuse it.
fn sparse(file: &File) {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_SPARSE};
        let mut returned = 0u32;
        // SAFETY: a valid handle we own for the duration of the call; no input or output buffer.
        unsafe {
            DeviceIoControl(
                file.as_raw_handle() as _,
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            );
        }
    }
    #[cfg(not(windows))]
    let _ = file;
}

fn range_start(headers: &HeaderMap) -> Option<u64> {
    let v = headers.get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    v.strip_prefix("bytes ")?.split(['-', '/']).next()?.trim().parse().ok()
}

async fn load_state(state: &Path, target: &Path, size: u64) -> Option<Vec<Segment>> {
    fs::metadata(target).await.ok().filter(|m| m.len() == size)?;
    let segs: Vec<Segment> = serde_json::from_slice(&fs::read(state).await.ok()?).ok()?;
    covers_exactly(segs, size)
}

/// Rejects a tampered or stale state file: segments must tile `[0, size)` with no gap or overlap.
fn covers_exactly(mut segs: Vec<Segment>, size: u64) -> Option<Vec<Segment>> {
    segs.sort_unstable_by_key(|s| s.start);
    let mut next = 0u64;
    for s in &segs {
        if s.start != next || s.end < s.start || s.pos < s.start {
            return None;
        }
        next = s.end.checked_add(1)?;
    }
    (next == size).then_some(segs)
}

/// Atomic (temp + rename, data synced first): a crash mid-write never leaves a torn state file.
async fn save_state(state: &Path, segs: &[Segment]) -> std::io::Result<()> {
    let tmp = state.with_extension("rdm.tmp");
    let mut file = File::create(&tmp).await?;
    file.write_all(&serde_json::to_vec(segs)?).await?;
    file.sync_all().await?;
    drop(file);
    fs::rename(tmp, state).await
}

/// One live connection of a download, counted in `Progress::active` (shown, and read by the pacer).
struct Active {
    ctx: Arc<Ctx>,
    left: bool,
}

impl Active {
    fn enter(ctx: Arc<Ctx>) -> Self {
        ctx.progress.active.fetch_add(1, AcqRel);
        Self { ctx, left: false }
    }

    /// Leaves only if another connection stays active, so the last one never gives up.
    fn try_leave(&mut self) -> bool {
        self.left = self.ctx.progress.active.fetch_update(AcqRel, Acquire, |n| (n > 1).then(|| n - 1)).is_ok();
        self.left
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        if !self.left {
            self.ctx.progress.active.fetch_sub(1, AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_must_tile_the_file() {
        let ok = vec![Segment::new(50, 99), Segment::new(0, 49)];
        assert!(covers_exactly(ok, 100).is_some());
        assert!(covers_exactly(vec![Segment::new(0, 49), Segment::new(60, 99)], 100).is_none());
        assert!(covers_exactly(vec![Segment::new(0, 49)], 100).is_none());
        assert!(covers_exactly(vec![Segment::new(0, u64::MAX)], 100).is_none());
    }

    #[test]
    fn parses_content_range() {
        let mut h = HeaderMap::new();
        h.insert(reqwest::header::CONTENT_RANGE, "bytes 1024-2047/4096".parse().unwrap());
        assert_eq!(range_start(&h), Some(1024));
    }
}
