#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod bridge;
mod extension;
mod i18n;
mod local;
mod manager;
mod native;
mod notify;
mod priority;
mod secrets;
mod settings;
mod shell;
mod tray;
mod ui;
mod update;
mod virustotal;
mod window;
mod ytdlp;

use std::time::Duration;

use manager::{AddRequest, Manager};
use url::Url;

/// Background work still running once everything is saved (a SHA-256 of a huge file, a merge past
/// its grace period) must not keep the process alive after the user quit.
const RUNTIME_GRACE: Duration = Duration::from_millis(500);
/// `rdm --quit`: closes the running RDM (downloads saved), if any, and starts nothing.
const QUIT_FLAG: &str = "--quit";

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Started by a browser as the extension's connector: relays, and nothing else (see `native`).
    if native::is_host(&args) {
        native::run_host();
        return Ok(());
    }
    // Before anything can speak: messages (even the update assistant's) in the chosen language.
    let settings = settings::Settings::load();
    i18n::set(settings.language);
    // A copy of RDM started to install an update once RDM has quit (see `update`).
    if update::run_assistant(&args) {
        return Ok(());
    }
    update::remember_exe();
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

    // The installer closes a running RDM cleanly before replacing it.
    if args.iter().any(|a| a == QUIT_FLAG) {
        rt.block_on(bridge::quit_running());
        return Ok(());
    }
    let minimized = args.iter().any(|a| a == autostart::MINIMIZED_FLAG);
    let url_arg = args
        .iter()
        .filter_map(|a| a.parse::<Url>().ok())
        .find(|u| matches!(u.scheme(), "http" | "https"));

    // The bridge port doubles as a single-instance lock: a second launch hands over its URL, or
    // just brings the running window to the front.
    let Some(listener) = rt.block_on(single_instance(url_arg.as_ref())) else {
        return Ok(());
    };
    // The port is ours: the local programs get the token that lets them use it.
    local::issue_token();

    notify::register();
    // Copies of the extension installed by an older RDM get this version's files.
    std::thread::spawn(extension::refresh_installed);
    std::thread::spawn(update::clean_leftovers);
    std::thread::spawn(native::register);
    let manager = Manager::new(rt.handle().clone(), engine::client().expect("http client"), settings);
    if let Some(url) = url_arg {
        manager.add(AddRequest::from_url(url));
    }
    rt.spawn(bridge::serve(manager.clone(), listener));
    #[cfg(unix)]
    rt.spawn(quit_on_signal(manager.clone()));
    let result = ui::run(manager.clone(), minimized); // returns once the manager has shut down
    let restart = manager.restart_requested();
    drop(manager);
    rt.shutdown_timeout(RUNTIME_GRACE); // frees the bridge port the new RDM needs
    if restart {
        update::relaunch(); // Linux, after an update: the new version
    }
    result
}

/// The bridge's listener if this is the only RDM; `None` once the running one took over (`url`
/// handed to it, or its window shown). An RDM still closing (a restart, an update) frees the
/// port within seconds: it is waited for. Never ends silently: if the port stays taken by
/// something that does not answer as RDM, the user is told.
async fn single_instance(url: Option<&Url>) -> Option<tokio::net::TcpListener> {
    // Asked once at once, once more at the end: a hung process would make each try wait.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut asked = false;
    loop {
        if let Ok(listener) = bridge::bind().await {
            return Some(listener);
        }
        let last = tokio::time::Instant::now() >= deadline;
        if !asked || last {
            window::allow_foreground_handoff();
            if bridge::forward(url).await {
                return None;
            }
            asked = true;
        }
        if last {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let port = settings::BRIDGE_PORT;
    notify::fatal(&trf!(
        "RDM ne peut pas démarrer : le port local 127.0.0.1:{port} est occupé par un programme qui ne répond pas \
         (un RDM bloqué, ou celui d'une autre session ?). Fermez-le (Gestionnaire des tâches), puis relancez RDM.",
        "RDM cannot start: the local port 127.0.0.1:{port} is taken by a program that does not answer \
         (a stuck RDM, or another session's?). Close it (Task Manager), then start RDM again.",
        port = port
    ));
    None
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
