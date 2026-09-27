//! `cargo run --release -p engine --example merge -- <video.mp4> <audio.m4a> <out.mp4>`

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [video, audio, out] = args.as_slice() else {
        return Err("usage: merge <video> <audio> <out>".into());
    };
    let start = std::time::Instant::now();
    engine::mux::merge(video.as_ref(), audio.as_ref(), out.as_ref())?;
    println!("merged in {:.0?}", start.elapsed());
    Ok(())
}
