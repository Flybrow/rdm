//! `cargo run --release -p engine --example hls -- <playlist-url>`: lists the qualities of an HLS stream.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args().nth(1).ok_or("usage: hls <playlist-url>")?.parse()?;
    let info = engine::hls_info(&engine::client()?, &url, &engine::HeaderMap::new()).await?;
    println!("container: {}", if info.fmp4 { "fMP4" } else { "MPEG-TS" });
    for v in &info.variants {
        let height = v.height.map_or("?".into(), |h| format!("{h}p"));
        let audio = if v.audio.is_some() { "separate audio" } else { "muxed" };
        println!("{height:>6} {:>9} kb/s  {audio}", v.bandwidth / 1000);
    }
    Ok(())
}
