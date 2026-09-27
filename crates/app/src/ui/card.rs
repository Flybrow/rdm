//! The download list: toolbar, one card per download (virtualized), empty states.

use domain::Status;
use eframe::egui::{
    Align, Align2, Color32, Id, Label, Layout, Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use egui_phosphor::regular as icon;

use super::{
    Action, App, Filter, category_icon, scan_running,
    theme::{self, Palette},
    widgets::{self, bytes, duration, ghost_button, icon_button, kbd, speed},
};
use crate::{
    manager::{Entry, Scan},
    virustotal::Stage,
};

/// Fixed so the list can be virtualized: only the cards in view are laid out.
const CARD_HEIGHT: f32 = 86.0;
const CARD_GAP: f32 = 10.0;

impl App<'_> {
    pub(super) fn list(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let p = Palette::of(ui);
        let needle = self.memo.search.trim().to_lowercase();
        let filter = self.memo.filter;
        let animating = self.manager.view(|entries| {
            let visible: Vec<&Entry> = entries
                .iter()
                .rev()
                .filter(|e| filter.accepts(e) && (needle.is_empty() || e.search_key.contains(&needle)))
                .collect();
            toolbar(ui, &p, filter, visible.len(), !needle.is_empty(), actions);
            if visible.is_empty() {
                empty_state(ui, &p, entries.is_empty(), !needle.is_empty());
                return false;
            }
            let mut animating = false;
            ui.spacing_mut().item_spacing.y = CARD_GAP;
            ScrollArea::vertical().auto_shrink(false).show_rows(ui, CARD_HEIGHT, visible.len(), |ui, rows| {
                for e in &visible[rows] {
                    animating |= card(ui, &p, e, actions);
                }
                ui.add_space(8.0);
            });
            animating
        });
        self.animating |= animating;
    }
}

fn toolbar(ui: &mut Ui, p: &Palette, filter: Filter, count: usize, searching: bool, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(filter.title()).font(theme::semibold(18.0)).color(p.text));
        let label = if searching { format!("{count} résultat(s)") } else { count.to_string() };
        let galley = ui.painter().layout_no_wrap(label, theme::semibold(11.5), p.muted);
        let (rect, _) = ui.allocate_exact_size(vec2(galley.size().x + 16.0, 22.0), Sense::hover());
        ui.painter().rect_filled(rect, 11, p.raised);
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.muted);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if ghost_button(ui, icon::BROOM, "Effacer les terminés").clicked() {
                actions.push(Action::ClearCompleted);
            }
            if ghost_button(ui, icon::PAUSE, "Tout suspendre").clicked() {
                actions.push(Action::PauseAll);
            }
            if ghost_button(ui, icon::PLAY, "Tout reprendre").clicked() {
                actions.push(Action::ResumeAll);
            }
        });
    });
    ui.add_space(12.0);
}

/// How a status reads and which colour carries it.
fn status_style(p: &Palette, status: &Status, recording: bool) -> (&'static str, Color32) {
    match status {
        Status::Running if recording => ("ENREGISTREMENT", p.danger),
        Status::Running => ("EN COURS", p.accent),
        Status::Queued => ("EN FILE", p.muted),
        Status::Paused => ("EN PAUSE", p.warning),
        Status::Completed => ("TERMINÉ", p.success),
        Status::Failed(_) => ("ÉCHEC", p.danger),
    }
}

