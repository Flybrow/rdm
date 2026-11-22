//! One download over HTTP: its first request, then connections added while they help, the pieces
//! they share, the resume state written along the way.

mod connection;
mod disk;
mod resume;

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::*},
    },
    time::Duration,
};

use domain::{Segment, plan_segments};
use reqwest::{Client, Response, header::HeaderMap};
use tokio::{
    fs,
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
use connection::{Active, pause, worker};
use disk::Disk;
use resume::{compact, load_state, persist};

/// Write buffers of a download, all connections together: each connection gets its share, between
/// `MIN_BUF` and `MAX_BUF` — few connections write big blocks (fewer system calls, fewer seeks on a
/// hard disk), many connections stay within memory.
const BUF_BUDGET: usize = 48 << 20;
const MIN_BUF: usize = 256 << 10;
const MAX_BUF: usize = 2 << 20;

/// Resume state written this often while downloading (data synced first).
const CHECKPOINT: Duration = Duration::from_secs(20);
/// How often the pacer may add connections (and each connection's speed is measured): during
/// slow start, connections double at each tick while they pay off — full speed within a second
/// or two on a fast line.
const RAMP_EVERY: Duration = Duration::from_millis(500);
/// How often the pieces are shown to the UI (its progress bar).
const SHOW_PIECES: Duration = Duration::from_millis(250);

struct Ctx {
    client: Client,
    url: Url,
    headers: HeaderMap,
    disk: Arc<Disk>,
    slots: Slots,
    ranges: bool,
    size: Option<u64>,
    version: Option<String>,
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

pub(crate) async fn run(client: &Client, job: &Job, progress: Arc<Progress>, cancel: CancellationToken) -> Result<Outcome, EngineError> {
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
        version: info.version.clone(),
        stop: cancel.child_token(),
        progress,
        limit: job.limit.clone(),
        own_limit: job.own_limit.clone(),
        pace,
        streaming: AtomicUsize::new(0),
        buf: (BUF_BUDGET / usize::from(job.connections.max(1))).clamp(MIN_BUF, MAX_BUF),
    });
    ctx.progress.total.store(info.size.unwrap_or(0), Relaxed);
    ctx.progress.set_resumable(info.ranges);
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
                let _ = persist(&ctx.disk, &state, info.version.as_deref(), &compact(ctx.slots.segments())).await;
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
        // On disk before its resume point goes: a power cut must not leave a file "complete" with
        // holes in it, and nothing saying so.
        ctx.disk.sync().await?;
        let _ = fs::remove_file(&state).await;
        ctx.progress.downloaded.store(ctx.progress.total.load(Relaxed), Relaxed);
        return Ok(Outcome::Completed);
    }
    // The data must be on disk before a state file claims it is: a power cut must not leave a
    // resume point ahead of the bytes actually stored.
    let saved = if info.ranges { persist(&ctx.disk, &state, info.version.as_deref(), &compact(ctx.slots.segments())).await } else { Ok(()) };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_told_apart_by_host_and_port() {
        let key = |u: &str| host_key(&u.parse().unwrap());
        assert_eq!(key("https://CDN.example/a"), Some("cdn.example:443".into()));
        assert_eq!(key("http://cdn.example:8080/a"), Some("cdn.example:8080".into()));
    }
}
