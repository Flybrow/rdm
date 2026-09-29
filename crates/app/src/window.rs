//! Brings the main window to the front from anywhere (tray, second launch, browser).
//!
//! Closing to the tray destroys the window (see `ui::run`): a hidden window never gets painted on
//! Windows, which left eframe's event loop polling at 100 % CPU, and Wayland cannot hide one at all.
//! What remains is a window that may be minimized or behind others.

use eframe::egui::{Color32, Context, ViewportCommand};

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

    /// Windows 11 draws the title bar in RDM's colours (background, title, border) instead of its
    /// own grey; Windows 10 only takes the dark mode. Elsewhere the window manager decides.
    pub fn paint_title_bar(self, dark: bool, background: Color32, text: Color32, border: Color32) {
        #[cfg(windows)]
        if self.hwnd != 0 {
            let hwnd = self.hwnd as win::HWND;
            // COLORREF: 0x00BBGGRR.
            let colorref = |c: Color32| u32::from(c.r()) | u32::from(c.g()) << 8 | u32::from(c.b()) << 16;
            let set = |attribute: i32, value: u32| {
                // SAFETY: our own window; a 4-byte value, as each of these attributes expects. An
                // attribute this Windows does not know is refused, harmlessly.
                unsafe { win::DwmSetWindowAttribute(hwnd, attribute as u32, (&raw const value).cast(), 4) };
            };
            set(win::DWMWA_USE_IMMERSIVE_DARK_MODE, u32::from(dark));
            set(win::DWMWA_CAPTION_COLOR, colorref(background));
            set(win::DWMWA_TEXT_COLOR, colorref(text));
            set(win::DWMWA_BORDER_COLOR, colorref(border));
        }
        #[cfg(not(windows))]
        let _ = (self, dark, background, text, border);
    }

    pub fn show(self, ctx: &Context) {
        #[cfg(windows)]
        if self.hwnd != 0 {
            let hwnd = self.hwnd as win::HWND;
            // SAFETY: our own top-level window, alive while attached to the shell; the async
            // variants only post messages, and `IsIconic` / `SetForegroundWindow` only read or
            // request state: safe from any thread, never blocking.
            unsafe {
                win::ShowWindowAsync(hwnd, if win::IsIconic(hwnd) != 0 { win::SW_RESTORE } else { win::SW_SHOW });
                if win::SetForegroundWindow(hwnd) == 0 {
                    // Windows refuses the focus to a program in the background (the browser has
                    // it): the window still comes above the others, without taking the keyboard.
                    let flags = win::SWP_NOMOVE | win::SWP_NOSIZE | win::SWP_NOACTIVATE | win::SWP_ASYNCWINDOWPOS;
                    win::SetWindowPos(hwnd, win::HWND_TOPMOST, 0, 0, 0, 0, flags);
                    win::SetWindowPos(hwnd, win::HWND_NOTOPMOST, 0, 0, 0, 0, flags);
                }
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
        let grip = POINT { x: (x as i32).saturating_add(48), y: (y as i32).saturating_add(12) };
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
        Graphics::Dwm::{DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
        UI::WindowsAndMessaging::{
            ASFW_ANY, AllowSetForegroundWindow, HWND_NOTOPMOST, HWND_TOPMOST, IsIconic, SW_RESTORE, SW_SHOW, SWP_ASYNCWINDOWPOS,
            SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow, SetWindowPos, ShowWindowAsync,
        },
    };
}
