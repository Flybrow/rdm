//! The download list: toolbar, one card per download (virtualized), empty states.

mod menu;
mod scan;
pub(super) mod text;

use domain::Status;
use eframe::egui::{
    Align, Align2, Color32, Id, Label, Layout, Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use egui_phosphor::regular as icon;

use super::{
    Action, App, category_icon, edit::Field, failure, queue_name,
    theme::{self, Palette},
    widgets::{self, ghost_button, icon_button, kbd},
};
use crate::{
    i18n::{bytes, duration, speed},
    manager::{Entry, Scan, Verify},
    settings::Queue,
    tr, trf,
    virustotal::Stage,
};
use menu::context_menu;
use scan::scan_badge;
use text::{detail_line, markers, status_style};

/// Fixed so the list can be virtualized: only the cards in view are laid out.
const CARD_HEIGHT: f32 = 86.0;
const CARD_GAP: f32 = 10.0;
/// The most pieces the bar draws one by one (see `widgets::pieces_progress`): one per connection
/// at most, finished neighbours drawn as one.
const MAX_SHOWN_PIECES: usize = domain::MAX_CONNECTIONS as usize;

impl App<'_> {
    pub(super) fn list(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let p = Palette::of(ui);
        let needle = self.memo.search.trim().to_lowercase();
        let filter = self.memo.filter;
        let queues = self.manager.with_settings(|s| s.queues.clone());
        let animating = self.manager.view(|entries| {
            let visible: Vec<&Entry> = entries
                .iter()
                .rev()
                .filter(|e| filter.accepts(e) && (needle.is_empty() || e.search_key.contains(&needle)))
                .collect();
            toolbar(ui, &p, &filter.title(&queues), visible.len(), !needle.is_empty(), actions);
            if visible.is_empty() {
                empty_state(ui, &p, entries.is_empty(), !needle.is_empty());
                return false;
            }
            let mut animating = false;
            ui.spacing_mut().item_spacing.y = CARD_GAP;
            ScrollArea::vertical().auto_shrink(false).show_rows(ui, CARD_HEIGHT, visible.len(), |ui, rows| {
                for e in &visible[rows] {
                    animating |= card(ui, &p, &queues, e, actions);
                }
                ui.add_space(8.0);
            });
            animating
        });
        self.animating |= animating;
    }
}

fn toolbar(ui: &mut Ui, p: &Palette, title: &str, count: usize, searching: bool, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(theme::semibold(18.0)).color(p.text));
        let label = if searching { crate::i18n::count(count as u64, ("résultat", "résultats"), ("result", "results")) } else { count.to_string() };
        let galley = ui.painter().layout_no_wrap(label, theme::semibold(11.5), p.muted);
        let (rect, _) = ui.allocate_exact_size(vec2(galley.size().x + 16.0, 22.0), Sense::hover());
        ui.painter().rect_filled(rect, 11, p.raised);
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.muted);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if ghost_button(ui, icon::BROOM, tr!("Effacer les terminés", "Clear completed")).clicked() {
                actions.push(Action::ClearCompleted);
            }
            if ghost_button(ui, icon::PAUSE, tr!("Tout suspendre", "Pause all")).clicked() {
                actions.push(Action::PauseAll);
            }
            if ghost_button(ui, icon::PLAY, tr!("Tout reprendre", "Resume all")).clicked() {
                actions.push(Action::ResumeAll);
            }
        });
    });
    ui.add_space(12.0);
}

