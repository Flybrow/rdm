use std::{
    cmp::min,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::*},
    },
    time::Duration,
};

use domain::{Segment, plan_segments};
use futures_util::StreamExt;
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, RANGE},
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, File, OpenOptions},
    task::JoinSet,
    time::{Instant, MissedTickBehavior, interval_at},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    EngineError, Job, Outcome, Progress, RateLimit,
    pace::{Growth, Pace, speed_cap},
    probe,
    slots::{MIN_SPLIT, Slot, Slots},
    state_path,
};

/// Write buffers of a download, all connections together: each connection gets its share, between
/// `MIN_BUF` and `MAX_BUF` — few connections write big blocks (fewer system calls, fewer seeks on a
/// hard disk), many connections stay within memory.
const BUF_BUDGET: usize = 48 << 20;
const MIN_BUF: usize = 256 << 10;
const MAX_BUF: usize = 2 << 20;
/// Data kept in a buffer is written after this long at most (slow connections).
const FLUSH_AFTER: Duration = Duration::from_secs(5);

/// Resume state written this often while downloading (data synced first).
const CHECKPOINT: Duration = Duration::from_secs(20);
/// How often the pacer may add connections (and each connection's speed is measured): during
/// slow start, connections double at each tick while they pay off — full speed within a second
/// or two on a fast line.
const RAMP_EVERY: Duration = Duration::from_millis(500);
/// How often the pieces are shown to the UI (its progress bar).
const SHOW_PIECES: Duration = Duration::from_millis(250);
/// Longest wait between two attempts of the last connection (network down, busy server).
const MAX_BACKOFF: Duration = Duration::from_secs(15);
/// A server without range support restarts the file from zero after a cut: at most this often.
const MAX_RESTARTS: u32 = 5;

struct Ctx {
    client: Client,
    url: Url,
    headers: HeaderMap,
    disk: Arc<Disk>,
    slots: Slots,
    ranges: bool,
    size: Option<u64>,
    stop: CancellationToken,
    progress: Arc<Progress>,
    limit: Arc<RateLimit>,
    own_limit: Arc<RateLimit>,
    pace: Pace,
    /// Connections receiving a body right now (what the server accepted, for the pacer).
    streaming: AtomicUsize,
    /// Write buffer per connection.
    buf: usize,
}

