//! `cargo run --release -p engine --example get -- <url> <file> [connections] [times]`
//! (`times`: downloads in a row in this process — the second one knows the server).

use std::{sync::Arc, time::Instant};

use engine::{CancellationToken, Job, Progress};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(url), Some(target)) = (args.next(), args.next()) else {
        return Err("usage: get <url> <file> [connections]".into());
    };
    let connections = args.next().map_or(Ok(domain::DEFAULT_CONNECTIONS), |c| c.parse())?;
    let times: u32 = args.next().map_or(Ok(1), |t| t.parse())?;
    let job = Job { connections, ..Job::new(url.parse()?, target.into()) };
    let client = engine::client()?;
    let gap = std::env::var("RDM_GAP").ok().and_then(|g| g.parse().ok()).map(std::time::Duration::from_secs);
    for i in 0..times {
        if let Some(gap) = gap.filter(|_| i > 0) {
            tokio::time::sleep(gap).await;
        }
        let progress = Arc::new(Progress::default());
        let start = Instant::now();
        let outcome = engine::run(&client, &job, progress.clone(), CancellationToken::new()).await?;
        let (bytes, _, _) = progress.snapshot();
        let secs = start.elapsed().as_secs_f64();
        println!("{outcome:?}: {bytes} bytes in {secs:.2}s ({:.1} MB/s)", bytes as f64 / secs / 1e6);
    }
    Ok(())
}