/// One download. Returns whether it animates (a spinner): plain progress is redrawn at a lower
/// rate by the caller.
fn card(ui: &mut Ui, p: &Palette, queues: &[Queue], e: &Entry, actions: &mut Vec<Action>) -> bool {
    let d = &e.download;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), CARD_HEIGHT), Sense::click());
    if !ui.is_rect_visible(rect) {
        return false;
    }
    let (done, total, conns) = e.progress.snapshot();
    let status = d.status();
    let recording = d.is_recording();
    let running = *status == Status::Running;
    let (label, color) = status_style(p, e);

    // Frame: lifts on hover.
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.14);
    let painter = ui.painter();
    widgets::soft_shadow(painter, rect, 16, if p.dark { 0.25 + 0.75 * hover } else { 0.45 + 0.55 * hover }, p.dark);
    painter.rect(
        rect,
        16,
        p.surface.lerp_to_gamma(p.raised, hover * 0.8),
        Stroke::new(theme::HAIRLINE, p.border.lerp_to_gamma(p.accent.gamma_multiply(0.55), hover)),
        StrokeKind::Inside,
    );

    // Category tile.
    let hue = p.category(e.category);
    let tile = Rect::from_min_size(pos2(rect.left() + 16.0, rect.center().y - 24.0), Vec2::splat(48.0));
    painter.add(widgets::gradient(ui, tile, 14, p.tint(hue, 0.30), p.tint(hue, 0.10), vec2(0.7, 0.7)));
    painter.text(tile.center(), Align2::CENTER_CENTER, category_icon(e.category), theme::regular(23.0), hue);

    // Right: action buttons, then the percentage or the VirusTotal badge.
    let mut buttons: Vec<(&str, &str, Option<Color32>, Action)> = Vec::new();
    let (pause, resume) = (tr!("Suspendre", "Pause"), tr!("Reprendre", "Resume"));
    match status {
        Status::Running | Status::Failed(_) if recording => {}
        Status::Queued if e.retry.is_some() => {
            buttons.push((icon::ARROW_CLOCKWISE, tr!("Réessayer maintenant", "Retry now"), Some(p.accent), Action::Resume(d.id)));
            buttons.push((icon::PAUSE, pause, None, Action::Pause(d.id)));
        }
        Status::Running | Status::Queued => buttons.push((icon::PAUSE, pause, None, Action::Pause(d.id))),
        Status::Paused | Status::Failed(_) => buttons.push((icon::PLAY, resume, Some(p.accent), Action::Resume(d.id))),
        Status::Completed => {
            if e.scannable() && matches!(e.scan, Scan::None) {
                buttons.push((icon::SHIELD_CHECK, tr!("Analyser avec VirusTotal", "Scan with VirusTotal"), Some(p.success), Action::Scan(d.id)));
            }
            buttons.push((icon::FOLDER_OPEN, tr!("Afficher dans le dossier", "Show in folder"), None, Action::Reveal(d.target.clone())));
            buttons.push((icon::ARROW_SQUARE_OUT, tr!("Ouvrir le fichier", "Open the file"), None, Action::Open(d.target.clone())));
        }
    }
    buttons.push((icon::X, tr!("Retirer de la liste", "Remove from the list"), Some(p.danger), Action::Remove(d.id, false)));
    let mut x = rect.right() - 14.0;
    for (glyph, tip, tint, action) in buttons.into_iter().rev() {
        let slot = Rect::from_center_size(pos2(x - 16.0, rect.center().y), Vec2::splat(32.0));
        if widgets::place(ui, slot, |ui| icon_button(ui, glyph, tip, tint)).clicked() {
            actions.push(action);
        }
        x -= 36.0;
    }
    x -= 8.0;

    let fraction = (total > 0).then(|| (done.min(total) as f64 / total as f64) as f32);
    let mut moving = false;
    if *status == Status::Completed {
        let (badge, moves) = scan_badge(ui, p, e, x, rect.center().y, actions);
        moving |= moves;
        x = badge.left() - 12.0;
    } else if let Some(f) = fraction {
        let text = format!("{} %", (f * 100.0).floor() as u32);
        let r = ui.painter().text(pos2(x, rect.center().y), Align2::RIGHT_CENTER, text, theme::semibold(15.0), p.text);
        x = r.left() - 14.0;
    }

    // Middle: name (with its markers), status line, progress.
    let left = tile.right() + 16.0;
    let middle = Rect::from_min_max(pos2(left, rect.top()), pos2(x.max(left + 60.0), rect.bottom()));
    let name = Rect::from_min_size(pos2(middle.left(), rect.top() + 14.0), vec2(middle.width(), 22.0));
    widgets::place(ui, name, |ui| {
        for (glyph, tint, tip) in markers(p, queues, e) {
            ui.label(RichText::new(glyph).font(theme::regular(14.0)).color(tint)).on_hover_text(tip);
        }
        ui.add(Label::new(RichText::new(&e.name).font(theme::semibold(14.5)).color(p.text)).truncate().selectable(false))
    });

    let line_y = rect.top() + 46.0;
    let pill = widgets::paint_pill(ui, middle.left(), line_y, label, color);
    let (detail, detail_color) = detail_line(e, status, recording, done, total, conns, p);
    let detail_rect = Rect::from_min_max(pos2(pill.right() + 10.0, line_y - 10.0), pos2(middle.right(), line_y + 10.0));
    widgets::place(ui, detail_rect, |ui| {
        ui.add(Label::new(RichText::new(detail).font(theme::regular(12.5)).color(detail_color)).truncate().selectable(false))
    });

    // Same length on every card: below the buttons too, which sit higher.
    let bar = Rect::from_min_max(pos2(middle.left(), rect.bottom() - 17.0), pos2(rect.right() - 20.0, rect.bottom() - 12.0));
    let shown = fraction.map(|f| ui.ctx().animate_value_with_time(Id::new(("progress", d.id)), f, 0.35));
    let gradient = match status {
        Status::Completed => (p.success, p.success.lerp_to_gamma(p.accent, 0.35)),
        Status::Failed(_) => (p.danger, p.danger.lerp_to_gamma(p.warning, 0.3)),
        Status::Paused => (p.warning, p.warning.lerp_to_gamma(p.danger, 0.2)),
        Status::Queued => (p.faint, p.muted),
        Status::Running => (p.accent, p.accent2),
    };
    let bar_fraction = if *status == Status::Completed { Some(1.0) } else if running { shown } else { shown.or(Some(0.0)) };
    // Running over several connections: one piece of the bar per connection. Past a few dozen
    // pieces the bar would be only gaps: the plain bar then.
    let pieces = if running && total > 0 { e.progress.pieces() } else { Vec::new() };
    if (2..=MAX_SHOWN_PIECES).contains(&pieces.len()) {
        widgets::pieces_progress(ui, bar, &pieces, total, gradient);
    } else {
        widgets::progress(ui, bar, bar_fraction, gradient, running);
    }

    // Double click: a finished file opens; otherwise its own window (progress) comes back.
    if response.double_clicked() {
        actions.push(if *status == Status::Completed { Action::Open(d.target.clone()) } else { Action::ShowProgress(d.id) });
    }
    response.context_menu(|ui| context_menu(ui, p, queues, e, actions));
    moving || matches!(e.verify, Verify::Running)
}