pub(crate) async fn run(
    client: &Client,
    job: &Job,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<Outcome, EngineError> {
    let state = state_path(&job.target);
    let pace = Pace::new(usize::from(job.connections), host_key(&job.url));
    // The server asked a recent download to come back later (`Retry-After`): not before.
    let wait = pace.hold();
    if !wait.is_zero() && pause(&cancel, wait).await {
        return Ok(Outcome::Paused);
    }
    // Resuming: the facts only (the start of the file may be here already). Otherwise the first
    // request brings the first bytes of the file too.
    let resuming = fs::try_exists(&state).await.unwrap_or(false);
    // The first request retries for a while: a pause must not wait for it.
    let (info, first) = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(Outcome::Paused),
        found = async {
            if resuming { probe(client, &job.url, &job.headers).await.map(|info| (info, None)) } else { probe::open(client, &job.url, &job.headers).await }
        } => found?,
    };
    // The server refused the browser-like identity but took another one: every request uses it.
    let adjusted;
    let job = match &info.agent {
        Some(agent) => {
            let mut headers = job.headers.clone();
            headers.insert(reqwest::header::USER_AGENT, agent.clone());
            adjusted = Job { headers, ..job.clone() };
            &adjusted
        }
        None => job,
    };
    if info.hls {
        drop(first); // the playlist is read by the HLS engine
        return Box::pin(crate::hls::run(client, job, progress, cancel)).await;
    }
    pace.cap(speed_cap(Job::effective_limit(&job.limit, &job.own_limit)));

    // An empty body is how expired/blocked media links (e.g. YouTube) answer: never report it as done.
    if info.size == Some(0) {
        return Err(EngineError::Empty);
    }
    let (segments, disk) = match info.size {
        Some(size) if info.ranges => match load_state(&state, &job.target, size, info.version.as_deref()).await {
            Some(saved) => (saved, Disk::open(&job.target).await?),
            None => {
                let disk = Disk::create(&job.target, size).await?;
                // A few big pieces: connections added later split the largest remaining one.
                let first = u8::try_from(pace.limit()).unwrap_or(u8::MAX);
                (plan_segments(size, first, MIN_SPLIT), disk)
            }
        },
        size => {
            let disk = Disk::create(&job.target, 0).await?;
            (vec![Segment::new(0, size.map_or(u64::MAX - 1, |s| s.saturating_sub(1)))], disk)
        }
    };

    let ctx = Arc::new(Ctx {
        client: client.clone(),
        url: job.url.clone(),
        headers: job.headers.clone(),
        disk,
        slots: Slots::new(segments),
        ranges: info.ranges,
        size: info.size,
        stop: cancel.child_token(),
        progress,
        limit: job.limit.clone(),
        own_limit: job.own_limit.clone(),
        pace,
        streaming: AtomicUsize::new(0),
        buf: (BUF_BUDGET / usize::from(job.connections.max(1))).clamp(MIN_BUF, MAX_BUF),
    });
    ctx.progress.total.store(info.size.unwrap_or(0), Relaxed);
    ctx.progress.downloaded.store(ctx.slots.downloaded(), Relaxed);

    let mut workers = JoinSet::new();
    // Resumed with more pieces than connections allowed: the rest wait for a free connection.
    let mut pending = ctx.slots.pending().into_iter();
    // The first request's answer carries the file from its first byte: for the piece starting there.
    let mut first = first;
    for slot in pending.by_ref().take(ctx.pace.limit()) {
        let from_start = slot.snapshot().start == 0 && slot.pos.load(Acquire) == 0;
        spawn(&mut workers, &ctx, slot, if from_start { first.take() } else { None });
    }
    drop(first);
    for slot in pending {
        ctx.slots.release(slot);
    }
    let mut failure = None;
    let mut checkpoint = interval_at(Instant::now() + CHECKPOINT, CHECKPOINT);
    let mut ramp = interval_at(Instant::now() + RAMP_EVERY, RAMP_EVERY);
    ramp.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut show = interval_at(Instant::now(), SHOW_PIECES);
    show.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let (mut growth, mut counted, mut ticked) = (Growth::default(), ctx.progress.downloaded.load(Relaxed), Instant::now());
    loop {
        tokio::select! {
            joined = workers.join_next() => {
                let Some(res) = joined else { break };
                if let Err(e) = res.map_err(|e| EngineError::Io(e.into())).and_then(|r| r) {
                    ctx.stop.cancel();
                    failure.get_or_insert(e);
                } else if ctx.ranges && !ctx.stop.is_cancelled() && ctx.slots.all_done() {
                    // Every byte is on disk: a connection still open only overshoots a piece
                    // taken over from it (a straggler) — no reason to wait for it.
                    ctx.stop.cancel();
                }
            }
            // A crash, a kill or a session closing mid-download loses at most this much.
            _ = checkpoint.tick(), if info.ranges => {
                let _ = persist(&job.target, &state, info.version.as_deref(), &compact(ctx.slots.segments())).await;
            }
            _ = show.tick(), if info.ranges => ctx.progress.set_pieces(compact(ctx.slots.segments())),
            // More connections while they help: waiting pieces first, then parts of the piece
            // expected to finish last; dead connections replaced.
            _ = ramp.tick(), if info.ranges && failure.is_none() && !ctx.stop.is_cancelled() => {
                ctx.slots.measure(ticked.elapsed());
                ticked = Instant::now();
                let now = ctx.progress.downloaded.load(Relaxed);
                let grow = growth.more(now.saturating_sub(counted), ctx.pace.slow_start());
                counted = now;
                let cap = speed_cap(Job::effective_limit(&ctx.limit, &ctx.own_limit));
                let limit = ctx.pace.ramp(cap, grow, ctx.streaming.load(Acquire));
                // The server said when to come back (`Retry-After`): no new request before.
                while ctx.pace.hold().is_zero() && ctx.progress.active.load(Acquire) < limit {
                    let Some(slot) = ctx.slots.steal(ctx.pace.spare_requests(), !ctx.pace.counts_requests()) else { break };
                    spawn(&mut workers, &ctx, slot, None);
                }
            }
        }
    }

    ctx.progress.set_pieces(Vec::new());
    let done = ctx.slots.all_done();
    if done && failure.is_none() {
        let _ = fs::remove_file(&state).await;
        ctx.progress.downloaded.store(ctx.progress.total.load(Relaxed), Relaxed);
        return Ok(Outcome::Completed);
    }
    // The data must be on disk before a state file claims it is: a power cut must not leave a
    // resume point ahead of the bytes actually stored.
    let saved = if info.ranges { persist(&job.target, &state, info.version.as_deref(), &compact(ctx.slots.segments())).await } else { Ok(()) };
    match failure {
        Some(e) => Err(e), // the real cause wins over a secondary save error
        None => {
            saved?;
            if cancel.is_cancelled() { Ok(Outcome::Paused) } else { Err(EngineError::Truncated) }
        }
    }
}