/// One download. Returns whether it moves (and needs the next frame).
fn card(ui: &mut Ui, p: &Palette, e: &Entry, actions: &mut Vec<Action>) -> bool {
    let d = &e.download;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), CARD_HEIGHT), Sense::click());
    if !ui.is_rect_visible(rect) {
        return false;
    }
    let (done, total, conns) = e.progress.snapshot();
    let status = d.status();
    let recording = d.is_recording();
    let running = *status == Status::Running;
    let (label, color) = status_style(p, status, recording);

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
    match status {
        Status::Running | Status::Failed(_) if recording => {}
        Status::Running | Status::Queued => buttons.push((icon::PAUSE, "Suspendre", None, Action::Pause(d.id))),
        Status::Paused | Status::Failed(_) => buttons.push((icon::PLAY, "Reprendre", Some(p.accent), Action::Resume(d.id))),
        Status::Completed => {
            if e.scannable() && matches!(e.scan, Scan::None) {
                buttons.push((icon::SHIELD_CHECK, "Analyser avec VirusTotal", Some(p.success), Action::Scan(d.id)));
            }
            buttons.push((icon::FOLDER_OPEN, "Afficher dans le dossier", None, Action::Reveal(d.target.clone())));
            buttons.push((icon::ARROW_SQUARE_OUT, "Ouvrir le fichier", None, Action::Open(d.target.clone())));
        }
    }
    buttons.push((icon::X, "Retirer de la liste", Some(p.danger), Action::Remove(d.id, false)));
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
    let mut moving = running;
    if *status == Status::Completed {
        let (badge, moves) = scan_badge(ui, p, e, x, rect.center().y, actions);
        moving |= moves;
        x = badge.left() - 12.0;
    } else if let Some(f) = fraction {
        let text = format!("{} %", (f * 100.0).floor() as u32);
        let r = ui.painter().text(pos2(x, rect.center().y), Align2::RIGHT_CENTER, text, theme::semibold(15.0), p.text);
        x = r.left() - 14.0;
    }

    // Middle: name, status line, progress.
    let left = tile.right() + 16.0;
    let middle = Rect::from_min_max(pos2(left, rect.top()), pos2(x.max(left + 60.0), rect.bottom()));
    let name = Rect::from_min_size(pos2(middle.left(), rect.top() + 14.0), vec2(middle.width(), 22.0));
    widgets::place(ui, name, |ui| {
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
    widgets::progress(ui, bar, bar_fraction, gradient, running);

    if response.double_clicked() && *status == Status::Completed {
        actions.push(Action::Open(d.target.clone()));
    }
    response.context_menu(|ui| context_menu(ui, p, e, actions));
    moving
}

fn detail_line(e: &Entry, status: &Status, recording: bool, done: u64, total: u64, conns: usize, p: &Palette) -> (String, Color32) {
    let size = if total > 0 { bytes(total) } else { "taille inconnue".into() };
    let eta = e.eta().map(|s| format!(" · reste {}", duration(s))).unwrap_or_default();
    let text = match status {
        Status::Running if recording => {
            let estimate = if total > 0 { format!(" / ~{}", bytes(total)) } else { String::new() };
            format!("{}{estimate} · {}{eta} · dans le navigateur", bytes(done), speed(e.speed))
        }
        Status::Running => format!("{} / {size} · {}{eta} · {conns} connexion(s)", bytes(done), speed(e.speed)),
        Status::Completed => format!("{} · {}", bytes(total.max(done)), e.download.target.parent().map_or(String::new(), |d| d.display().to_string())),
        Status::Paused => format!("{} / {size}", bytes(done)),
        Status::Queued => format!("{} / {size} · démarre dès qu'une place se libère", bytes(done)),
        Status::Failed(reason) if recording => return (format!("{reason} — relancez l'enregistrement depuis la page"), p.danger),
        Status::Failed(reason) => return (format!("{reason} — ▶ pour réessayer"), p.danger),
    };
    (text, p.muted)
}

/// VirusTotal state of a finished file, right-aligned at `right`. Returns its rect and whether it
/// animates.
fn scan_badge(ui: &mut Ui, p: &Palette, e: &Entry, right: f32, y: f32, actions: &mut Vec<Action>) -> (Rect, bool) {
    let (glyph, text, color, action, tip) = match &e.scan {
        Scan::None => return (Rect::from_min_max(pos2(right, y), pos2(right, y)), false),
        Scan::Done(r) if r.flagged() == 0 => (
            icon::SHIELD_CHECK,
            format!("{}/{}", r.flagged(), r.engines()),
            p.success,
            Some(Action::ShowReport(e.download.id)),
            "VirusTotal : aucun antivirus ne détecte de menace — voir l'analyse".to_owned(),
        ),
        Scan::Done(r) => (
            icon::SHIELD_WARNING,
            format!("{}/{}", r.flagged(), r.engines()),
            if r.malicious > 0 { p.danger } else { p.warning },
            Some(Action::ShowReport(e.download.id)),
            format!("VirusTotal : {} antivirus signalent ce fichier — voir l'analyse", r.flagged()),
        ),
        Scan::Running(stage) => ("", stage_text(*stage), p.accent, None, "Analyse VirusTotal en cours".to_owned()),
        Scan::Failed(reason) => (
            icon::SHIELD_SLASH,
            "Réessayer".to_owned(),
            p.danger,
            Some(Action::Scan(e.download.id)),
            format!("Analyse impossible : {reason}. Cliquez pour réessayer."),
        ),
    };
    let running = scan_running(e);
    let galley = ui.painter().layout_no_wrap(text, theme::semibold(12.0), color);
    let width = galley.size().x + 44.0;
    let rect = Rect::from_min_max(pos2(right - width, y - 14.0), pos2(right, y + 14.0));
    let response = ui.interact(rect, Id::new(("scan", e.download.id)), if action.is_some() { Sense::click() } else { Sense::hover() });
    let hover = ui.ctx().animate_bool_with_time(response.id, response.hovered(), 0.12);
    let painter = ui.painter();
    painter.rect(rect, 14, p.tint(color, 0.13 + 0.10 * hover), Stroke::new(theme::HAIRLINE, p.tint(color, 0.35)), StrokeKind::Inside);
    let icon_center = pos2(rect.left() + 17.0, y);
    if running {
        widgets::spinner(painter, icon_center, 6.5, color, ui.input(|i| i.time) as f32);
    } else {
        painter.text(icon_center, Align2::CENTER_CENTER, glyph, theme::regular(16.0), color);
    }
    painter.galley(pos2(rect.left() + 31.0, y - galley.size().y / 2.0), galley, color);
    if let Some(action) = action
        && response.on_hover_text(tip).clicked()
    {
        actions.push(action);
    }
    (rect, running)
}

fn stage_text(stage: Stage) -> String {
    match stage {
        Stage::Queued => "VirusTotal · en attente".into(),
        Stage::Hashing => "VirusTotal · empreinte…".into(),
        Stage::LookingUp => "VirusTotal · recherche…".into(),
        Stage::Uploading(f) => format!("VirusTotal · envoi {} %", (f * 100.0).floor() as u32),
        Stage::Analyzing => "VirusTotal · analyse…".into(),
    }
}

fn context_menu(ui: &mut Ui, p: &Palette, e: &Entry, actions: &mut Vec<Action>) {
    ui.set_min_width(250.0);
    let d = &e.download;
    let item = |ui: &mut Ui, glyph: &str, label: &str, color: Color32| {
        ui.add(eframe::egui::Button::new(RichText::new(format!("{glyph}   {label}")).font(theme::regular(13.5)).color(color)).frame(false))
            .clicked()
    };
    if *d.status() == Status::Completed {
        if item(ui, icon::ARROW_SQUARE_OUT, "Ouvrir", p.text) {
            actions.push(Action::Open(d.target.clone()));
        }
        let scan = match &e.scan {
            Scan::Done(_) => Some(("Voir l'analyse VirusTotal", Action::ShowReport(d.id))),
            Scan::None | Scan::Failed(_) if e.scannable() => Some(("Analyser avec VirusTotal", Action::Scan(d.id))),
            _ => None,
        };
        if let Some((label, action)) = scan
            && item(ui, icon::SHIELD_CHECK, label, p.text)
        {
            actions.push(action);
        }
        match &e.sha256 {
            Some(hash) => {
                if item(ui, icon::FINGERPRINT, &format!("Copier le SHA-256 ({}…)", hash.get(..10).unwrap_or(hash)), p.text) {
                    actions.push(Action::Copy(hash.clone(), "SHA-256"));
                }
            }
            None => {
                if item(ui, icon::FINGERPRINT, "Calculer le SHA-256", p.text) {
                    actions.push(Action::Hash(d.id));
                }
            }
        }
    }
    if item(ui, icon::FOLDER_OPEN, "Afficher dans le dossier", p.text) {
        actions.push(Action::Reveal(d.target.clone()));
    }
    let link = if d.is_recording() { "Copier le lien de la page" } else { "Copier le lien" };
    if item(ui, icon::LINK, link, p.text) {
        actions.push(Action::Copy(d.url.to_string(), "Lien"));
    }
    if item(ui, icon::COPY, "Copier le chemin", p.text) {
        actions.push(Action::Copy(d.target.display().to_string(), "Chemin"));
    }
    ui.separator();
    if item(ui, icon::X, "Retirer de la liste", p.text) {
        actions.push(Action::Remove(d.id, false));
    }
    if item(ui, icon::TRASH, "Supprimer le fichier", p.danger) {
        actions.push(Action::Remove(d.id, true));
    }
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
            ("Aucun téléchargement", "Collez un lien en haut, ou laissez l'extension du navigateur les envoyer ici.")
        } else if searching {
            ("Aucun résultat", "Aucun fichier de cette vue ne correspond à la recherche.")
        } else {
            ("Rien ici", "Aucun téléchargement dans cette vue pour le moment.")
        };
        ui.label(RichText::new(title).font(theme::semibold(20.0)).color(p.text));
        ui.label(RichText::new(subtitle).font(theme::regular(13.5)).color(p.muted));
        if nothing_at_all {
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(330.0, 28.0), Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                kbd(ui, "Ctrl");
                ui.label(RichText::new("+").color(p.faint));
                kbd(ui, "V");
                ui.label(RichText::new("n'importe où colle et télécharge").font(theme::regular(13.0)).color(p.muted));
            });
        }
    });
}
