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
    let (c, p) = (concurrent.clone(), peak.clone());
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let (faults, data, served, c, p) = (faults.clone(), data.clone(), served.clone(), c.clone(), p.clone());
            tokio::spawn(async move {
                let now = c.fetch_add(1, SeqCst) + 1;
                p.fetch_max(now, SeqCst);
                let _ = handle(socket, &faults, &data, &served, now).await;
                c.fetch_sub(1, SeqCst);
            });
        }
    });
    Server { url, peak }
}

async fn handle(mut socket: TcpStream, faults: &Faults, data: &[u8], served: &AtomicUsize, now: usize) -> std::io::Result<()> {
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
        let range = text.lines().find_map(|l| l.strip_prefix("range: bytes=")).map(|r| {
            let (a, b) = r.trim().split_once('-').unwrap();
            let a: usize = a.parse().unwrap();
            let b = b.parse::<usize>().map_or(data.len() - 1, |b| b.min(data.len() - 1));
            (a, b)
        });
        let (start, end) = range.unwrap_or((0, data.len() - 1));
        let head = match range {
            Some(_) => format!(
                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {start}-{end}/{}\r\ncontent-length: {}\r\n\r\n",
                data.len(),
                end - start + 1
            ),
            None => format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", data.len()),
        };
        socket.write_all(head.as_bytes()).await?;
        let slice = &data[start..=end];
        let n = served.fetch_add(1, SeqCst) + 1;
        if faults.cut_every > 0 && n.is_multiple_of(faults.cut_every) && slice.len() > 2 {
            socket.write_all(&slice[..slice.len() / 2]).await?;
            return Ok(()); // cut mid-body
        }
        for chunk in slice.chunks(64 << 10) {
            if faults.down.load(SeqCst) {
                return Ok(());
            }
            socket.write_all(chunk).await?;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starts_with_few_connections() {
    let server = serve(Arc::new(Faults::default())).await;
    let (result, path) = download(server.url, target("start"), 64).await;
    assert_eq!(result.unwrap(), Outcome::Completed);
    assert_intact(&path);
    // The probe plus at most the initial connections, then one doubling per second at most.
    assert!(server.peak.load(SeqCst) <= 64 + 1, "peak {}", server.peak.load(SeqCst));
}