/// `host:port` of `url`: the server whose accepted connection count the pacer remembers.
fn host_key(url: &Url) -> Option<String> {
    Some(format!("{}:{}", url.host_str()?.to_ascii_lowercase(), url.port_or_known_default()?))
}

/// A new connection on `slot`, counted as active from now on (the pacer reads the count);
/// `first`: the download's first request, already answered (see `probe::open`).
fn spawn(workers: &mut JoinSet<Result<(), EngineError>>, ctx: &Arc<Ctx>, slot: Arc<Slot>, first: Option<Response>) {
    let active = Active::enter(ctx.clone());
    workers.spawn(worker(ctx.clone(), slot, active, first));
}

/// Syncs the data through a handle of its own (the connections keep writing through theirs),
/// then records how far each piece went.
async fn persist(target: &Path, state: &Path, version: Option<&str>, segs: &[Segment]) -> std::io::Result<()> {
    OpenOptions::new().write(true).open(target).await?.sync_data().await?;
    save_state(state, version, segs).await
}

async fn worker(ctx: Arc<Ctx>, mut slot: Arc<Slot>, mut active: Active, mut first: Option<Response>) -> Result<(), EngineError> {
    // This worker's last request was accepted: its next one follows it (see `Pace::refuse`).
    let mut followed = false;
    loop {
        if !fetch_with_retry(&ctx, &mut slot, &mut active, followed, &mut first).await? {
            return Ok(()); // this connection closed: its piece went back to the others
        }
        followed = true;
        if ctx.stop.is_cancelled() || !ctx.ranges {
            return Ok(());
        }
        // Connection trouble lowered the limit: this one closes instead of taking more work.
        if ctx.progress.active.load(Acquire) > ctx.pace.limit() && active.try_leave() {
            return Ok(());
        }
        match ctx.slots.steal(ctx.pace.spare_requests(), !ctx.pace.counts_requests()) {
            Some(next) => slot = next,
            None => return Ok(()),
        }
    }
}

