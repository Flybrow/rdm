use std::{
    cmp::min,
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::*},
    },
    time::Duration,
};

use domain::{Segment, plan_segments};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, header::{HeaderMap, RANGE}};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    EngineError, Job, Outcome, Progress, RateLimit, probe,
    slots::{MIN_SPLIT, Slot, Slots},
    state_path,
};

/// Per-connection write buffer: 64 connections × 256 KiB = 16 MiB at most, same throughput as 1 MiB.
const BUF: usize = 256 << 10;
const RETRIES: u32 = 8;
/// Resume state written this often while downloading (data synced first).
const CHECKPOINT: Duration = Duration::from_secs(20);
/// Back-off rounds (1…8 s each) the last connection accepts from a 429/503-ing server (~3 min).
const THROTTLE_ROUNDS: u32 = 30;

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
}

pub(crate) async fn run(
    client: &Client,
    job: &Job,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<Outcome, EngineError> {
    let info = probe(client, &job.url, &job.headers).await?;
    if info.hls {
        return crate::hls::run(client, job, progress, cancel).await;
    }
    let state = state_path(&job.target);

    // An empty body is how expired/blocked media links (e.g. YouTube) answer: never report it as done.
    if info.size == Some(0) {
        return Err(EngineError::Empty);
    }
    let segments = match info.size {
        Some(size) if info.ranges => match load_state(&state, &job.target, size).await {
            Some(saved) => saved,
            None => {
                preallocate(&job.target, size).await?;
                plan_segments(size, job.connections, MIN_SPLIT)
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
    });
    ctx.progress.total.store(info.size.unwrap_or(0), Relaxed);
    ctx.progress.downloaded.store(ctx.slots.downloaded(), Relaxed);

    let mut workers = JoinSet::new();
    for slot in ctx.slots.pending() {
        workers.spawn(worker(ctx.clone(), slot));
    }
    let mut failure = None;
    let mut checkpoint = tokio::time::interval_at(tokio::time::Instant::now() + CHECKPOINT, CHECKPOINT);
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

async fn persist(target: &Path, state: &Path, segs: &[Segment]) -> std::io::Result<()> {
    OpenOptions::new().write(true).open(target).await?.sync_data().await?;
    save_state(state, segs).await
}

async fn worker(ctx: Arc<Ctx>, mut slot: Arc<Slot>) -> Result<(), EngineError> {
    let mut active = Active::enter(&ctx.progress.active);
    let mut throttled = 0u32;
    loop {
        match fetch_with_retry(&ctx, &slot).await {
            Ok(()) => throttled = 0,
            // Server caps connections per client: hand the work back while others keep going.
            Err(e) if e.is_throttled() => {
                if active.try_leave() {
                    ctx.slots.release(slot);
                    return Ok(());
                }
                // Last connection standing: back off, but not forever.
                throttled += 1;
                if throttled > THROTTLE_ROUNDS {
                    return Err(e);
                }
                if pause(&ctx.stop, Duration::from_secs(u64::from(throttled.min(8)))).await {
                    return Ok(());
                }
                continue;
            }
            Err(e) => return Err(e),
        }
        if ctx.stop.is_cancelled() || !ctx.ranges {
            return Ok(());
        }
        match ctx.slots.steal() {
            Some(next) => slot = next,
            None => return Ok(()),
        }
    }
}

async fn fetch_with_retry(ctx: &Ctx, slot: &Slot) -> Result<(), EngineError> {
    let mut attempt = 0;
    loop {
        let before = slot.pos.load(Acquire);
        match fetch(ctx, slot).await {
            Ok(()) => return Ok(()),
            // A pause racing a network error is still a pause, not a failure.
            Err(_) if ctx.stop.is_cancelled() => return Ok(()),
            Err(e) if !ctx.ranges || e.is_throttled() || e.is_permanent() => return Err(e),
            Err(_) if slot.pos.load(Acquire) > before => attempt = 0,
            Err(e) if attempt >= RETRIES => return Err(e),
            Err(_) => attempt += 1,
        }
        if pause(&ctx.stop, Duration::from_millis(250 << attempt.min(5))).await {
            return Ok(());
        }
    }
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
    flush(&mut file, &mut buf, slot).await?;
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

/// Atomic (temp + rename): a crash mid-write never leaves a torn state file behind.
async fn save_state(state: &Path, segs: &[Segment]) -> std::io::Result<()> {
    let tmp = state.with_extension("rdm.tmp");
    fs::write(&tmp, serde_json::to_vec(segs)?).await?;
    fs::rename(tmp, state).await
}

struct Active<'a> {
    count: &'a AtomicUsize,
    left: bool,
}

impl<'a> Active<'a> {
    fn enter(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, AcqRel);
        Self { count, left: false }
    }

    /// Leaves only if another worker stays active, so the last one never gives up.
    fn try_leave(&mut self) -> bool {
        self.left = self.count.fetch_update(AcqRel, Acquire, |n| (n > 1).then(|| n - 1)).is_ok();
        self.left
    }
}

impl Drop for Active<'_> {
    fn drop(&mut self) {
        if !self.left {
            self.count.fetch_sub(1, AcqRel);
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
