//! Download windows, like IDM's: real windows of their own, apart from the main one, brought in
//! front of the others. A browser download first asks there (folder, name, options for this download
//! only); once accepted, the same window shows its progress, then "Open", "Open the location" and
//! "Close".
//!
//! They are egui viewports, children of whichever root window runs: the main window, or — while RDM
//! sits in the tray without one — an invisible host (see `ui::run`). Their state lives here, outside
//! any root: when the main window opens or closes, the windows are created again under the new root,
//! where they were.

mod ask;
mod track;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Align2, CentralPanel, Color32, Context, Frame, Key, Label, Layout, Margin, Pos2, RichText, Sense, Ui, Vec2, ViewportBuilder, ViewportCommand,
    ViewportId, WindowLevel, pos2, vec2,
};

use super::theme::{self, Palette};
use crate::{
    manager::{DownloadWindow, Manager},
    tr,
};

/// Download windows on screen at once; the next ones wait for one to close.
const MAX_OPEN: usize = 5;
/// Landscape: the height follows the content (about half the width).
const WIDTH: f32 = 720.0;
/// Before the first frame measured the content.
const FIRST_HEIGHT: f32 = 300.0;
/// How long a new window stays above every other one: enough to come in front of the browser
/// (Windows refuses the focus to a program in the background), then it behaves like any window.
const ON_TOP_FOR: Duration = Duration::from_millis(1500);
/// Each new window a little lower and to the right of the previous one.
const CASCADE: f32 = 28.0;
const MARGIN: i8 = 24;

/// The download windows, shared by the roots (the main window, the tray host) over time.
#[derive(Clone, Default)]
pub struct DownloadWindows(Arc<Mutex<Registry>>);

#[derive(Default)]
struct Registry {
    open: Vec<Arc<Mutex<Pane>>>,
    waiting: VecDeque<DownloadWindow>,
    /// Viewport ids are never reused: a closing window and a new one cannot be mixed up.
    last: u64,
}

