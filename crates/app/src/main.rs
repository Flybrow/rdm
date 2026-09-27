#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod bridge;
mod extension;
mod manager;
mod notify;
mod priority;
mod settings;
mod shell;
mod tray;
mod ui;
mod update;
mod virustotal;
mod window;

use std::time::Duration;

use manager::{AddRequest, Manager};
use url::Url;

/// Background work still running once everything is saved (a SHA-256 of a huge file, a merge past
/// its grace period) must not keep the process alive after the user quit.
const RUNTIME_GRACE: Duration = Duration::from_millis(500);

fn main() -> eframe::Result {
    priority::full_speed_in_background();
    // Transfers are I/O-bound: a few workers drive 64 connections; fewer threads, less memory.
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(64)
        .on_thread_start(priority::transfer_thread)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let args: Vec<String> = std::env::args().skip(1).collect();
    let minimized = args.iter().any(|a| a == autostart::MINIMIZED_FLAG);
    let url_arg = args
        .iter()
        .filter_map(|a| a.parse::<Url>().ok())
        .find(|u| matches!(u.scheme(), "http" | "https"));

    // The bridge port doubles as a single-instance lock: a second launch hands over its URL, or
    // just brings the running window to the front.
    let Ok(listener) = rt.block_on(bridge::bind()) else {
        window::allow_foreground_handoff();
        rt.block_on(bridge::forward(url_arg));
        return Ok(());
    };

    notify::register();
    // Copies of the extension installed by an older RDM get this version's files.
    std::thread::spawn(extension::refresh_installed);
    let manager = Manager::new(rt.handle().clone(), engine::client().expect("http client"));
    if let Some(url) = url_arg {
        manager.add(AddRequest::from_url(url));
    }
    rt.spawn(bridge::serve(manager.clone(), listener));
    #[cfg(unix)]
    rt.spawn(quit_on_signal(manager.clone()));
    let result = ui::run(manager, minimized); // returns once the manager has shut down
    rt.shutdown_timeout(RUNTIME_GRACE);
    result
}

/// Closing the session (SIGTERM), Ctrl+C in a terminal or a closed terminal: quit as if from the
/// tray — transfers stop cleanly and keep their resume point — instead of dying on the spot.
#[cfg(unix)]
async fn quit_on_signal(manager: std::sync::Arc<Manager>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) =
        (signal(SignalKind::terminate()), signal(SignalKind::interrupt()), signal(SignalKind::hangup()))
    else {
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
        _ = hup.recv() => {}
    }
    manager.request_quit();
}
