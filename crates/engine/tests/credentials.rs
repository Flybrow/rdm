//! End-to-end: the page's cookies and the site's login go with a stream's own requests, never to
//! another site a playlist names.

use std::sync::{Arc, Mutex};

use engine::{CancellationToken, HeaderValue, Job, Outcome, Progress, header};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// Serves `/master.m3u8` (same host) and two segments: `/own.ts` on the playlist's host (127.0.0.1)
/// and `/foreign.ts` named through another host (`localhost`, another site). Records each
/// request's path and whether it carried credentials.
async fn serve() -> (u16, Arc<Mutex<Vec<(String, bool)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    request.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
                let path = text.split_whitespace().nth(1).unwrap_or_default().to_owned();
                let credentials = text.contains("\r\ncookie:") || text.contains("\r\nauthorization:");
                log.lock().unwrap().push((path.clone(), credentials));
                let body: Vec<u8> = match path.as_str() {
                    "/master.m3u8" => format!(
                        "#EXTM3U\n#EXTINF:1,\nhttp://127.0.0.1:{port}/own.ts\n#EXTINF:1,\nhttp://localhost:{port}/foreign.ts\n#EXT-X-ENDLIST\n"
                    )
                    .into_bytes(),
                    _ => vec![0x47; 188],
                };
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    (port, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_playlist_cannot_send_the_credentials_to_another_site() {
    let (port, seen) = serve().await;
    let dir = std::env::temp_dir().join(format!("rdm-credentials-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut job = Job::new(format!("http://127.0.0.1:{port}/master.m3u8").parse().unwrap(), dir.join("stream.ts"));
    job.headers.insert(header::COOKIE, HeaderValue::from_static("session=secret"));
    job.headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic dXNlcjpwYXNz"));

    let outcome = engine::run(&engine::client().unwrap(), &job, Arc::new(Progress::default()), CancellationToken::new()).await;
    assert_eq!(outcome.unwrap(), Outcome::Completed);
    assert_eq!(std::fs::metadata(dir.join("stream.ts")).unwrap().len(), 2 * 188);
    let _ = std::fs::remove_dir_all(&dir);

    let seen = seen.lock().unwrap().clone();
    let with = |path: &str| seen.iter().filter(|(p, _)| p == path).map(|(_, c)| *c).collect::<Vec<_>>();
    assert!(with("/master.m3u8").iter().all(|c| *c), "the playlist's own site gets them: {seen:?}");
    assert!(with("/own.ts").iter().all(|c| *c), "{seen:?}");
    let foreign = with("/foreign.ts");
    assert!(!foreign.is_empty() && foreign.iter().all(|c| !c), "another site never gets them: {seen:?}");
}