fn empty_state(ui: &mut Ui, p: &Palette, nothing_at_all: bool, searching: bool) {
    ui.add_space((ui.available_height() * 0.12).max(16.0));
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(160.0, 130.0), Sense::hover());
        let center = rect.center();
        widgets::glow(ui.painter(), center, Vec2::splat(96.0), p.accent.gamma_multiply(if p.dark { 0.30 } else { 0.20 }));
        let disc = Rect::from_center_size(center, Vec2::splat(84.0));
        ui.painter().add(widgets::gradient(ui, disc, 42, p.accent, p.accent2, vec2(0.7, 0.7)));
        let glyph = if nothing_at_all { icon::TRAY_ARROW_DOWN } else if searching { icon::MAGNIFYING_GLASS } else { icon::FUNNEL };
        ui.painter().text(center, Align2::CENTER_CENTER, glyph, theme::regular(38.0), Color32::WHITE);
        ui.add_space(10.0);
        let (title, subtitle) = if nothing_at_all {
            (
                tr!("Aucun téléchargement", "No downloads"),
                tr!(
                    "Collez un lien en haut, ou laissez l'extension du navigateur les envoyer ici.",
                    "Paste a link above, or let the browser extension send them here."
                ),
            )
        } else if searching {
            (tr!("Aucun résultat", "No results"), tr!("Aucun fichier de cette vue ne correspond à la recherche.", "No file in this view matches the search."))
        } else {
            (tr!("Rien ici", "Nothing here"), tr!("Aucun téléchargement dans cette vue pour le moment.", "No download in this view for now."))
        };
        ui.label(RichText::new(title).font(theme::semibold(20.0)).color(p.text));
        ui.label(RichText::new(subtitle).font(theme::regular(13.5)).color(p.muted));
        if nothing_at_all {
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(330.0, 28.0), Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                kbd(ui, &widgets::shortcut("Ctrl"));
                ui.label(RichText::new("+").color(p.faint));
                kbd(ui, "V");
                ui.label(RichText::new(tr!("n'importe où colle et télécharge", "anywhere pastes and downloads")).font(theme::regular(13.0)).color(p.muted));
            });
        }
    });
}
