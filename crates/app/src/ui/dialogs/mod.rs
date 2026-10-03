//! Dialogs: settings, VirusTotal report, Firefox pairing, what's new.

mod changelog;
mod report;
mod settings;

pub(super) use settings::SettingsTab;

use std::time::Instant;

use domain::{Category, MAX_CONNECTIONS};
use eframe::egui::{
    Align, Align2, Color32, ComboBox, Context, DragValue, Frame, Id, Label, Layout, Margin, Modal, Rect, RichText, ScrollArea,
    Sense, Slider, Stroke, StrokeKind, TextEdit, Ui, Vec2, pos2, vec2,
};
use egui_phosphor::regular as icon;

use super::{
    Action, App, category_icon, open_link,
    theme::{self, Palette},
    widgets::{self, accent_button, caption, ghost_button, icon_button, segmented, toggle},
};
use crate::{
    autostart,
    i18n::Language,
    manager::Scan,
    secrets::{Secrets, SiteLogin},
    settings::{ClipboardMode, ExistingFile, MAX_PARALLEL, ProxyMode, Queue, Settings, Theme},
    tr, trf, update,
    virustotal::{self, Report},
};

pub(super) fn dialog_frame(p: &Palette) -> Frame {
    Frame::new()
        .fill(p.surface)
        .corner_radius(22)
        .inner_margin(Margin::same(26))
        .stroke(Stroke::new(theme::HAIRLINE, p.border_strong))
        .shadow(eframe::egui::Shadow { offset: [0, 24], blur: 64, spread: 0, color: Color32::from_black_alpha(if p.dark { 170 } else { 60 }) })
}

pub(super) fn backdrop(p: &Palette) -> Color32 {
    Color32::from_black_alpha(if p.dark { 150 } else { 80 })
}

/// Icon tile, title, subtitle and a close button; `true` when the button is clicked.
pub(super) fn dialog_header(ui: &mut Ui, p: &Palette, glyph: &str, color: Color32, title: &str, subtitle: &str) -> bool {
    ui.horizontal(|ui| {
        let (tile, _) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::hover());
        ui.painter().add(widgets::gradient(ui, tile, 14, color, color.lerp_to_gamma(p.accent2, 0.45), vec2(0.7, 0.7)));
        ui.painter().text(tile.center(), Align2::CENTER_CENTER, glyph, theme::regular(23.0), Color32::WHITE);
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.add_space(2.0);
            ui.label(RichText::new(title).font(theme::semibold(19.0)).color(p.text));
            ui.add(Label::new(RichText::new(subtitle).font(theme::regular(12.5)).color(p.muted)).truncate());
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| icon_button(ui, icon::X, tr!("Fermer (Échap)", "Close (Esc)"), None).clicked()).inner
    })
    .inner
}

/// A titled block of settings.
fn section(ui: &mut Ui, p: &Palette, glyph: &str, title: &str, add: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(if p.dark { p.bg.lerp_to_gamma(p.surface, 0.55) } else { p.raised })
        .corner_radius(16)
        .inner_margin(Margin::same(18))
        .stroke(Stroke::new(theme::HAIRLINE, p.border))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(glyph).font(theme::regular(18.0)).color(p.accent));
                ui.label(RichText::new(title).font(theme::semibold(14.5)).color(p.text));
            });
            ui.add_space(8.0);
            add(ui);
        });
    ui.add_space(12.0);
}

/// What downloading without confirmation risks: a site can then drop files in the download folder
/// unseen, and the browser's own check of dangerous files ("this file may harm your computer")
/// does not run, RDM taking the download over first. RDM never opens a file by itself.
fn unconfirmed_warning(ui: &mut Ui, p: &Palette) {
    widgets::icon_text(
        ui,
        icon::WARNING,
        p.warning,
        tr!(
            "Prudence : un site malveillant pourra alors déposer des fichiers dans votre dossier sans que vous le voyiez, sans l'avertissement « fichier dangereux » du navigateur. RDM n'ouvre jamais un fichier tout seul : n'ouvrez que ceux que vous attendiez.",
            "Caution: a malicious site could then drop files in your folder without you seeing it, without the browser's \"dangerous file\" warning. RDM never opens a file by itself: only open the ones you expected."
        ),
        p.muted,
        12.0,
    );
}

