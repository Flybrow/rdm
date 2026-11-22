//! The second view of a download window: its progress — one line and the bar, the rest folded
//! under "Details" — then what to do with the file: open it, or its folder.

use std::{path::PathBuf, sync::Arc, time::Duration};

use domain::{Category, DownloadId, Segment, Status};
use eframe::egui::{Color32, Label, RichText, ScrollArea, Sense, Ui, vec2};
use egui_phosphor::regular as icon;

use super::{Heading, Next, buttons, fold, header, panel, row};
use crate::{
    i18n::{bytes, duration, speed},
    manager::{Entry, Manager, Scan, ScanRefused, Verify},
    tr, trf,
    ui::{
        card::text::status_style,
        category_icon,
        desktop::{open_file, reveal_file},
        failure,
        theme::{self, Palette},
        widgets::{self, accent_button, ghost_button, icon_button},
    },
};

/// Redraws while the download runs: 15 per second in front, 2 behind other windows.
const FRONT_FRAME: Duration = Duration::from_millis(66);
const BEHIND_FRAME: Duration = Duration::from_millis(500);
/// Waiting (queued, paused, retrying): its state may change at any moment.
const IDLE_FRAME: Duration = Duration::from_millis(500);

pub(in crate::ui) struct Track {
    pub(super) id: DownloadId,
    /// Close the window as soon as the download is complete.
    close_at_end: bool,
    /// The details are unfolded.
    details: bool,
    /// The SHA-256 of the finished file was asked for.
    hashing: bool,
    /// Why the VirusTotal scan could not start.
    scan_refused: Option<&'static str>,
    /// Whether it was complete at the previous frame (`None`: first frame): "close at the end"
    /// acts on the change only, not on a window opened on a finished download.
    was_complete: Option<bool>,
}

/// What the window shows of the download, copied out of the list (its lock is not held while
/// drawing, nor while acting on the download).
struct Shown {
    name: String,
    category: Category,
    status: Status,
    target: PathBuf,
    url: String,
    pill: (&'static str, Color32),
    done: u64,
    total: u64,
    connections: usize,
    speed: f64,
    eta: Option<u64>,
    pieces: Vec<Segment>,
    resumable: Option<bool>,
    sha256: Option<String>,
    scan: Scan,
    scannable: bool,
    verify: Verify,
    recording: bool,
    resolving: bool,
    /// Waiting to try again after a failure: why it failed.
    retry: Option<String>,
}

impl Shown {
    fn of(e: &Entry, p: &Palette) -> Self {
        let (done, total, connections) = e.progress.snapshot();
        let status = e.download.status().clone();
        let running = status == Status::Running;
        Self {
            name: e.name.clone(),
            category: e.category,
            target: e.download.target.clone(),
            url: e.download.url.to_string(),
            pill: status_style(p, e),
            done,
            total,
            connections,
            speed: e.speed,
            eta: e.eta(),
            pieces: if running && total > 0 { e.progress.pieces() } else { Vec::new() },
            resumable: e.progress.resumable(),
            sha256: e.sha256.clone(),
            scan: e.scan.clone(),
            scannable: e.scannable(),
            verify: e.verify.clone(),
            recording: e.download.is_recording(),
            resolving: e.resolving,
            retry: e.retry.as_ref().map(|r| failure::text(&r.reason)),
            status,
        }
    }

    fn folder(&self) -> String {
        self.target.parent().map(|d| d.display().to_string()).unwrap_or_default()
    }
}

impl Track {
    pub(super) fn new(id: DownloadId) -> Self {
        Self { id, close_at_end: false, details: false, hashing: false, scan_refused: None, was_complete: None }
    }

    pub(super) fn ui(&mut self, ui: &mut Ui, p: &Palette, manager: &Arc<Manager>, closing: bool) -> Next {
        // Closing the window hides it: the download goes on, in the main window's list.
        if closing {
            return Next::Close;
        }
        // Removed from the list meanwhile.
        let Some(s) = manager.view(|es| es.iter().find(|e| e.download.id == self.id).map(|e| Shown::of(e, p))) else {
            return Next::Close;
        };
        let complete = s.status == Status::Completed;
        if self.was_complete == Some(false) && complete && self.close_at_end {
            return Next::Close;
        }
        self.was_complete = Some(complete);
        match s.status {
            Status::Completed => self.complete(ui, p, manager, &s),
            Status::Failed(_) => failed(ui, p, manager, self.id, &s),
            _ => self.progress(ui, p, manager, &s),
        }
    }