/// `Ok(true)`: the piece is done (or the download stopped). `Ok(false)`: this connection failed
/// while others keep going; the piece was handed back to them.
async fn fetch_with_retry(
    ctx: &Ctx,
    slot: &mut Arc<Slot>,
    active: &mut Active,
    mut followed: bool,
    first: &mut Option<Response>,
) -> Result<bool, EngineError> {
    let (mut attempt, mut restarts, mut replaced) = (0u32, 0u32, false);
    loop {
        // A server counting requests: none before the time it said (`Retry-After`).
        let hold = ctx.pace.hold();
        if ctx.pace.counts_requests() && !hold.is_zero() && pause(&ctx.stop, hold).await {
            return Ok(true);
        }
        let before = slot.pos.load(Acquire);
        let result = fetch(ctx, slot, followed, first.take()).await;
        followed = false;
        let err = match result {
            Ok(()) => return Ok(true),
            // A pause racing a network error is still a pause, not a failure.
            Err(_) if ctx.stop.is_cancelled() => return Ok(true),
            Err(e) if e.is_permanent() => return Err(e),
            Err(e) => e,
        };
        if slot.pos.load(Acquire) > before {
            attempt = 0;
        }
        // A dead or far too slow connection was closed (see `Slots::measure`): a fresh one takes
        // over at once — the bad one is never reused. Twice in a row is treated like any failure.
        if matches!(err, EngineError::Stalled) && !std::mem::replace(&mut replaced, true) {
            continue;
        }
        replaced = false;
        match &err {
            // A refusal was counted by `fetch` (with the server's `Retry-After`); a dead connection
            // is one bad path, not a sign that the network takes fewer connections.
            e if e.is_throttled() => {}
            EngineError::Stalled => {}
            _ => ctx.pace.trouble(ctx.progress.active.load(Acquire)),
        }
        err.forget_address();
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
        // The server said when to come back (`Retry-After`): exactly then. Otherwise longer and
        // longer waits.
        let base: u64 = if err.is_throttled() { 2000 } else { 500 };
        let hold = ctx.pace.hold();
        let wait = if err.is_throttled() && !hold.is_zero() { hold } else { Duration::from_millis(base << attempt.min(6)).min(MAX_BACKOFF).max(hold) };
        if pause(&ctx.stop, wait).await {
            return Ok(true);
        }
        // Refused (or a server counting requests): the next request starts at the first missing
        // byte and runs on through the free pieces — as few requests as possible.
        if ctx.ranges && (ctx.pace.counts_requests() || (ctx.pace.spare_requests() && err.is_throttled())) {
            *slot = ctx.slots.restart_lowest(slot);
        }
    }
}

/// No range support: the only way on after a cut is from the start.
async fn restart(ctx: &Ctx, slot: &Slot) -> std::io::Result<()> {
    slot.pos.store(0, Release);
    ctx.progress.downloaded.store(0, Relaxed);
    ctx.disk.truncate().await
}

/// Sleeps `d`; `true` if cancelled meanwhile.
async fn pause(stop: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        () = stop.cancelled() => true,
        () = tokio::time::sleep(d) => false,
    }
}