impl DownloadWindows {
    /// Called at every frame of the root window: opens the windows the manager asks for, forgets
    /// the closed ones, and keeps the others alive (a child not shown during a frame of its root
    /// would be closed).
    pub fn show(&self, ctx: &Context, manager: &Arc<Manager>) {
        let mut registry = lock(&self.0);
        for window in manager.take_windows() {
            // A download whose window is open already: that window comes to the front.
            let open = registry.open.iter().map(|pane| lock(pane)).find(|pane| !pane.closed && pane.is(window)).map(|pane| pane.viewport);
            match open {
                Some(viewport) => {
                    ctx.send_viewport_cmd_to(viewport, ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd_to(viewport, ViewportCommand::Focus);
                }
                None if !registry.waiting.contains(&window) => registry.waiting.push_back(window),
                None => {}
            }
        }
        registry.open.retain(|pane| !lock(pane).closed);
        while registry.open.len() < MAX_OPEN
            && let Some(window) = registry.waiting.pop_front()
        {
            registry.last += 1;
            let viewport = ViewportId::from_hash_of(("download-window", registry.last));
            let position = cascade(ctx, registry.open.len());
            if let Some(pane) = Pane::new(window, viewport, position, manager) {
                registry.open.push(Arc::new(Mutex::new(pane)));
            }
        }
        for pane in &registry.open {
            let (viewport, builder) = {
                let pane = lock(pane);
                (pane.viewport, pane.builder.clone())
            };
            let (pane, manager) = (pane.clone(), manager.clone());
            ctx.show_viewport_deferred(viewport, builder, move |ui, _| lock(&pane).ui(ui, &manager));
        }
    }

    /// No window open nor waiting (the tray host can go).
    pub fn is_empty(&self) -> bool {
        let registry = lock(&self.0);
        registry.open.iter().all(|pane| lock(pane).closed) && registry.waiting.is_empty()
    }

    /// A new root window takes the download windows over: each is created again where it was,
    /// brought in front again.
    pub fn rehome(&self) {
        for pane in &lock(&self.0).open {
            let mut pane = lock(pane);
            if let Some(position) = pane.position {
                pane.builder = pane.builder.clone().with_position(position);
            }
            pane.builder = pane.builder.clone().with_inner_size(vec2(WIDTH, pane.height)).with_title(&pane.title);
            pane.opened = None;
            pane.lowered = false;
            pane.title_bar_dark = None;
        }
    }
}

/// Where the `n`-th open window goes: centred on the root's screen, cascading. `None` when the
/// screen is not known (Wayland: the compositor places windows itself).
fn cascade(ctx: &Context, n: usize) -> Option<Pos2> {
    let monitor = ctx.input(|i| i.viewport().monitor_size)?;
    let offset = CASCADE * n as f32;
    let x = ((monitor.x - WIDTH) / 2.0).max(0.0) + offset;
    let y = ((monitor.y - FIRST_HEIGHT) / 2.5).max(0.0) + offset;
    Some(pos2(x, y))
}

/// What one window shows.
enum View {
    Ask(Box<ask::Form>),
    Track(track::Track),
}

/// What a view asks its window to do after a frame.
pub(super) enum Next {
    Stay,
    Close,
    /// The download was accepted: its progress, in the same window.
    Track(domain::DownloadId),
}

struct Pane {
    viewport: ViewportId,
    view: View,
    builder: ViewportBuilder,
    closed: bool,
    /// Where the window was last seen (outer top-left corner, in points).
    position: Option<Pos2>,
    /// The content's height, last measured; the window follows it.
    height: f32,
    /// Its first frame (under the current root): it stays on top for a moment from then.
    opened: Option<Instant>,
    lowered: bool,
    title: String,
    /// The theme its title bar was last painted for (Windows draws it in RDM's colours).
    title_bar_dark: Option<bool>,
}

impl Pane {
    fn new(window: DownloadWindow, viewport: ViewportId, position: Option<Pos2>, manager: &Arc<Manager>) -> Option<Self> {
        let view = match window {
            DownloadWindow::Ask(ticket) => View::Ask(Box::new(ask::Form::new(ticket, manager)?)),
            DownloadWindow::Track(id) => View::Track(track::Track::new(id)),
        };
        let title = title(&view, manager);
        let mut builder = ViewportBuilder::default()
            .with_title(&title)
            .with_app_id("rdm")
            .with_icon(super::window_icon())
            .with_inner_size(vec2(WIDTH, FIRST_HEIGHT))
            .with_resizable(false)
            .with_maximize_button(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_active(true);
        if let Some(position) = position {
            builder = builder.with_position(position);
        }
        Some(Self { viewport, view, builder, closed: false, position, height: FIRST_HEIGHT, opened: None, lowered: false, title, title_bar_dark: None })
    }

    fn is(&self, window: DownloadWindow) -> bool {
        match (&self.view, window) {
            (View::Ask(form), DownloadWindow::Ask(ticket)) => form.ticket == ticket,
            (View::Track(track), DownloadWindow::Track(id)) => track.id == id,
            _ => false,
        }
    }

    fn ui(&mut self, ui: &mut Ui, manager: &Arc<Manager>) {
        if self.closed {
            return;
        }
        let ctx = ui.ctx().clone();
        self.come_forward(&ctx);
        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
            self.position = Some(rect.min);
        }
        let p = Palette::from_ctx(&ctx);
        if self.title_bar_dark != Some(p.dark) {
            self.title_bar_dark = Some(p.dark);
            crate::window::paint_title_bars(p.dark, p.bg, p.text, p.border);
        }
        let closing =ctx.input(|i| i.viewport().close_requested() || i.key_pressed(Key::Escape));
        let mut next = Next::Stay;
        let frame = Frame::new().fill(p.bg).inner_margin(Margin::same(MARGIN));
        let content = CentralPanel::default()
            .frame(frame)
            .show(ui, |ui| {
                ui.set_width(WIDTH - 2.0 * f32::from(MARGIN));
                ui.vertical(|ui| match &mut self.view {
                    View::Ask(form) => next = form.ui(ui, &p, manager, closing),
                    View::Track(track) => next = track.ui(ui, &p, manager, closing),
                })
                .response
                .rect
            })
            .inner;
        match next {
            Next::Stay => {}
            Next::Close => self.close(&ctx),
            Next::Track(id) => {
                self.view = View::Track(track::Track::new(id));
                ctx.request_repaint();
            }
        }
        let title = title(&self.view, manager);
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        self.fit(&ctx, content.height() + 2.0 * f32::from(MARGIN));
    }

    /// Its first moments: above every window, then an ordinary one (still in front, until the user
    /// clicks elsewhere).
    fn come_forward(&mut self, ctx: &Context) {
        let opened = *self.opened.get_or_insert_with(|| {
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            Instant::now()
        });
        if self.lowered {
            return;
        }
        match ON_TOP_FOR.checked_sub(opened.elapsed()) {
            Some(left) => ctx.request_repaint_after(left),
            None => {
                ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::Normal));
                self.lowered = true;
            }
        }
    }

    /// The window as tall as its content.
    fn fit(&mut self, ctx: &Context, height: f32) {
        let height = height.ceil();
        if (height - self.height).abs() >= 1.0 {
            self.height = height;
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(WIDTH, height)));
        }
    }

    /// Goes away at the root's next frame (it stops showing the viewport).
    fn close(&mut self, ctx: &Context) {
        self.closed = true;
        ctx.request_repaint_of(ViewportId::ROOT);
    }
}