    /// Running, queued or paused: one line, the bar; the details folded.
    fn progress(&mut self, ui: &mut Ui, p: &Palette, manager: &Arc<Manager>, s: &Shown) -> Next {
        let running = s.status == Status::Running;
        let fraction = (s.total > 0).then(|| (s.done.min(s.total) as f64 / s.total as f64) as f32);
        let got = if s.total > 0 { trf!("{got} sur {size}", "{got} of {size}", got = bytes(s.done), size = bytes(s.total)) } else { bytes(s.done) };
        let line = match &s.status {
            Status::Running => {
                let left = s.eta.map(|e| trf!(" · reste {}", " · {} left", duration(e))).unwrap_or_default();
                format!("{} · {got}{left}", speed(s.speed))
            }
            Status::Queued if s.resolving => tr!("analyse du lien auprès du serveur…", "asking the server about the link…").to_owned(),
            Status::Queued => match &s.retry {
                Some(reason) => trf!("{reason} · nouvel essai bientôt", "{reason} · retrying soon", reason = reason),
                None => trf!("{got} · en file d'attente", "{got} · queued", got = got),
            },
            _ => format!("{} · {got}", s.pill.0.to_lowercase()),
        };
        let percent = fraction.map(|f| format!("{} %", (f * 100.0).floor() as u32));
        let heading =
            Heading { glyph: category_icon(s.category), color: p.category(s.category), title: &s.name, subtitle: &line, busy: s.resolving, trailing: percent };
        header(ui, p, heading);
        ui.add_space(14.0);
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
        let gradient = match s.status {
            Status::Paused => (p.warning, p.warning.lerp_to_gamma(p.danger, 0.2)),
            Status::Queued => (p.faint, p.muted),
            _ => (p.accent, p.accent2),
        };
        if (2..=usize::from(domain::MAX_CONNECTIONS)).contains(&s.pieces.len()) {
            widgets::pieces_progress(ui, bar, &s.pieces, s.total, gradient);
        } else {
            widgets::progress(ui, bar, if running { fraction } else { fraction.or(Some(0.0)) }, gradient, running);
        }
        ui.add_space(12.0);
        if fold(ui, p, &mut self.details, tr!("Détails", "Details")) {
            ui.add_space(6.0);
            details(ui, p, s);
        }
        ui.add_space(14.0);
        let mut next = Next::Stay;
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.close_at_end, RichText::new(tr!("Fermer à la fin", "Close when done")).font(theme::regular(12.5)).color(p.muted));
            buttons(ui, |ui| {
                let pausable = matches!(s.status, Status::Running | Status::Queued) && !s.recording;
                if pausable && accent_button(ui, icon::PAUSE, tr!("Suspendre", "Pause")).clicked() {
                    manager.pause(self.id);
                }
                if s.status == Status::Paused && accent_button(ui, icon::PLAY, tr!("Reprendre", "Resume")).clicked() {
                    manager.resume(self.id);
                }
                if ghost_button(ui, icon::X, tr!("Annuler", "Cancel"))
                    .on_hover_text(tr!("Arrête le téléchargement et efface ce qui a été reçu", "Stops the download and erases what was received"))
                    .clicked()
                {
                    manager.remove(self.id, false);
                    next = Next::Close;
                }
                if ghost_button(ui, icon::EYE_SLASH, tr!("Masquer", "Hide"))
                    .on_hover_text(tr!(
                        "Le téléchargement continue ; double-cliquez dessus dans la liste pour rouvrir cette fenêtre",
                        "The download goes on; double-click it in the list to open this window again"
                    ))
                    .clicked()
                {
                    next = Next::Close;
                }
            });
        });
        let ctx = ui.ctx();
        if running {
            ctx.request_repaint_after(if ctx.input(|i| i.focused) { FRONT_FRAME } else { BEHIND_FRAME });
        } else {
            ctx.request_repaint_after(IDLE_FRAME);
        }
        next
    }

    /// Done: open it, or its folder; its SHA-256 and a VirusTotal scan, discreetly.
    fn complete(&mut self, ui: &mut Ui, p: &Palette, manager: &Arc<Manager>, s: &Shown) -> Next {
        let mut line = vec![tr!("Terminé", "Done").to_owned(), bytes(s.total.max(s.done)), s.folder()];
        match &s.verify {
            Verify::Ok => line.insert(1, tr!("✓ empreinte vérifiée", "✓ checksum verified").to_owned()),
            Verify::Running => line.insert(1, tr!("vérification de l'empreinte…", "verifying the checksum…").to_owned()),
            _ => {}
        }
        let line = line.join(" · ");
        let busy = matches!(s.verify, Verify::Running);
        header(ui, p, Heading { glyph: icon::CHECK, color: p.success, title: &s.name, subtitle: &line, busy, trailing: None });
        if let Verify::Mismatch(_) | Verify::Failed(_) = &s.verify {
            ui.add_space(8.0);
            let text = match &s.verify {
                Verify::Failed(reason) => trf!("Empreinte non vérifiée : {reason}", "Checksum not verified: {reason}", reason = reason),
                _ => tr!("✗ Empreinte différente : fichier corrompu ou pas le bon", "✗ Checksum mismatch: corrupted or not the expected file").to_owned(),
            };
            ui.label(RichText::new(text).color(p.danger));
        }
        if s.sha256.is_none() && !self.hashing {
            self.hashing = true;
            manager.compute_sha256(self.id);
        }
        ui.add_space(14.0);
        let mut copy = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("SHA-256").font(theme::regular(12.0)).color(p.faint));
            match &s.sha256 {
                Some(hash) => {
                    let room = ui.available_width() - widgets::ICON_BUTTON - 8.0;
                    ui.allocate_ui(vec2(room, 20.0), |ui| {
                        ui.add(Label::new(RichText::new(hash).monospace().small().color(p.muted)).truncate()).on_hover_text(hash);
                    });
                    if icon_button(ui, icon::COPY, tr!("Copier le SHA-256", "Copy the SHA-256"), None).clicked() {
                        copy = Some(hash.clone());
                    }
                }
                None => {
                    ui.label(RichText::new(tr!("calcul…", "computing…")).font(theme::regular(12.0)).color(p.faint));
                    ui.ctx().request_repaint_after(IDLE_FRAME);
                }
            }
        });
        if let Some((text, color)) = scan_text(p, &s.scan) {
            ui.horizontal(|ui| {
                ui.label(RichText::new("VirusTotal").font(theme::regular(12.0)).color(p.faint));
                ui.label(RichText::new(text).font(theme::regular(12.5)).color(color.unwrap_or(p.muted)));
            });
            if color.is_none() {
                ui.ctx().request_repaint_after(IDLE_FRAME);
            }
        }
        if let Some(reason) = self.scan_refused {
            ui.add(Label::new(RichText::new(reason).font(theme::regular(12.5)).color(p.warning)).wrap());
        }
        if let Some(hash) = copy {
            manager.copied_by_rdm(&hash);
            ui.ctx().copy_text(hash);
        }
        ui.add_space(16.0);
        let mut next = Next::Stay;
        let mut scan = false;
        buttons(ui, |ui| {
            if accent_button(ui, icon::ARROW_SQUARE_OUT, tr!("Ouvrir", "Open")).clicked() {
                open_file(s.target.clone(), manager);
                next = Next::Close;
            }
            if ghost_button(ui, icon::FOLDER_OPEN, tr!("Ouvrir l'emplacement", "Open the location")).clicked() {
                reveal_file(s.target.clone(), manager);
                next = Next::Close;
            }
            if ghost_button(ui, icon::X, tr!("Fermer", "Close")).clicked() {
                next = Next::Close;
            }
            let failed_scan = matches!(s.scan, Scan::Failed(_));
            if s.scannable && (matches!(s.scan, Scan::None) || failed_scan) {
                scan = ghost_button(ui, icon::SHIELD_CHECK, "VirusTotal")
                    .on_hover_text(tr!("Analyser le fichier avec VirusTotal", "Scan the file with VirusTotal"))
                    .clicked();
            }
        });
        if scan {
            self.scan_refused = match manager.scan_virustotal(self.id) {
                Ok(()) | Err(ScanRefused::NotEligible) => None,
                Err(ScanRefused::NoKey) => Some(tr!(
                    "Ajoutez d'abord votre clé API VirusTotal (gratuite) : Paramètres › Sécurité, dans la fenêtre principale.",
                    "Add your (free) VirusTotal API key first: Settings › Security, in the main window."
                )),
            };
        }
        next
    }
}

