//! End-to-end: a download that starts on the Internet cannot be led into the local network by a
//! name that resolves there (`public_only` clients); one on the local network stays usable.

use std::sync::Arc;

use engine::{CancellationToken, ClientOptions, EngineError, Job, Progress};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn names_resolving_into_the_local_network_are_refused() {
    // A server on this computer: reachable as `localhost`, a name.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = requests.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && socket.read(&mut byte).await.unwrap_or(0) == 1 {
                request.push(byte[0]);
            }
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").await;
        }
    });
    let url: url::Url = format!("http://localhost:{port}/file.bin").parse().unwrap();
    assert!(engine::net::reaches_lan(&url).await, "the address of `localhost` is local");

    let public_only = engine::client_with(&ClientOptions { public_only: true, ..ClientOptions::default() }).unwrap();
    let dir = std::env::temp_dir().join(format!("rdm-lan-{}", std::process::id()));
    let job = Job::new(url, dir.join("file.bin"));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine::run(&public_only, &job, Arc::new(Progress::default()), CancellationToken::new()),
    )
    .await
    .expect("refused at once, not retried");
    assert!(matches!(result, Err(EngineError::LocalNetwork)), "{result:?}");
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0, "nothing was sent to the local server");

    // The same download, started on the local network: fine.
    let lan = engine::client_with(&ClientOptions::default()).unwrap();
    let result = engine::run(&lan, &job, Arc::new(Progress::default()), CancellationToken::new()).await;
    assert!(result.is_ok(), "{result:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn internet_names_are_not_local() {
    assert!(engine::net::reaches_lan(&"http://192.168.1.20/share/a.iso".parse().unwrap()).await);
    assert!(engine::net::reaches_lan(&"http://printer.local/".parse().unwrap()).await);
    // A name that does not resolve: not known to be local (the download will fail on its own).
    assert!(!engine::net::reaches_lan(&"https://no-such-host.invalid/a.zip".parse().unwrap()).await);
}