fn title(view: &View, manager: &Manager) -> String {
    let name = match view {
        View::Ask(_) => None,
        View::Track(track) => manager.view(|es| es.iter().find(|e| e.download.id == track.id).map(|e| e.name.clone())),
    };
    match name {
        Some(name) => format!("{name} — RDM"),
        None => format!("{} — RDM", tr!("Nouveau téléchargement", "New download")),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The top of a window: a soft icon tile (the colour of the file's type), the file's name, one
/// line under it, and maybe a figure on the right (the percentage).
pub(super) struct Heading<'a> {
    pub glyph: &'a str,
    pub color: Color32,
    pub title: &'a str,
    pub subtitle: &'a str,
    /// Something is on its way (a spinner after the subtitle).
    pub busy: bool,
    pub trailing: Option<String>,
}

pub(super) fn header(ui: &mut Ui, p: &Palette, h: Heading<'_>) {
    ui.horizontal(|ui| {
        let (tile, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
        ui.painter().add(super::widgets::gradient(ui, tile, 13, p.tint(h.color, 0.30), p.tint(h.color, 0.10), vec2(0.7, 0.7)));
        ui.painter().text(tile.center(), Align2::CENTER_CENTER, h.glyph, theme::regular(22.0), h.color);
        ui.add_space(8.0);
        // Room left for the figure on the right.
        let trailing = h.trailing.map(|t| ui.painter().layout_no_wrap(t, theme::semibold(20.0), p.text));
        let room = ui.available_width() - trailing.as_ref().map_or(0.0, |g| g.size().x + 16.0);
        ui.allocate_ui_with_layout(vec2(room, 44.0), Layout::top_down(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.add_space(2.0);
            ui.add(Label::new(RichText::new(h.title).font(theme::semibold(16.0)).color(p.text)).truncate());
            ui.horizontal(|ui| {
                ui.add(Label::new(RichText::new(h.subtitle).font(theme::regular(12.5)).color(p.muted)).truncate());
                if h.busy {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                    super::widgets::spinner(ui.painter(), rect.center(), 5.0, p.accent, ui.input(|i| i.time) as f32);
                    ui.ctx().request_repaint_after(Duration::from_millis(33));
                }
            });
        });
        if let Some(galley) = trailing {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::hover());
            ui.painter().galley(rect.right_center() - vec2(galley.size().x, galley.size().y / 2.0), galley, p.text);
        }
    });
}

/// A quiet disclosure line ("More options", "Details"): `true` while unfolded.
pub(super) fn fold(ui: &mut Ui, p: &Palette, open: &mut bool, title: &str) -> bool {
    let arrow = if *open { egui_phosphor::regular::CARET_DOWN } else { egui_phosphor::regular::CARET_RIGHT };
    let text = RichText::new(format!("{arrow}  {title}")).font(theme::regular(13.0)).color(p.muted);
    let response = ui.add(Label::new(text).sense(Sense::click()));
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if response.clicked() {
        *open = !*open;
    }
    *open
}

/// A row of label and value, the labels in one column.
pub(super) fn row(ui: &mut Ui, p: &Palette, label: &str, value: impl Into<RichText>) {
    ui.horizontal(|ui| {
        label_cell(ui, p, label, LABEL_WIDTH);
        ui.add(Label::new(value.into().font(theme::regular(13.0))).truncate());
    });
}

/// Width of the labels' column.
pub(super) const LABEL_WIDTH: f32 = 150.0;

/// A grey label in a cell of fixed width: the values after it line up.
pub(super) fn label_cell(ui: &mut Ui, p: &Palette, label: &str, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
    ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, label, theme::regular(13.0), p.muted);
}

/// A quiet card around a group of rows.
pub(super) fn panel<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(p.raised)
        .corner_radius(12)
        .inner_margin(Margin::same(14))
        .stroke(egui::Stroke::new(theme::HAIRLINE, p.border))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            add(ui)
        })
        .inner
}

/// The window's buttons, right-aligned on one line (only as tall as they are: a layout of the
/// remaining height would centre them there, and the window would grow to fit).
pub(super) fn buttons<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 38.0), Layout::right_to_left(Align::Center), add).inner
}