/// Failed: why, and a new try.
fn failed(ui: &mut Ui, p: &Palette, manager: &Arc<Manager>, id: DownloadId, s: &Shown) -> Next {
    let reason = match &s.status {
        Status::Failed(why) => failure::text(why),
        _ => String::new(),
    };
    header(ui, p, Heading { glyph: icon::WARNING, color: p.danger, title: &s.name, subtitle: &reason, busy: false, trailing: None });
    ui.add_space(16.0);
    let mut next = Next::Stay;
    buttons(ui, |ui| {
        if !s.recording && accent_button(ui, icon::ARROW_CLOCKWISE, tr!("Réessayer", "Retry")).clicked() {
            manager.resume(id);
        }
        if ghost_button(ui, icon::X, tr!("Fermer", "Close")).clicked() {
            next = Next::Close;
        }
    });
    ui.ctx().request_repaint_after(IDLE_FRAME);
    next
}

/// What the details unfold: connections, resuming, the link, and each piece of the file.
fn details(ui: &mut Ui, p: &Palette, s: &Shown) {
    panel(ui, p, |ui| {
        if s.status == Status::Running && !s.recording {
            row(ui, p, tr!("Connexions", "Connections"), RichText::new(s.connections.to_string()).color(p.text));
        }
        let resumable = match s.resumable {
            Some(true) => RichText::new(tr!("Oui", "Yes")).color(p.success),
            Some(false) => RichText::new(tr!("Non", "No")).color(p.warning),
            None => RichText::new("—").color(p.muted),
        };
        row(ui, p, tr!("Reprise possible", "Can resume"), resumable);
        row(ui, p, tr!("Dossier", "Folder"), RichText::new(s.folder()).color(p.text));
        row(ui, p, tr!("Adresse", "Address"), RichText::new(&s.url).monospace().small().color(p.muted));
        if s.pieces.len() >= 2 {
            ui.add_space(4.0);
            ScrollArea::vertical().max_height(120.0).auto_shrink([false, true]).show(ui, |ui| {
                for (n, piece) in s.pieces.iter().enumerate() {
                    let size = piece.end.saturating_add(1).saturating_sub(piece.start);
                    let got = piece.pos.min(piece.end.saturating_add(1)).saturating_sub(piece.start);
                    let state = if got >= size { tr!("terminé", "done").to_owned() } else { format!("{:.0} %", got as f64 * 100.0 / size.max(1) as f64) };
                    let text = trf!("{got} sur {size}", "{got} of {size}", got = bytes(got), size = bytes(size));
                    let color = if got >= size { p.success } else { p.text };
                    row(ui, p, &format!("#{}", n + 1), RichText::new(format!("{text} · {state}")).color(color));
                }
            });
        }
    });
}

/// The VirusTotal line: its text, and its colour (`None`: in progress).
fn scan_text(p: &Palette, scan: &Scan) -> Option<(String, Option<Color32>)> {
    Some(match scan {
        Scan::None => return None,
        Scan::Running(_) => (tr!("Analyse VirusTotal en cours…", "VirusTotal analysis in progress…").to_owned(), None),
        Scan::Done(r) if r.flagged() == 0 => {
            (trf!("aucune menace ({flagged}/{engines})", "no threat ({flagged}/{engines})", flagged = 0, engines = r.engines()), Some(p.success))
        }
        Scan::Done(r) => {
            let color = if r.malicious > 0 { p.danger } else { p.warning };
            let text = trf!("{flagged} antivirus sur {engines} signalent ce fichier", "{flagged} antivirus out of {engines} flag this file", flagged = r.flagged(), engines = r.engines());
            (text, Some(color))
        }
        Scan::Failed(reason) => (trf!("analyse impossible : {reason}", "analysis failed: {reason}", reason = reason), Some(p.danger)),
    })
}
