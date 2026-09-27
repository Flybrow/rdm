//! Small editors on one download — a new link, its own speed limit, the checksum to verify — and
//! the banner offering links copied to the clipboard.

use domain::DownloadId;
use eframe::egui::{Align, Context, DragValue, Frame, Id, Key, Label, Layout, Margin, Modal, RichText, Stroke, TextEdit, Ui};
use egui_phosphor::regular as icon;
use url::Url;

use super::{
    App,
    dialogs::{backdrop, dialog_frame, dialog_header},
    theme::{self, Palette},
    widgets::{accent_button, ghost_button},
};
use crate::{manager::Manager, tr, trf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Url,
    SpeedLimit,
    Checksum,
}

pub struct Editor {
    id: DownloadId,
    field: Field,
    name: String,
    text: String,
    /// Speed limit in MB/s (0 = none of its own).
    mbps: f64,
    error: Option<&'static str>,
}

impl Editor {
    pub fn open(manager: &Manager, id: DownloadId, field: Field) -> Option<Self> {
        manager.view(|entries| {
            let e = entries.iter().find(|e| e.download.id == id)?;
            let text = match field {
                Field::Url => e.download.url.to_string(),
                Field::Checksum => e.download.checksum.clone().unwrap_or_default(),
                Field::SpeedLimit => String::new(),
            };
            Some(Self { id, field, name: e.name.clone(), text, mbps: f64::from(e.download.speed_limit_kib) / 1024.0, error: None })
        })
    }
}

