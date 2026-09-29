//! End-to-end: the engine against a local server that misbehaves like real networks and servers do.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};

use engine::{CancellationToken, Job, Outcome, Progress};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const SIZE: usize = 12 << 20;

fn body() -> Arc<Vec<u8>> {
    Arc::new((0..SIZE).map(|i| (i.wrapping_mul(31) ^ (i >> 7)) as u8).collect())
}

#[derive(Default)]
struct Faults {
    /// Refuse with 503 beyond this many simultaneous connections (0 = no cap).
    max_concurrent: usize,
    /// Every n-th response is cut off half-way (0 = never).
    cut_every: usize,
    /// While set, every connection is dropped at once (network down).
    down: AtomicBool,
    /// A range covering the whole file gets `200 OK` and the whole file (RFC 9110 allows it).
    whole_as_200: bool,
    /// Browser-like User-Agents get the connection closed (some mirrors).
    ua_block: bool,
    /// Every connection sends at most this many bytes per second (0 = no limit)…
    per_conn: usize,
    /// …and every n-th connection crawls at 1/64 of it (0 = none): a bad path.
    straggler_every: usize,
    /// At most this many requests per second, then 429 with `Retry-After: 1` (0 = no limit).
    per_second: usize,
}

struct Server {
    url: url::Url,

    peak: Arc<AtomicUsize>,
}

async fn serve(faults: Arc<Faults>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/file.bin", listener.local_addr().unwrap()).parse().unwrap();
    let (concurrent, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let data = body();
    let served = Arc::new(AtomicUsize::new(0));
    let window = Arc::new(std::sync::Mutex::new((std::time::Instant::now(), 0usize)));
    let (c, p) = (concurrent.clone(), peak.clone());
    tokio::spawn(async move {
        let mut accepted = 0usize;
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            accepted += 1;
            let slow = faults.straggler_every > 0 && accepted.is_multiple_of(faults.straggler_every);
            let (faults, data, served, c, p, window) = (faults.clone(), data.clone(), served.clone(), c.clone(), p.clone(), window.clone());
            tokio::spawn(async move {
                let now = c.fetch_add(1, SeqCst) + 1;
                p.fetch_max(now, SeqCst);
                let _ = handle(socket, &faults, &data, &served, now, slow, &window).await;
                c.fetch_sub(1, SeqCst);
            });
        }
    });
    Server { url, peak }
}

async fn handle(
    mut socket: TcpStream,
    faults: &Faults,
    data: &[u8],
    served: &AtomicUsize,
    now: usize,
    slow: bool,
    window: &std::sync::Mutex<(std::time::Instant, usize)>,
) -> std::io::Result<()> {
    loop {
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if socket.read(&mut byte).await? == 0 {
                return Ok(());
            }
            request.push(byte[0]);
        }
        if faults.down.load(SeqCst) {
            return Ok(()); // connection dropped
        }
        if faults.max_concurrent > 0 && now > faults.max_concurrent {
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await?;
            return Ok(());
        }
        let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
        if faults.ua_block && text.lines().any(|l| l.starts_with("user-agent: mozilla")) {
            return Ok(()); // connection dropped without an answer
        }
        let over_rate = faults.per_second > 0 && {
            let mut w = window.lock().unwrap();
            if w.0.elapsed() > Duration::from_secs(1) {
                *w = (std::time::Instant::now(), 0);
            }
            w.1 += 1;
            w.1 > faults.per_second
        };
        if over_rate {
            socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 1\r\ncontent-length: 0\r\n\r\n").await?;
            continue;
        }
        let range = text.lines().find_map(|l| l.strip_prefix("range: bytes=")).map(|r| {
            let (a, b) = r.trim().split_once('-').unwrap();
            let a: usize = a.parse().unwrap();
            let b = b.parse::<usize>().map_or(data.len() - 1, |b| b.min(data.len() - 1));
            (a, b)
        });
        let (start, end) = range.unwrap_or((0, data.len() - 1));
        let whole = faults.whole_as_200 && start == 0 && end == data.len() - 1;
        let head = match range {
            Some(_) if !whole => format!(
                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {start}-{end}/{}\r\ncontent-length: {}\r\n\r\n",
                data.len(),
                end - start + 1
            ),
            _ => format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", data.len()),
        };
        socket.write_all(head.as_bytes()).await?;
        let slice = &data[start..=end];
        let n = served.fetch_add(1, SeqCst) + 1;
        if faults.cut_every > 0 && n.is_multiple_of(faults.cut_every) && slice.len() > 2 {
            socket.write_all(&slice[..slice.len() / 2]).await?;
            return Ok(()); // cut mid-body
        }
        let rate = if slow { faults.per_conn / 64 } else { faults.per_conn };
        let piece = if rate == 0 { 64 << 10 } else { (rate / 20).clamp(1024, 64 << 10) };
        let started = std::time::Instant::now();
        let mut sent = 0;
        for chunk in slice.chunks(piece) {
            if faults.down.load(SeqCst) {
                return Ok(());
            }
            socket.write_all(chunk).await?;
            sent += chunk.len();
            if rate > 0
                && let Some(wait) = Duration::from_secs_f64(sent as f64 / rate as f64).checked_sub(started.elapsed())
            {
                tokio::time::sleep(wait).await;
            }
        }
    }
}