/// Grey explanatory text under a setting.
fn note(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add(Label::new(RichText::new(text).font(theme::regular(12.0)).color(p.muted)).wrap());
}

/// The interface language: the system's (detected) or one picked in a drop-down list, each named in
/// its own language.
fn language_menu(ui: &mut Ui, language: &mut Language) {
    let name = |l: Language| match l {
        Language::Auto => trf!("Automatique · {name}", "Automatic · {name}", name = Language::system().native_name()),
        other => other.native_name().to_owned(),
    };
    ComboBox::from_id_salt("language")
        .width(ui.available_width().min(320.0))
        .selected_text(name(*language))
        .show_ui(ui, |ui| {
            for option in std::iter::once(Language::Auto).chain(Language::ALL) {
                ui.selectable_value(language, option, name(option));
            }
        });
}

/// A label on the left of a row of controls.
fn row_label(ui: &mut Ui, p: &Palette, text: &str) {
    ui.allocate_ui_with_layout(vec2(150.0, 30.0), Layout::left_to_right(Align::Center), |ui| {
        ui.label(RichText::new(text).font(theme::regular(13.5)).color(p.text));
    });
}

fn text_field<'t>(text: &'t mut String, hint: &str, p: &Palette, width: f32) -> TextEdit<'t> {
    TextEdit::singleline(text).hint_text(RichText::new(hint).color(p.faint)).desired_width(width).margin(Margin::symmetric(10, 7))
}

