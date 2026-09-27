//! Notification-area icon: native on Windows, StatusNotifierItem (pure-Rust D-Bus) on Linux.

use std::time::{Duration, Instant};

use tray_icon::{
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Show,
    PauseAll,
    ResumeAll,
    Quit,
}

const ITEMS: [(&str, &str, Command); 4] = [
    ("show", "Ouvrir RDM", Command::Show),
    ("resume", "Tout reprendre", Command::ResumeAll),
    ("pause", "Tout suspendre", Command::PauseAll),
    ("quit", "Quitter", Command::Quit),
];

const IDLE_TIP: &str = "RDM";
/// On Linux every tooltip change is a blocking D-Bus round trip: at most one per interval.
const TIP_INTERVAL: Duration = Duration::from_secs(1);

pub struct Tray {
    icon: TrayIcon,
    tip: String,
    tip_at: Option<Instant>,
}

/// `None` when the desktop has no notification area (e.g. GNOME without the AppIndicator extension):
/// callers must then keep the window reachable instead of hiding it.
///
/// `on_command` runs on the tray's own thread on Linux: it must not block, nor touch the tray.
pub fn create(rgba: Vec<u8>, size: u32, on_command: impl Fn(Command) + Send + Sync + Clone + 'static) -> Option<Tray> {
    let menu = Menu::new();
    let items: Vec<MenuItem> = ITEMS.iter().map(|(id, label, _)| MenuItem::with_id(MenuId::new(id), label, true, None)).collect();
    menu.append_items(&[&items[0], &PredefinedMenuItem::separator(), &items[1], &items[2], &PredefinedMenuItem::separator(), &items[3]])
        .ok()?;

    let on_menu = on_command.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some((_, _, command)) = ITEMS.iter().find(|(id, ..)| event.id == *id) {
            on_menu(*command);
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let open = matches!(
            event,
            TrayIconEvent::Click { button: MouseButton::Left, button_state: tray_icon::MouseButtonState::Up, .. }
                | TrayIconEvent::DoubleClick { .. }
        );
        if open {
            on_command(Command::Show);
        }
    }));

    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(IDLE_TIP)
        .with_icon(Icon::from_rgba(rgba, size, size).ok()?)
        .build()
        .ok()?;
    Some(Tray { icon, tip: IDLE_TIP.to_owned(), tip_at: None })
}

impl Tray {
    /// `None` = idle: shown at once. Live transfer summaries are throttled.
    pub fn set_tooltip(&mut self, summary: Option<String>) {
        let idle = summary.is_none();
        let tip = summary.unwrap_or_else(|| IDLE_TIP.to_owned());
        if tip == self.tip || (!idle && self.tip_at.is_some_and(|t| t.elapsed() < TIP_INTERVAL)) {
            return;
        }
        let _ = self.icon.set_tooltip(Some(&tip));
        self.tip = tip;
        self.tip_at = Some(Instant::now());
    }

    /// Removes the icon when the app quits. Windows needs it explicitly (or a ghost icon stays until
    /// hovered). On Linux the icon goes with the process's D-Bus connection, while an orderly
    /// shutdown would wait on the tray's service thread: that must never hold up quitting.
    pub fn close(self) {
        if cfg!(windows) {
            drop(self);
        } else {
            std::mem::forget(self);
        }
    }
}
