//! What the user asked for during a frame, carried out once the frame is drawn.

use std::path::PathBuf;

use domain::{DownloadId, Status};
use eframe::egui::Context;
use egui_phosphor::regular as icon;

use super::{
    App,
    desktop::{open_file, reveal_file},
    edit,
};
use crate::{manager::ScanRefused, tr, trf};

pub(super) enum Action {
    Resume(DownloadId),
    Pause(DownloadId),
    Remove(DownloadId, bool),
    Open(PathBuf),
    Reveal(PathBuf),
    /// Text to copy, and what it is (for the confirmation).
    Copy(String, &'static str),
    /// Opens a small editor on one download (new link, speed limit, checksum).
    Edit(DownloadId, edit::Field),
    SetInsecure(DownloadId, bool),
    MoveToQueue(DownloadId, u32),
    Hash(DownloadId),
    Scan(DownloadId),
    ShowReport(DownloadId),
    /// The download's own window (progress), opened or brought back.
    ShowProgress(DownloadId),
    PauseAll,
    ResumeAll,
    ClearCompleted,
    OpenSettings,
    CheckUpdates,
    InstallUpdate,
    OpenBrowsers,
}

impl App<'_> {
    pub(super) fn apply(&mut self, ctx: &Context, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Resume(id) => self.manager.resume(id),
                Action::Pause(id) => self.manager.pause(id),
                // Deleting a file cannot be undone: asked first (see `delete_dialog`).
                Action::Remove(id, true) => self.confirm_delete = Some(id),
                Action::Remove(id, false) => self.manager.remove(id, false),
                Action::Open(path) => open_file(path, &self.manager),
                Action::Reveal(path) => reveal_file(path, &self.manager),
                Action::Copy(text, what) => {
                    self.manager.copied_by_rdm(&text);
                    ctx.copy_text(text);
                    self.toasts.info(icon::COPY, &trf!("{what} copié", "{what} copied", what = what));
                }
                Action::Edit(id, field) => self.edit = edit::Editor::open(&self.manager, id, field),
                Action::SetInsecure(id, insecure) => {
                    self.manager.set_insecure(id, insecure);
                    if insecure {
                        self.toasts.warn(icon::WARNING, tr!("Certificat accepté pour ce téléchargement seulement", "Certificate accepted for this download only"));
                    }
                }
                Action::MoveToQueue(id, queue) => self.manager.move_to_queue(id, queue),
                Action::Hash(id) => self.manager.compute_sha256(id),
                Action::Scan(id) => match self.manager.scan_virustotal(id) {
                    Ok(()) => self.toasts.info(icon::SHIELD, tr!("Analyse VirusTotal lancée", "VirusTotal analysis started")),
                    Err(ScanRefused::NoKey) => {
                        self.open_settings();
                        self.asking_key = true;
                        self.toasts.warn(icon::KEY, tr!("Ajoutez d'abord votre clé API VirusTotal (gratuite)", "Add your (free) VirusTotal API key first"));
                    }
                    Err(ScanRefused::NotEligible) => {}
                },
                Action::ShowReport(id) => self.report = Some(id),
                Action::ShowProgress(id) => self.manager.show_progress(id),
                Action::PauseAll => self.manager.pause_all(),
                Action::ResumeAll => self.manager.resume_all(),
                Action::ClearCompleted => {
                    let n = self.manager.view(|es| es.iter().filter(|e| *e.download.status() == Status::Completed).count());
                    self.manager.clear_completed();
                    if n > 0 {
                        let text = crate::i18n::count(
                            n as u64,
                            ("téléchargement terminé retiré de la liste", "téléchargements terminés retirés de la liste"),
                            ("completed download removed from the list", "completed downloads removed from the list"),
                        );
                        self.toasts.info(icon::BROOM, &text);
                    }
                }
                Action::OpenSettings => self.open_settings(),
                Action::OpenBrowsers => self.open_browsers(),
                Action::CheckUpdates => self.manager.check_updates(true),
                Action::InstallUpdate => self.install_update(),
            }
        }
    }
}