impl App<'_> {
    pub(super) fn edit_dialog(&mut self, ctx: &Context) {
        let Some(mut ed) = self.edit.take() else { return };
        let p = Palette::from_ctx(ctx);
        let (glyph, title) = match ed.field {
            Field::Url => (icon::LINK, tr!("Changer le lien", "Change the link")),
            Field::SpeedLimit => (icon::GAUGE, tr!("Limite de vitesse de ce téléchargement", "Speed limit of this download")),
            Field::Checksum => (icon::FINGERPRINT, tr!("Empreinte à vérifier", "Checksum to verify")),
        };
        let mut apply = false;
        let modal = Modal::new(Id::new("edit")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(560.0);
            let close = dialog_header(ui, &p, glyph, p.accent, title, &ed.name);
            ui.add_space(14.0);
            match ed.field {
                Field::Url | Field::Checksum => {
                    let hint = if ed.field == Field::Url {
                        "https://…"
                    } else {
                        tr!("SHA-256, SHA-1, MD5 ou SHA-512 publié par le site", "SHA-256, SHA-1, MD5 or SHA-512 published by the site")
                    };
                    let response = ui.add(
                        TextEdit::singleline(&mut ed.text)
                            .hint_text(RichText::new(hint).color(p.faint))
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(12, 9))
                            .font(eframe::egui::TextStyle::Monospace),
                    );
                    response.request_focus();
                    apply |= response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.add_space(8.0);
                    let note = if ed.field == Field::Url {
                        tr!(
                            "Pour un lien expiré ou déplacé : le téléchargement reprend là où il en était. Le serveur doit servir le même fichier (même taille), sinon le lien est refusé.",
                            "For an expired or moved link: the download resumes where it was. The server must serve the same file (same size), otherwise the link is refused."
                        )
                    } else {
                        tr!(
                            "Vérifiée dès que le fichier est complet (ou tout de suite s'il l'est). Laisser vide pour ne rien vérifier.",
                            "Checked as soon as the file is complete (right away if it is). Leave empty to check nothing."
                        )
                    };
                    ui.add(Label::new(RichText::new(note).font(theme::regular(12.5)).color(p.muted)).wrap());
                }
                Field::SpeedLimit => {
                    ui.horizontal(|ui| {
                        ui.add(DragValue::new(&mut ed.mbps).range(0.0..=10_000.0).speed(0.05).max_decimals(2).suffix(tr!(" Mo/s", " MB/s")));
                        let text = if ed.mbps <= 0.0 { tr!("aucune limite propre (0)", "no limit of its own (0)") } else { tr!("au plus", "at most") };
                        ui.label(RichText::new(text).color(p.muted));
                    });
                    ui.add_space(8.0);
                    ui.add(
                        Label::new(
                            RichText::new(tr!(
                                "S'applique tout de suite, en plus de la limite globale des paramètres.",
                                "Applies at once, on top of the global limit in the settings."
                            ))
                            .font(theme::regular(12.5))
                            .color(p.muted),
                        )
                        .wrap(),
                    );
                }
            }
            if let Some(error) = ed.error {
                ui.add_space(6.0);
                ui.label(RichText::new(error).color(p.danger));
            }
            ui.add_space(16.0);
            let mut cancel = close;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                apply |= accent_button(ui, icon::CHECK, tr!("Appliquer", "Apply")).clicked();
                cancel |= ghost_button(ui, icon::X, tr!("Annuler", "Cancel")).clicked();
            });
            cancel
        });
        if modal.inner || modal.should_close() {
            return;
        }
        if apply {
            match ed.field {
                Field::Url => match ed.text.trim().parse::<Url>() {
                    Ok(url) if matches!(url.scheme(), "http" | "https") => {
                        self.manager.change_url(ed.id, url);
                        return;
                    }
                    _ => ed.error = Some(tr!("Ce n'est pas un lien http(s) valide.", "This is not a valid http(s) link.")),
                },
                Field::SpeedLimit => {
                    let kib = (ed.mbps.max(0.0) * 1024.0).round() as u32;
                    self.manager.set_speed_limit(ed.id, kib);
                    return;
                }
                Field::Checksum => {
                    if self.manager.set_checksum(ed.id, &ed.text) {
                        return;
                    }
                    ed.error = Some(tr!("Empreinte non reconnue (MD5, SHA-1, SHA-256 ou SHA-512 en hexadécimal).", "Unrecognised checksum (MD5, SHA-1, SHA-256 or SHA-512, in hexadecimal)."));
                }
            }
        }
        self.edit = Some(ed);
    }

    /// Links just copied to the clipboard: one click to download them.
    pub(super) fn clipboard_banner(&mut self, ui: &mut Ui) {
        let Some(offer) = self.manager.clipboard_offer() else { return };
        let p = Palette::of(ui);
        let first = offer.urls.first().map(file_name).unwrap_or_default();
        let more = offer.urls.len().saturating_sub(1);
        let text = if more == 0 {
            trf!("Lien copié : {first}", "Link copied: {first}")
        } else {
            let more = crate::i18n::count(more as u64, ("autre", "autres"), ("more", "more"));
            trf!("Liens copiés : {first} et {more}", "Links copied: {first} and {more}")
        };
        let mut answer = None;
        Frame::new()
            .fill(p.tint(p.accent, 0.14))
            .corner_radius(14)
            .inner_margin(Margin::symmetric(16, 10))
            .stroke(Stroke::new(theme::HAIRLINE, p.tint(p.accent, 0.4)))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::CLIPBOARD_TEXT).font(theme::regular(18.0)).color(p.accent));
                    ui.add(Label::new(RichText::new(text).font(theme::semibold(13.5)).color(p.text)).truncate());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ghost_button(ui, icon::X, tr!("Ignorer", "Dismiss")).clicked() {
                            answer = Some(false);
                        }
                        if accent_button(ui, icon::DOWNLOAD_SIMPLE, tr!("Télécharger", "Download")).clicked() {
                            answer = Some(true);
                        }
                    });
                });
            });
        ui.add_space(12.0);
        if let Some(download) = answer {
            self.manager.answer_clipboard(download);
        } else {
            // Goes away on its own when it expires.
            let left = crate::manager::clipboard::OFFER_TTL.saturating_sub(offer.at.elapsed());
            ui.ctx().request_repaint_after(left);
        }
    }
}

/// The file name a link points to, for display.
fn file_name(url: &Url) -> String {
    let last = url.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty()).unwrap_or(url.as_str());
    percent_decode(last)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, ..) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_the_file_name_of_a_link() {
        assert_eq!(file_name(&"https://x.io/dir/My%20File.zip?x=1".parse().unwrap()), "My File.zip");
        assert_eq!(file_name(&"https://x.io/".parse().unwrap()), "https://x.io/");
        assert_eq!(percent_decode("%E2%82%AC%zz"), "€%zz");
    }
}
