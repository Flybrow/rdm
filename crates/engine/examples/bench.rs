//! Speed bench against a local server that behaves like real ones:
//! `cargo run --release -p engine --example bench [scenario…]`.
//!
//! Scenarios: `fast` (loopback, no limit: the engine's own cost), `per-conn` (servers that cap each
//! connection), `cap4` (at most 4 connections, then 429 + `Retry-After`), `straggler` (one
//! connection in five crawls: a bad path), `ua-block` (browser-like User-Agents cut off),
//! `req-limit` (a few requests per window, then 429 + `Retry-After`, like OVH's mirrors).

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use engine::{CancellationToken, Job, Progress};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const SIZE: usize = 96 << 20;

#[derive(Clone, Copy)]
struct Behaviour {
    /// Bytes per second per connection (0 = unlimited).
    per_conn: u64,
    /// Time before each answer (a distant server).
    latency: Duration,
    /// Beyond this many simultaneous connections: 429 (0 = no cap).
    cap: usize,
    /// Every n-th connection crawls at 1/64 of `per_conn` (0 = none).
    straggler_every: usize,
    /// Browser-like User-Agents get the connection closed.
    ua_block: bool,
    /// At most this many requests per 3-second window (0 = no limit).
    per_window: usize,
}

fn body() -> Arc<Vec<u8>> {
    Arc::new((0..SIZE).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect())
}

async fn serve(b: Behaviour, data: Arc<Vec<u8>>) -> (url::Url, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/file.bin", listener.local_addr().unwrap()).parse().unwrap();
    let (live, refused) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let accepted = Arc::new(AtomicUsize::new(0));
    let window = Arc::new(std::sync::Mutex::new((Instant::now(), 0usize)));
    let r = refused.clone();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let n = accepted.fetch_add(1, SeqCst) + 1;
            let (data, live, refused, window) = (data.clone(), live.clone(), r.clone(), window.clone());
            let slow = b.straggler_every > 0 && n.is_multiple_of(b.straggler_every);
            tokio::spawn(async move {
                let _ = handle(socket, b, &data, &live, &refused, &window, slow).await;
            });
        }
    });
    (url, refused)
}

async fn handle(
    mut socket: TcpStream,
    b: Behaviour,
    data: &[u8],
    live: &AtomicUsize,
    refused: &AtomicUsize,
    window: &std::sync::Mutex<(Instant, usize)>,
    slow: bool,
) -> std::io::Result<()> {
    socket.set_nodelay(true)?;
    let mut buf = Vec::with_capacity(4096);
    loop {
        buf.clear();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            if socket.read(&mut byte).await? == 0 {
                return Ok(());
            }
            buf.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&buf).to_ascii_lowercase();
        if b.ua_block && text.lines().any(|l| l.starts_with("user-agent: mozilla")) {
            return Ok(());
        }
        tokio::time::sleep(b.latency).await;
        let now = live.fetch_add(1, SeqCst) + 1;
        struct Leave<'a>(&'a AtomicUsize);
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, SeqCst);
            }
        }
        let _leave = Leave(live);
        let over_rate = b.per_window > 0 && {
            let mut w = window.lock().unwrap();
            if w.0.elapsed() > Duration::from_secs(3) {
                *w = (Instant::now(), 0);
            }
            w.1 += 1;
            w.1 > b.per_window
        };
        if (b.cap > 0 && now > b.cap) || over_rate {
            refused.fetch_add(1, SeqCst);
            socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 2\r\ncontent-length: 0\r\n\r\n").await?;
            continue;
        }
        let range = text.lines().find_map(|l| l.strip_prefix("range: bytes=")).map(|r| {
            let (a, z) = r.trim().split_once('-').unwrap();
            let a: usize = a.parse().unwrap();
            (a, z.parse::<usize>().map_or(data.len() - 1, |z| z.min(data.len() - 1)))
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
        let rate = if slow { b.per_conn / 64 } else { b.per_conn };
        let piece = if rate == 0 { 256 << 10 } else { (rate / 50).clamp(1024, 256 << 10) as usize };
        let started = Instant::now();
        let mut sent = 0u64;
        for chunk in data[start..=end].chunks(piece) {
            socket.write_all(chunk).await?;
            sent += chunk.len() as u64;
            if rate > 0 {
                let due = Duration::from_secs_f64(sent as f64 / rate as f64);
                if let Some(wait) = due.checked_sub(started.elapsed()) {
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

async fn run(name: &str, b: Behaviour, data: Arc<Vec<u8>>) {
    let (url, refused) = serve(b, data.clone()).await;
    let dir = std::env::temp_dir().join(format!("rdm-bench-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let target = dir.join("file.bin");
    let client = engine::client().unwrap();
    let job = Job::new(url, target.clone());
    let progress = Arc::new(Progress::default());
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(300), engine::run(&client, &job, progress, CancellationToken::new())).await;
    let secs = started.elapsed().as_secs_f64();
    let intact = std::fs::read(&target).is_ok_and(|got| got == *data);
    let verdict = match result {
        Ok(Ok(outcome)) => format!("{outcome:?}"),
        Ok(Err(e)) => format!("error: {e}"),
        Err(_) => "timed out".into(),
    };
    println!(
        "{name:<10} {verdict:<10} {secs:>6.2} s  {:>7.1} MB/s  intact: {intact}  429s: {}",
        SIZE as f64 / secs / 1e6,
        refused.load(SeqCst)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::main]
async fn main() {
    let data = body();
    let base = Behaviour { per_conn: 0, latency: Duration::ZERO, cap: 0, straggler_every: 0, ua_block: false, per_window: 0 };
    let per_conn = Behaviour { per_conn: 4 << 20, latency: Duration::from_millis(30), ..base };
    let scenarios = [
        ("fast", base),
        ("per-conn", per_conn),
        ("cap4", Behaviour { cap: 4, per_conn: 8 << 20, ..per_conn }),
        ("straggler", Behaviour { straggler_every: 5, ..per_conn }),
        ("ua-block", Behaviour { ua_block: true, ..per_conn }),
        ("req-limit", Behaviour { per_window: 3, per_conn: 32 << 20, ..per_conn }),
    ];
    let wanted: Vec<String> = std::env::args().skip(1).collect();
    for (name, b) in scenarios {
        if wanted.is_empty() || wanted.iter().any(|w| w == name) {
            run(name, b, data.clone()).await;
        }
    }
}
