//! Brings the main window to the front from anywhere (tray, second launch, browser).
//!
//! Closing to the tray destroys the window (see `ui::run`): a hidden window never gets painted on
//! Windows, which left eframe's event loop polling at 100 % CPU, and Wayland cannot hide one at all.
//! What remains is a window that may be minimized or behind others.

use eframe::egui::{Context, ViewportCommand};

#[derive(Clone, Copy, Default)]
pub struct Window {
    #[cfg(windows)]
    hwnd: isize,
}

impl Window {
    pub fn of(cc: &eframe::CreationContext<'_>) -> Self {
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(w)) => w.hwnd.get(),
                _ => 0,
            };
            Self { hwnd }
        }
        #[cfg(not(windows))]
        {
            let _ = cc;
            Self::default()
        }
    }

    pub fn show(self, ctx: &Context) {
        #[cfg(windows)]
        if self.hwnd != 0 {
            let hwnd = self.hwnd as win::HWND;
            // SAFETY: our own top-level window, alive while attached to the shell; the async
            // variant only posts a message, and `IsIconic` / `SetForegroundWindow` only read or
            // request state: safe from any thread, never blocking.
            unsafe {
                win::ShowWindowAsync(hwnd, if win::IsIconic(hwnd) != 0 { win::SW_RESTORE } else { win::SW_SHOW });
                win::SetForegroundWindow(hwnd);
            }
            ctx.request_repaint();
            return;
        }
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        ctx.request_repaint();
    }
}

/// Whether a window whose top-left corner is at `x, y` (physical pixels) would be on a screen: the
/// monitor it was on may have been unplugged since (a laptop taken off its dock), and Windows does not
/// move a window created off-screen. Elsewhere the window manager constrains new windows itself.
pub fn on_screen(x: f32, y: f32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::POINT,
            Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromPoint},
        };
        // A point inside the title bar, where the user grabs the window.
        let grip = POINT { x: x as i32 + 48, y: y as i32 + 12 };
        // SAFETY: pure query on a plain value.
        !unsafe { MonitorFromPoint(grip, MONITOR_DEFAULTTONULL) }.is_null()
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        true
    }
}

/// A second instance hands the foreground to the running one (Windows blocks focus stealing).
pub fn allow_foreground_handoff() {
    #[cfg(windows)]
    // SAFETY: plain flag call with the documented `ASFW_ANY` constant.
    unsafe {
        win::AllowSetForegroundWindow(win::ASFW_ANY);
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn positions_beyond_every_monitor_are_off_screen() {
        assert!(!super::on_screen(-100_000.0, -100_000.0));
        assert!(!super::on_screen(10_000_000.0, 10_000_000.0));
    }
}

#[cfg(windows)]
mod win {
    pub use windows_sys::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            ASFW_ANY, AllowSetForegroundWindow, IsIconic, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindowAsync,
        },
    };
}
