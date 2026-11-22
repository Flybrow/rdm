//! Routes "show" / "quit" requests (tray, browser, second launch) and download windows to the
//! window or, while RDM sits in the tray without one (closing to the tray destroys the window), to
//! the invisible host of the download windows, or to the windowless event loop of `ui::run`, which
//! it wakes up.

use std::{
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
    time::Instant,
};

use eframe::{
    UserEvent,
    egui::{Context, ViewportCommand, ViewportId},
};
use winit::event_loop::EventLoopProxy;

use crate::window::Window;

/// What the windowless event loop is woken up for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Wake {
    /// A download window to open (the lowest priority: showing the main window serves it too).
    Popup,
    Show,
    Quit,
}

#[derive(Default)]
pub struct Shell {
    state: Mutex<State>,
    quitting: AtomicBool,
}

#[derive(Default)]
struct State {
    /// The open window, if any.
    window: Option<(Context, Window)>,
    /// Without it, the invisible root window of the download windows, if any.
    host: Option<Context>,
    /// A request that arrived while no window was open.
    pending: Option<Wake>,
    /// Wakes the windowless event loop up.
    waker: Option<EventLoopProxy<UserEvent>>,
}

impl Shell {
    pub fn set_waker(&self, waker: EventLoopProxy<UserEvent>) {
        self.lock().waker = Some(waker);
    }

    /// A window just opened: it answers every request from now on. It satisfies a pending "show";
    /// a pending "quit" closes it at once.
    pub fn attach(&self, ctx: &Context, window: Window) {
        let mut state = self.lock();
        state.window = Some((ctx.clone(), window));
        if state.pending.take() == Some(Wake::Quit) || self.quitting() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    pub fn detach(&self) {
        self.lock().window = None;
    }

    /// The download windows' invisible host just opened (no main window): like `attach`.
    pub fn attach_host(&self, ctx: &Context) {
        let mut state = self.lock();
        state.host = Some(ctx.clone());
        if state.pending == Some(Wake::Quit) || self.quitting() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    pub fn detach_host(&self) {
        self.lock().host = None;
    }

    pub fn show(&self) {
        let mut state = self.lock();
        match (state.window.clone(), state.host.clone()) {
            (Some((ctx, window)), _) => {
                drop(state);
                window.show(&ctx);
            }
            // The host closes, and the main window opens in its place (taking the download windows).
            (None, Some(host)) => {
                rank(&mut state, Wake::Show);
                drop(state);
                host.send_viewport_cmd(ViewportCommand::Close);
            }
            (None, None) => wake(&mut state, Wake::Show),
        }
    }

    /// A download window is waiting to open: the root window (main or host) opens it at its next
    /// frame; without one, the host is started.
    pub fn popup(&self) {
        let mut state = self.lock();
        match state.window.as_ref().map(|(ctx, _)| ctx.clone()).or_else(|| state.host.clone()) {
            Some(ctx) => {
                drop(state);
                ctx.request_repaint();
            }
            None => wake(&mut state, Wake::Popup),
        }
    }

    pub fn quit(&self) {
        self.quitting.store(true, SeqCst);
        let mut state = self.lock();
        match state.window.as_ref().map(|(ctx, _)| ctx.clone()).or_else(|| state.host.clone()) {
            Some(ctx) => {
                drop(state);
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            None => wake(&mut state, Wake::Quit),
        }
    }

    pub fn quitting(&self) -> bool {
        self.quitting.load(SeqCst)
    }

    pub fn repaint(&self) {
        let ctx = {
            let state = self.lock();
            state.window.as_ref().map(|(ctx, _)| ctx.clone()).or_else(|| state.host.clone())
        };
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    }

    /// The request that arrived while no window was open, served once.
    pub fn take_pending(&self) -> Option<Wake> {
        self.lock().pending.take()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Keeps the more important of the pending request and `wake`: "quit" wins over a "show" not yet
/// served, which wins over a download window (the main window opens those too).
fn rank(state: &mut State, wake: Wake) {
    if state.pending.is_none_or(|pending| pending < wake) {
        state.pending = Some(wake);
    }
}

fn wake(state: &mut State, wake: Wake) {
    rank(state, wake);
    if let Some(waker) = &state.waker {
        // Any event wakes the loop; with no window it is only a nudge.
        let nudge = UserEvent::RequestRepaint { viewport_id: ViewportId::ROOT, when: Instant::now(), cumulative_pass_nr: 0 };
        let _ = waker.send_event(nudge);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windowless_requests_wait_for_the_loop() {
        let shell = Shell::default();
        assert_eq!(shell.take_pending(), None);
        shell.show();
        assert_eq!(shell.take_pending(), Some(Wake::Show));
        assert_eq!(shell.take_pending(), None, "a request is served once");
        shell.quit();
        shell.show();
        assert_eq!(shell.take_pending(), Some(Wake::Quit), "quit is never overridden");
        assert!(shell.quitting());
    }

    #[test]
    fn download_windows_rank_below_show_and_quit() {
        let shell = Shell::default();
        shell.popup();
        assert_eq!(shell.take_pending(), Some(Wake::Popup));
        shell.show();
        shell.popup();
        assert_eq!(shell.take_pending(), Some(Wake::Show), "the main window opens the download windows too");
        shell.popup();
        shell.quit();
        shell.popup();
        assert_eq!(shell.take_pending(), Some(Wake::Quit));
    }

    #[test]
    fn showing_from_the_host_closes_it_for_the_main_window() {
        let shell = Shell::default();
        shell.attach_host(&Context::default());
        shell.popup();
        assert_eq!(shell.take_pending(), None, "the host opens download windows itself");
        shell.show();
        assert_eq!(shell.take_pending(), Some(Wake::Show), "the main window opens once the host has closed");
        shell.detach_host();
    }

    #[test]
    fn a_new_window_serves_a_pending_show() {
        let shell = Shell::default();
        shell.show();
        shell.attach(&Context::default(), Window::default());
        shell.detach();
        assert_eq!(shell.take_pending(), None, "no stale show once the window has opened");
    }

    #[test]
    fn requests_go_to_the_open_window() {
        let shell = Shell::default();
        shell.attach(&Context::default(), Window::default());
        shell.show();
        shell.quit();
        assert_eq!(shell.take_pending(), None, "the window took them, the windowless loop has nothing to do");
        assert!(shell.quitting());
    }
}