impl App<'_> {

    /// "Delete the file": the file goes for good (not to the recycle bin), so the user confirms.
    pub(super) fn delete_dialog(&mut self, ctx: &Context) {
        let Some(id) = self.confirm_delete else { return };
        let Some((name, path)) = self.manager.view(|es| es.iter().find(|e| e.download.id == id).map(|e| (e.name.clone(), e.download.target.clone()))) else {
            self.confirm_delete = None; // removed meanwhile
            return;
        };
        let p = Palette::from_ctx(ctx);
        let mut answer = None;
        let modal = Modal::new(Id::new("delete")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(500.0);
            if dialog_header(ui, &p, icon::TRASH, p.danger, tr!("Supprimer le fichier ?", "Delete the file?"), &name) {
                answer = Some(false);
            }
            ui.add_space(14.0);
            Frame::new().fill(p.raised).corner_radius(12).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(Label::new(RichText::new(path.display().to_string()).monospace().color(p.text)).wrap());
            });
            ui.add_space(10.0);
            ui.label(
                RichText::new(tr!(
                    "Le fichier est effacé du disque définitivement (il ne va pas dans la corbeille), et le téléchargement quitte la liste.",
                    "The file is erased from the disk for good (it does not go to the recycle bin), and the download leaves the list."
                ))
                .color(p.muted),
            );
            ui.add_space(16.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if accent_button(ui, icon::TRASH, tr!("Supprimer", "Delete")).clicked() {
                    answer = Some(true);
                }
                if ghost_button(ui, icon::X, tr!("Annuler", "Cancel")).clicked() {
                    answer = Some(false);
                }
            });
        });
        if modal.should_close() {
            answer = answer.or(Some(false));
        }
        match answer {
            Some(true) => {
                self.manager.remove(id, true);
                self.toasts.info(icon::TRASH, tr!("Fichier supprimé", "File deleted"));
                self.confirm_delete = None;
            }
            Some(false) => self.confirm_delete = None,
            None => {}
        }
    }

    /// A download sent by the browser waits for the user's go-ahead (`confirm_browser`).
    pub(super) fn confirm_prompt(&mut self, ctx: &Context) {
        let Some(crate::manager::ToConfirm { url, name, ask_existing: exists, confirming, waiting }) = self.manager.to_confirm() else { return };
        let p = Palette::from_ctx(ctx);
        let name = name.unwrap_or_else(|| engine::suggest_file_name(&url, None));
        // None: cancel; Some(existing): download, with that answer if the file is already there.
        let mut answer: Option<Option<ExistingFile>> = None;
        let mut cancel = false;
        Modal::new(Id::new("confirm-browser")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(500.0);
            let subtitle = if waiting > 1 {
                let more = crate::i18n::count(waiting as u64 - 1, ("autre en attente", "autres en attente"), ("more waiting", "more waiting"));
                trf!("Envoyé par le navigateur · {more}", "Sent by the browser · {more}", more = more)
            } else {
                tr!("Envoyé par le navigateur", "Sent by the browser").to_owned()
            };
            // Only there for the file already in the folder (no confirmation of every download).
            let (title, subtitle) = if confirming {
                (tr!("Nouveau téléchargement", "New download"), subtitle)
            } else {
                (tr!("Le fichier existe déjà", "The file already exists"), tr!("Que voulez-vous faire ?", "What do you want to do?").to_owned())
            };
            if dialog_header(ui, &p, icon::DOWNLOAD_SIMPLE, p.accent, title, &subtitle) {
                cancel = true;
            }
            ui.add_space(14.0);
            Frame::new().fill(p.raised).corner_radius(12).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(Label::new(RichText::new(&name).font(theme::semibold(14.0)).color(p.text)).wrap());
                ui.add_space(4.0);
                ui.add(Label::new(RichText::new(url.as_str()).monospace().small().color(p.muted)).truncate());
            });
            if exists {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(tr!(
                        "Un fichier de ce nom est déjà dans le dossier de téléchargement.",
                        "A file of this name is already in the download folder."
                    ))
                    .color(p.warning),
                );
            }
            if confirming {
                ui.add_space(12.0);
                toggle(
                    ui,
                    &mut self.confirm_always,
                    tr!("Ne plus demander", "Don't ask again"),
                    tr!(
                        "Les nouveaux téléchargements se lanceront sans message de confirmation. Réactivable depuis les paramètres.",
                        "New downloads will start without a confirmation message. Can be turned back on in the settings."
                    ),
                );
                if self.confirm_always {
                    ui.add_space(6.0);
                    unconfirmed_warning(ui, &p);
                }
            }
            ui.add_space(16.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if exists {
                    if accent_button(ui, icon::COPY, tr!("Télécharger à côté", "Keep both")).clicked() {
                        answer = Some(Some(ExistingFile::Rename));
                    }
                    if ghost_button(ui, icon::ARROWS_CLOCKWISE, tr!("Remplacer", "Replace")).clicked() {
                        answer = Some(Some(ExistingFile::Overwrite));
                    }
                } else if accent_button(ui, icon::DOWNLOAD_SIMPLE, tr!("Télécharger", "Download")).clicked() {
                    answer = Some(None);
                }
                if ghost_button(ui, icon::X, tr!("Annuler", "Cancel")).clicked() {
                    cancel = true;
                }
            });
        });
        if cancel {
            self.manager.answer_confirm(false, None, false);
            self.confirm_always = false;
        } else if let Some(existing) = answer {
            // "Don't ask again" only counts with a download: cancelling one link says nothing about the next.
            self.manager.answer_confirm(true, existing, self.confirm_always);
            self.confirm_always = false;
        }
    }

    /// A Firefox extension asked to use the bridge: the user approves its (per-install) origin once.
    pub(super) fn firefox_prompt(&self, ctx: &Context) {
        let Some(origin) = self.manager.firefox_pending() else { return };
        let p = Palette::from_ctx(ctx);
        Modal::new(Id::new("firefox")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(500.0);
            let subtitle = tr!("Une extension demande à envoyer des téléchargements à RDM.", "An extension asks to send downloads to RDM.");
            if dialog_header(ui, &p, icon::PUZZLE_PIECE, p.warning, tr!("Extension Firefox / Waterfox", "Firefox / Waterfox extension"), subtitle) {
                self.manager.answer_firefox(None);
            }
            ui.add_space(14.0);
            Frame::new().fill(p.raised).corner_radius(12).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(Label::new(RichText::new(&origin).monospace().color(p.text)).wrap());
            });
            ui.add_space(10.0);
            ui.label(
                RichText::new(tr!(
                    "Autorisez-la seulement si vous venez d'installer ou de recharger l'extension RDM dans Firefox ou Waterfox.",
                    "Allow it only if you just installed or reloaded the RDM extension in Firefox or Waterfox."
                ))
                .color(p.muted),
            );
            ui.add_space(16.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if accent_button(ui, icon::CHECK, tr!("Autoriser", "Allow")).clicked() {
                    self.manager.answer_firefox(Some(true));
                }
                if ghost_button(ui, icon::X, tr!("Refuser", "Deny")).clicked() {
                    self.manager.answer_firefox(Some(false));
                }
            });
        });
    }
}