async fn fetch(ctx: &Ctx, slot: &Slot, followed: bool, answered: Option<Response>) -> Result<(), EngineError> {
    let pos = slot.pos.load(Acquire);
    let end = slot.end.load(Acquire);
    if pos > end {
        return Ok(());
    }
    // Cancelled when this connection stops receiving while the others do (see `Slots::measure`).
    let kill = slot.attach();
    let asked_at = Instant::now();

    // A request that may run on into the next free piece, without asking again: whenever the
    // server counts requests, and for the first piece — should the server refuse the other
    // connections, this one carries on alone.
    let open = ctx.ranges && ctx.size.is_some() && (ctx.pace.spare_requests() || pos == 0);
    let asked = if open { ctx.size.map_or(end, |size| size - 1) } else { end };
    let res = match answered {
        // The download's first request (`bytes=0-`), answered already.
        Some(res) => res,
        None => {
            let mut req = ctx.client.get(ctx.url.clone()).headers(ctx.headers.clone());
            if ctx.ranges {
                req = req.header(RANGE, format!("bytes={pos}-{asked}"));
            }
            // Connecting can take seconds: a pause must not wait for it.
            let res = tokio::select! {
                biased;
                () = ctx.stop.cancelled() => return Ok(()),
                () = kill.cancelled() => return Err(EngineError::Stalled),
                res = req.send() => res?,
            };
            if matches!(res.status().as_u16(), 429 | 503) {
                ctx.pace.refuse(ctx.progress.active.load(Acquire), crate::retry_after(res.headers()), followed);
            }
            res.error_for_status()?
        }
    };
    // A 206 for another range (broken proxy/CDN), or from another version of the file (another
    // size: a mirror serving a newer release), would silently corrupt the file. A 200 is the whole
    // file: right only when the whole file was asked for (RFC 9110 lets a server answer that way).
    let whole_file = pos == 0 && ctx.size.is_some_and(|size| asked.checked_add(1) == Some(size));
    let fits = match res.status() {
        StatusCode::PARTIAL_CONTENT => crate::content_range(res.headers()).is_some_and(|(start, total)| {
            start == pos && total.is_none_or(|total| Some(total) == ctx.size)
        }),
        // Its length too: a server answering with another version of the file (another size) would
        // not fit.
        StatusCode::OK => whole_file && res.content_length() == ctx.size,
        _ => false,
    };
    if ctx.ranges && !fits {
        return Err(EngineError::RangeIgnored);
    }
    let _receiving = Counted::enter(&ctx.streaming);

    let mut stream = res.bytes_stream();
    // Grows with the connection's speed up to its share: slow connections (flushed every few
    // seconds) never hold the full size.
    let mut buf = Vec::with_capacity(MIN_BUF.min(ctx.buf));
    let mut flushed = Instant::now();
    // Hot loop (thousands of chunks per second per connection): the stop token — shared by every
    // connection of the job — is subscribed to once, not re-registered under its lock per chunk.
    let stopped = ctx.stop.cancelled();
    let killed = kill.cancelled();
    tokio::pin!(stopped, killed);

    let result = loop {
        let chunk = tokio::select! {
            biased;
            () = &mut stopped => break Ok(false),
            () = &mut killed => break Err(EngineError::Stalled),
            chunk = stream.next() => chunk,
        };
        let chunk = match chunk {
            Some(Ok(c)) => c,
            Some(Err(e)) => break Err(EngineError::from(e)),
            None => break Ok(true),
        };
        // What fits in this piece — for an open request, running on into the next free pieces.
        let (mut rest, mut taken, mut over) = (&chunk[..], 0, false);
        while !rest.is_empty() {
            let written = slot.pos.load(Acquire) + buf.len() as u64;
            let room = slot.end.load(Acquire).saturating_add(1).saturating_sub(written);
            if room == 0 {
                // A server counting requests: rather read again a piece already here than ask anew.
                match ctx.slots.run_on(slot, open, ctx.pace.counts_requests()) {
                    Some(again) => {
                        ctx.progress.downloaded.fetch_sub(again, Relaxed);
                        continue;
                    }
                    None => {
                        over = true;
                        break;
                    }
                }
            }
            let n = min(rest.len() as u64, room) as usize;
            if buf.len() + n > buf.capacity() {
                // Doubles towards this connection's share, never beyond it (plus this chunk).
                let target = (buf.capacity() * 2).clamp(MIN_BUF, ctx.buf).max(buf.len() + n);
                buf.reserve_exact(target - buf.len());
            }
            buf.extend_from_slice(&rest[..n]);
            slot.head.store(written + n as u64, Release);
            rest = &rest[n..];
            taken += n;
        }
        let n = taken;
        ctx.progress.downloaded.fetch_add(n as u64, Relaxed);
        ctx.pace.progressed(); // something arrived: the connection is alive, however slow
        // Throttling sleeps can be long: they must not delay a pause/shutdown. The global limit, then
        // this download's own.
        for limit in [&ctx.limit, &ctx.own_limit] {
            if limit.get() > 0 {
                tokio::select! {
                    biased;
                    () = &mut stopped => {}
                    () = limit.take(n) => {}
                }
            }
        }
        if over {
            break Ok(false);
        }
        // Full, or held for a while (a slow connection): to disk, where a checkpoint can count it.
        if buf.len() >= ctx.buf || flushed.elapsed() >= FLUSH_AFTER {
            buf = flush(&ctx.disk, buf, slot).await?;
            flushed = Instant::now();
        }
    };
    if !buf.is_empty() {
        flush(&ctx.disk, buf, slot).await?;
    }

    let eof = result?;
    let written = slot.pos.load(Acquire);
    // The piece is done: how fast this connection went (for telling stragglers apart).
    if slot.snapshot().is_done() {
        ctx.slots.finished(written.saturating_sub(pos), asked_at.elapsed());
    }
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

/// Writes the buffer where it belongs, then advances the segment: `pos` only ever covers bytes the
/// OS has, so a checkpoint (sync + state) never claims data still in flight. The buffer comes back
/// empty, for reuse.
async fn flush(disk: &Arc<Disk>, buf: Vec<u8>, slot: &Slot) -> std::io::Result<Vec<u8>> {
    let len = buf.len() as u64;
    let mut buf = disk.write_at(buf, slot.pos.load(Acquire)).await?;
    slot.pos.fetch_add(len, Release);
    buf.clear();
    Ok(buf)
}

/// The file being downloaded, opened once for every connection: each writes at its own offset
/// (no seek, no reopening). Opening and closing a file for each piece costs system calls — and on
/// Windows an antivirus scan of the whole file at each close after writing.
struct Disk(std::fs::File);

impl Disk {
    /// A new file of `size` bytes (sparse where the system allows it).
    async fn create(path: &Path, size: u64) -> std::io::Result<Arc<Self>> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).await?;
        }
        let file = File::create(path).await?.into_std().await;
        sparse(&file);
        let disk = Arc::new(Self(file));
        let d = disk.clone();
        tokio::task::spawn_blocking(move || d.0.set_len(size)).await.map_err(std::io::Error::other)??;
        Ok(disk)
    }

    /// The file of a download being resumed.
    async fn open(path: &Path) -> std::io::Result<Arc<Self>> {
        let file = OpenOptions::new().write(true).open(path).await?.into_std().await;
        Ok(Arc::new(Self(file)))
    }

    async fn write_at(self: &Arc<Self>, buf: Vec<u8>, offset: u64) -> std::io::Result<Vec<u8>> {
        let disk = self.clone();
        tokio::task::spawn_blocking(move || disk.write_all_at(&buf, offset).map(|()| buf)).await.map_err(std::io::Error::other)?
    }

    async fn truncate(self: &Arc<Self>) -> std::io::Result<()> {
        let disk = self.clone();
        tokio::task::spawn_blocking(move || disk.0.set_len(0)).await.map_err(std::io::Error::other)?
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.0.write_all_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let (mut buf, mut offset) = (buf, offset);
            while !buf.is_empty() {
                match self.0.seek_write(buf, offset) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => {
                        buf = &buf[n..];
                        offset += n as u64;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }
}

/// Windows: a sparse file. On a plain NTFS file, writing far past the data written so far first
/// makes the file system fill the gap with zeros — every segment but the first would wait for
/// gigabytes of zeros before its first byte lands, and the disk would write the file twice.
/// (Linux file systems create sparse files by themselves.) Best effort: FAT/exFAT refuse it.
fn sparse(file: &std::fs::File) {
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

/// The pieces for the resume state and the progress bar, in file order, finished neighbours joined:
/// a long download split many times keeps a short list (a smaller state file, less to draw).
fn compact(mut segs: Vec<Segment>) -> Vec<Segment> {
    segs.sort_unstable_by_key(|s| s.start);
    let mut out: Vec<Segment> = Vec::with_capacity(segs.len());
    for s in segs {
        match out.last_mut() {
            Some(last) if last.is_done() && s.is_done() && last.end.checked_add(1) == Some(s.start) => {
                last.end = s.end;
                last.pos = s.end.saturating_add(1);
            }
            _ => out.push(s),
        }
    }
    out
}

/// What a resume state file holds: the file's version on the server when the download began, and
/// how far each piece went.
#[derive(Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    version: Option<String>,
    segments: Vec<Segment>,
}

/// The pieces to resume, or `None` to start over: no usable state, or the file changed on the
/// server since (another size, another version) — its new bytes would not continue the old ones.
async fn load_state(state: &Path, target: &Path, size: u64, version: Option<&str>) -> Option<Vec<Segment>> {
    fs::metadata(target).await.ok().filter(|m| m.len() == size)?;
    let saved = parse_state(&fs::read(state).await.ok()?)?;
    let changed = matches!((saved.version.as_deref(), version), (Some(then), Some(now)) if then != now);
    if changed {
        return None;
    }
    covers_exactly(saved.segments, size)
}

/// A state file, of this version of RDM or an older one (the bare list of pieces).
fn parse_state(bytes: &[u8]) -> Option<Saved> {
    serde_json::from_slice::<Saved>(bytes)
        .or_else(|_| serde_json::from_slice::<Vec<Segment>>(bytes).map(|segments| Saved { version: None, segments }))
        .ok()
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

async fn save_state(state: &Path, version: Option<&str>, segs: &[Segment]) -> std::io::Result<()> {
    let saved = Saved { version: version.map(str::to_owned), segments: segs.to_vec() };
    crate::write_atomic(state, &serde_json::to_vec(&saved)?).await
}

/// Counts itself in an `AtomicUsize` while alive.
struct Counted<'a>(&'a AtomicUsize);

impl<'a> Counted<'a> {
    fn enter(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, AcqRel);
        Self(count)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, AcqRel);
    }
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
    fn finished_neighbours_are_joined() {
        let done = |start, end| Segment { start, pos: end + 1, end };
        let live = Segment { start: 20, pos: 25, end: 29 };
        // Out of order (pieces split later come last), one overshooting its end.
        let segs = vec![done(30, 39), Segment { pos: 22, ..done(10, 19) }, done(0, 9), live];
        let joined = compact(segs);
        assert_eq!(joined, [done(0, 19), live, done(30, 39)]);
        assert!(covers_exactly(joined, 40).is_some(), "still tiles the file");
    }

    #[tokio::test]
    async fn resumes_only_the_same_version_of_the_file() {
        let dir = std::env::temp_dir().join(format!("rdm-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (target, state) = (dir.join("f.bin"), dir.join("f.bin.rdm"));
        std::fs::write(&target, vec![0u8; 100]).unwrap();
        let segs = [Segment::new(0, 49), Segment::new(50, 99)];
        let v1 = Some("date:Wed, 21 Oct 2025 07:28:00 GMT");

        save_state(&state, v1, &segs).await.unwrap();
        assert!(load_state(&state, &target, 100, v1).await.is_some(), "same version: resumed");
        assert!(load_state(&state, &target, 100, None).await.is_some(), "the server no longer says: resumed");
        assert!(load_state(&state, &target, 100, Some("date:Thu, 22 Oct 2025 07:28:00 GMT")).await.is_none(), "changed: start over");
        assert!(load_state(&state, &target, 200, v1).await.is_none(), "another size: start over");
        // A state written by an older RDM (the bare list of pieces).
        std::fs::write(&state, serde_json::to_vec(&segs).unwrap()).unwrap();
        assert!(load_state(&state, &target, 100, v1).await.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn every_connection_writes_at_its_own_offset() {
        let dir = std::env::temp_dir().join(format!("rdm-disk-{}", std::process::id()));
        let path = dir.join("f.bin");
        let disk = Disk::create(&path, 8).await.unwrap();
        let (a, b) = tokio::join!(disk.write_at(b"5678".to_vec(), 4), disk.write_at(b"1234".to_vec(), 0));
        assert!(a.unwrap().capacity() >= 4 && b.is_ok(), "the buffers come back for reuse");
        drop(disk);
        assert_eq!(std::fs::read(&path).unwrap(), b"12345678");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_content_range() {
        let range = |v: &str| crate::content_range(&HeaderMap::from_iter([(reqwest::header::CONTENT_RANGE, v.parse().unwrap())]));
        assert_eq!(range("bytes 1024-2047/4096"), Some((1024, Some(4096))));
        assert_eq!(range("bytes 0-0/*"), Some((0, None)), "size unknown");
        assert_eq!(range("bytes */4096"), None, "no range at all");
        assert_eq!(range("items 0-1/2"), None);
        assert_eq!(crate::content_range(&HeaderMap::new()), None);
    }

    #[test]
    fn servers_are_told_apart_by_host_and_port() {
        let key = |u: &str| host_key(&u.parse().unwrap());
        assert_eq!(key("https://CDN.example/a"), Some("cdn.example:443".into()));
        assert_eq!(key("http://cdn.example:8080/a"), Some("cdn.example:8080".into()));
    }
}