fn target(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rdm-flaky-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("file.bin")
}

async fn download(url: url::Url, target: PathBuf, connections: u8) -> (Result<Outcome, engine::EngineError>, PathBuf) {
    let client = engine::client().unwrap();
    let mut job = Job::new(url, target.clone());
    job.connections = connections;
    let progress = Arc::new(Progress::default());
    let result = tokio::time::timeout(Duration::from_secs(120), engine::run(&client, &job, progress, CancellationToken::new()))
        .await
        .expect("download hung");
    (result, target)
}

fn assert_intact(path: &PathBuf) {
    let got = std::fs::read(path).unwrap();
    assert_eq!(got.len(), SIZE);
    assert!(got == *body(), "content differs");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn survives_connections_cut_mid_transfer() {
    let server = serve(Arc::new(Faults { cut_every: 3, ..Faults::default() })).await;
    let (result, path) = download(server.url, target("cut"), 16).await;
    assert_eq!(result.unwrap(), Outcome::Completed);
    assert_intact(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backs_off_when_the_server_caps_connections() {
    let server = serve(Arc::new(Faults { max_concurrent: 4, ..Faults::default() })).await;
    let (result, path) = download(server.url, target("cap"), 32).await;
    assert_eq!(result.unwrap(), Outcome::Completed);
    assert_intact(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rides_out_a_network_outage() {
    let faults = Arc::new(Faults::default());
    let server = serve(faults.clone()).await;
    let outage = {
        let faults = faults.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            faults.down.store(true, SeqCst);
            tokio::time::sleep(Duration::from_secs(4)).await;
            faults.down.store(false, SeqCst);
        })
    };
    let (result, path) = download(server.url, target("outage"), 8).await;
    outage.await.unwrap();
    assert_eq!(result.unwrap(), Outcome::Completed);
    assert_intact(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn respects_a_speed_limit_without_timeouts() {
    let server = serve(Arc::new(Faults::default())).await;
    let client = engine::client().unwrap();
    let path = target("limit");
    let mut job = Job::new(server.url, path.clone());
    job.connections = 32;
    job.limit.set(4 << 20); // 12 MiB at 4 MiB/s: about 3 s
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        engine::run(&client, &job, Arc::new(Progress::default()), CancellationToken::new()),
    )
    .await
    .expect("download hung");
    assert_eq!(result.unwrap(), Outcome::Completed);
    let took = started.elapsed().as_secs_f64();
    assert!((2.0..10.0).contains(&took), "took {took:.1} s");
    assert!(server.peak.load(SeqCst) <= 16 + 1, "at most one connection per 256 KiB/s: peak {}", server.peak.load(SeqCst));
    assert_intact(&path);
}

/// One connection asks for the whole file (`bytes=0-<last>`): a server may answer `200` with the
/// whole file instead of `206`, which is exactly what was asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepts_a_whole_file_answer_to_a_whole_file_range() {
    let server = serve(Arc::new(Faults { whole_as_200: true, ..Faults::default() })).await;
    let (result, path) = download(server.url, target("whole"), 1).await;
    assert_eq!(result.unwrap(), Outcome::Completed);
    assert_intact(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starts_with_few_connections() {
    let server = serve(Arc::new(Faults::default())).await;
    let (result, path) = download(server.url, target("start"), 64).await;
    assert_eq!(result.unwrap(), Outcome::Completed);
    assert_intact(&path);
    // The probe plus at most the initial connections, then one doubling per second at most.
    assert!(server.peak.load(SeqCst) <= 64 + 1, "peak {}", server.peak.load(SeqCst));
}

/// Some mirrors drop any browser-like User-Agent coming from a program: the download goes on as a
/// common download tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takes_another_identity_when_the_browser_one_is_refused() {
    let server = serve(Arc::new(Faults { ua_block: true, ..Faults::default() })).await;
    let (result, path) = download(server.url, target("agent"), 8).await;
    assert_eq!(result.unwrap(), Outcome::Completed);
    assert_intact(&path);
}

/// One connection in five crawls (a bad path): it is replaced instead of holding the end of the
/// download — about 2 s here, where waiting for it would take half a minute.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replaces_a_crawling_connection() {
    let server = serve(Arc::new(Faults { per_conn: 2 << 20, straggler_every: 5, ..Faults::default() })).await;
    let started = std::time::Instant::now();
    let (result, path) = download(server.url, target("straggler"), 16).await;
    assert_eq!(result.unwrap(), Outcome::Completed);
    let took = started.elapsed().as_secs_f64();
    assert!(took < 20.0, "took {took:.1} s");
    assert_intact(&path);
}

/// A server counting requests (a few per second, then 429 + `Retry-After`): the download
/// completes, with its pieces joined into as few requests as possible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copes_with_a_server_counting_requests() {
    let server = serve(Arc::new(Faults { per_second: 3, ..Faults::default() })).await;
    let (result, path) = download(server.url, target("requests"), 16).await;
    assert_eq!(result.unwrap(), Outcome::Completed);
    assert_intact(&path);
}
