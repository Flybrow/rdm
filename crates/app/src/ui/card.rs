//! The download list: toolbar, one card per download (virtualized), empty states.

use domain::Status;
use eframe::egui::{
    Align, Align2, Color32, Id, Label, Layout, Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use egui_phosphor::regular as icon;

use super::{
    Action, App, category_icon, edit::Field, queue_name, scan_running,
    theme::{self, Palette},
    widgets::{self, bytes, duration, ghost_button, icon_button, kbd, speed},
};
use crate::{
    manager::{Entry, Scan, Verify},
    settings::Settings,
    tr, trf,
    virustotal::Stage,
};

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
        let settings = self.manager.settings();
        let animating = self.manager.view(|entries| {
            let visible: Vec<&Entry> = entries
                .iter()
                .rev()
                .filter(|e| filter.accepts(e) && (needle.is_empty() || e.search_key.contains(&needle)))
                .collect();
            toolbar(ui, &p, &filter.title(&settings), visible.len(), !needle.is_empty(), actions);
            if visible.is_empty() {
                empty_state(ui, &p, entries.is_empty(), !needle.is_empty());
                return false;
            }
            let mut animating = false;
            ui.spacing_mut().item_spacing.y = CARD_GAP;
            ScrollArea::vertical().auto_shrink(false).show_rows(ui, CARD_HEIGHT, visible.len(), |ui, rows| {
                for e in &visible[rows] {
                    animating |= card(ui, &p, &settings, e, actions);
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

/// How a status reads and which colour carries it.
fn status_style(p: &Palette, e: &Entry) -> (&'static str, Color32) {
    match e.download.status() {
        Status::Running if e.download.is_recording() => (tr!("ENREGISTREMENT", "RECORDING"), p.danger),
        Status::Running => (tr!("EN COURS", "ACTIVE"), p.accent),
        Status::Queued if e.retry.is_some() => (tr!("NOUVEL ESSAI", "RETRYING"), p.warning),
        Status::Queued if e.resolving => (tr!("PRÉPARATION", "PREPARING"), p.accent),
        Status::Queued => (tr!("EN FILE", "QUEUED"), p.muted),
        Status::Paused => (tr!("EN PAUSE", "PAUSED"), p.warning),
        Status::Completed => (tr!("TERMINÉ", "DONE"), p.success),
        Status::Failed(_) => (tr!("ÉCHEC", "FAILED"), p.danger),
    }
}

/// One download. Returns whether it animates (a spinner): plain progress is redrawn at a lower
/// rate by the caller.
fn card(ui: &mut Ui, p: &Palette, settings: &Settings, e: &Entry, actions: &mut Vec<Action>) -> bool {
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
        for (glyph, tint, tip) in markers(p, settings, e) {
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
    let pieces = if running && total > 0 { widgets::merge_finished(e.progress.pieces()) } else { Vec::new() };
    if (2..=MAX_SHOWN_PIECES).contains(&pieces.len()) {
        widgets::pieces_progress(ui, bar, &pieces, total, gradient);
    } else {
        widgets::progress(ui, bar, bar_fraction, gradient, running);
    }

    if response.double_clicked() && *status == Status::Completed {
        actions.push(Action::Open(d.target.clone()));
    }
    response.context_menu(|ui| context_menu(ui, p, settings, e, actions));
    moving || matches!(e.verify, Verify::Running)
}

/// Small icons before the name: its queue, its own speed limit, an accepted certificate, the
/// proxy route.
fn markers(p: &Palette, settings: &Settings, e: &Entry) -> Vec<(&'static str, Color32, String)> {
    let d = &e.download;
    let mut marks = Vec::new();
    if d.queue != 0 && settings.queues.iter().any(|q| q.id == d.queue) {
        marks.push((icon::QUEUE, p.accent, trf!("File : {}", "Queue: {}", queue_name(settings, d.queue))));
    }
    if d.speed_limit_kib > 0 {
        let limit = speed(f64::from(d.speed_limit_kib) * 1024.0);
        marks.push((icon::GAUGE, p.warning, trf!("Limité à {limit}", "Limited to {limit}", limit = limit)));
    }
    if d.insecure {
        marks.push((
            icon::SHIELD_WARNING,
            p.danger,
            tr!("Certificat non valide accepté pour ce téléchargement", "Invalid certificate accepted for this download").to_owned(),
        ));
    }
    if e.via_proxy {
        marks.push((icon::SHUFFLE, p.muted, tr!("Passe par le proxy (connexion directe trop lente)", "Goes through the proxy (direct connection too slow)").to_owned()));
    }
    marks
}

fn detail_line(e: &Entry, status: &Status, recording: bool, done: u64, total: u64, conns: usize, p: &Palette) -> (String, Color32) {
    let size = if total > 0 { bytes(total) } else { tr!("taille inconnue", "unknown size").into() };
    let eta = e.eta().map(|s| trf!(" · reste {}", " · {} left", duration(s))).unwrap_or_default();
    let (got, rate) = (bytes(done), speed(e.speed));
    let text = match status {
        Status::Running if recording => {
            let estimate = if total > 0 { format!(" / ~{}", bytes(total)) } else { String::new() };
            trf!("{got}{estimate} · {rate}{eta} · dans le navigateur", "{got}{estimate} · {rate}{eta} · in the browser", estimate = estimate, eta = eta, got = got, rate = rate)
        }
        Status::Running => {
            let conns = crate::i18n::count(conns as u64, ("connexion", "connexions"), ("connection", "connections"));
            format!("{got} / {size} · {rate}{eta} · {conns}")
        }
        Status::Completed => {
            let place = e.download.target.parent().map_or(String::new(), |d| d.display().to_string());
            let size = bytes(total.max(done));
            match &e.verify {
                Verify::Ok => return (trf!("{size} · ✓ empreinte vérifiée · {place}", "{size} · ✓ checksum verified · {place}", place = place, size = size), p.success),
                Verify::Mismatch(_) => {
                    return (trf!("✗ empreinte différente : fichier corrompu ou pas le bon · {place}", "✗ checksum mismatch: corrupted or not the expected file · {place}", place = place), p.danger);
                }
                Verify::Running => trf!("{size} · vérification de l'empreinte…", "{size} · verifying the checksum…", size = size),
                Verify::Failed(reason) => return (trf!("{size} · empreinte non vérifiée : {reason}", "{size} · checksum not verified: {reason}", reason = reason, size = size), p.warning),
                Verify::None => format!("{size} · {place}"),
            }
        }
        Status::Paused => format!("{got} / {size}"),
        Status::Queued if e.retry.is_some() => {
            let retry = e.retry.as_ref().expect("checked");
            let left = retry.at.saturating_duration_since(std::time::Instant::now()).as_secs();
            let reason = &retry.reason;
            let when = if left == 0 { tr!("maintenant", "now").to_owned() } else { trf!("dans {}", "in {}", duration(left)) };
            return (trf!("{reason} · nouvel essai {when}", "{reason} · retrying {when}", reason = reason, when = when), p.warning);
        }
        Status::Queued if e.resolving => tr!("analyse du lien auprès du serveur…", "asking the server about the link…").to_owned(),
        Status::Queued => trf!("{got} / {size} · démarre dès qu'une place se libère", "{got} / {size} · starts as soon as a place is free", got = got, size = size),
        Status::Failed(reason) if recording => {
            return (trf!("{reason} — relancez l'enregistrement depuis la page", "{reason} — start the recording again from the page", reason = reason), p.danger);
        }
        Status::Failed(reason) => return (trf!("{reason} — ▶ pour réessayer", "{reason} — ▶ to retry", reason = reason), p.danger),
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
            tr!("VirusTotal : aucun antivirus ne détecte de menace — voir l'analyse", "VirusTotal: no antivirus detects a threat — see the report").to_owned(),
        ),
        Scan::Done(r) => {
            let n = r.flagged();
            (
                icon::SHIELD_WARNING,
                format!("{n}/{}", r.engines()),
                if r.malicious > 0 { p.danger } else { p.warning },
                Some(Action::ShowReport(e.download.id)),
                trf!("VirusTotal : {n} antivirus signalent ce fichier — voir l'analyse", "VirusTotal: {n} antivirus flag this file — see the report", n = n),
            )
        }
        Scan::Running(stage) => ("", stage_text(*stage), p.accent, None, tr!("Analyse VirusTotal en cours", "VirusTotal analysis in progress").to_owned()),
        Scan::Failed(reason) => (
            icon::SHIELD_SLASH,
            tr!("Réessayer", "Retry").to_owned(),
            p.danger,
            Some(Action::Scan(e.download.id)),
            trf!("Analyse impossible : {reason}. Cliquez pour réessayer.", "Analysis failed: {reason}. Click to retry.", reason = reason),
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
        Stage::Queued => tr!("VirusTotal · en attente", "VirusTotal · waiting").into(),
        Stage::Hashing => tr!("VirusTotal · empreinte…", "VirusTotal · hashing…").into(),
        Stage::LookingUp => tr!("VirusTotal · recherche…", "VirusTotal · looking up…").into(),
        Stage::Uploading(f) => {
            let pct = (f * 100.0).floor() as u32;
            trf!("VirusTotal · envoi {pct} %", "VirusTotal · uploading {pct} %", pct = pct)
        }
        Stage::Analyzing => tr!("VirusTotal · analyse…", "VirusTotal · analyzing…").into(),
    }
}

fn context_menu(ui: &mut Ui, p: &Palette, settings: &Settings, e: &Entry, actions: &mut Vec<Action>) {
    ui.set_min_width(270.0);
    let d = &e.download;
    let item = |ui: &mut Ui, glyph: &str, label: &str, color: Color32| {
        ui.add(eframe::egui::Button::new(RichText::new(format!("{glyph}   {label}")).font(theme::regular(13.5)).color(color)).frame(false))
            .clicked()
    };
    let done = *d.status() == Status::Completed;
    if done {
        if item(ui, icon::ARROW_SQUARE_OUT, tr!("Ouvrir", "Open"), p.text) {
            actions.push(Action::Open(d.target.clone()));
        }
        let scan = match &e.scan {
            Scan::Done(_) => Some((tr!("Voir l'analyse VirusTotal", "See the VirusTotal report"), Action::ShowReport(d.id))),
            Scan::None | Scan::Failed(_) if e.scannable() => Some((tr!("Analyser avec VirusTotal", "Scan with VirusTotal"), Action::Scan(d.id))),
            _ => None,
        };
        if let Some((label, action)) = scan
            && item(ui, icon::SHIELD_CHECK, label, p.text)
        {
            actions.push(action);
        }
        match &e.sha256 {
            Some(hash) => {
                let short = hash.get(..10).unwrap_or(hash);
                if item(ui, icon::FINGERPRINT, &trf!("Copier le SHA-256 ({short}…)", "Copy the SHA-256 ({short}…)", short = short), p.text) {
                    actions.push(Action::Copy(hash.clone(), "SHA-256"));
                }
            }
            None => {
                if item(ui, icon::FINGERPRINT, tr!("Calculer le SHA-256", "Compute the SHA-256"), p.text) {
                    actions.push(Action::Hash(d.id));
                }
            }
        }
    }
    if !d.is_recording() {
        if item(ui, icon::SEAL_CHECK, tr!("Vérifier une empreinte…", "Verify a checksum…"), p.text) {
            actions.push(Action::Edit(d.id, Field::Checksum));
        }
        if !done {
            if item(ui, icon::LINK_SIMPLE, tr!("Changer le lien…", "Change the link…"), p.text) {
                actions.push(Action::Edit(d.id, Field::Url));
            }
            if item(ui, icon::GAUGE, tr!("Limiter la vitesse…", "Limit the speed…"), p.text) {
                actions.push(Action::Edit(d.id, Field::SpeedLimit));
            }
            if !settings.queues.is_empty() {
                ui.menu_button(RichText::new(format!("{}   {}", icon::QUEUE, tr!("Déplacer vers", "Move to"))).font(theme::regular(13.5)).color(p.text), |ui| {
                    for queue in std::iter::once(0).chain(settings.queues.iter().map(|q| q.id)) {
                        let mark = if d.queue == queue { "● " } else { "   " };
                        if ui.button(format!("{mark}{}", queue_name(settings, queue))).clicked() {
                            actions.push(Action::MoveToQueue(d.id, queue));
                            ui.close();
                        }
                    }
                });
            }
            let (label, color) = if d.insecure {
                (tr!("Ne plus accepter le certificat non valide", "Stop accepting the invalid certificate"), p.text)
            } else {
                (tr!("Accepter un certificat non valide (ce téléchargement)", "Accept an invalid certificate (this download)"), p.warning)
            };
            if item(ui, icon::SHIELD_WARNING, label, color) {
                actions.push(Action::SetInsecure(d.id, !d.insecure));
            }
        }
    }
    ui.separator();
    if item(ui, icon::FOLDER_OPEN, tr!("Afficher dans le dossier", "Show in folder"), p.text) {
        actions.push(Action::Reveal(d.target.clone()));
    }
    let link = if d.is_recording() { tr!("Copier le lien de la page", "Copy the page link") } else { tr!("Copier le lien", "Copy the link") };
    if item(ui, icon::LINK, link, p.text) {
        actions.push(Action::Copy(d.url.to_string(), tr!("Lien", "Link")));
    }
    if item(ui, icon::COPY, tr!("Copier le chemin", "Copy the path"), p.text) {
        actions.push(Action::Copy(d.target.display().to_string(), tr!("Chemin", "Path")));
    }
    ui.separator();
    if item(ui, icon::X, tr!("Retirer de la liste", "Remove from the list"), p.text) {
        actions.push(Action::Remove(d.id, false));
    }
    if item(ui, icon::TRASH, tr!("Supprimer le fichier", "Delete the file"), p.danger) {
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
                kbd(ui, "Ctrl");
                ui.label(RichText::new("+").color(p.faint));
                kbd(ui, "V");
                ui.label(RichText::new(tr!("n'importe où colle et télécharge", "anywhere pastes and downloads")).font(theme::regular(13.0)).color(p.muted));
            });
        }
    });
}
