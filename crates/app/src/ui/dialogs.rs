//! Dialogs: settings, VirusTotal report, Firefox pairing.

use std::time::Instant;

use domain::{Category, MAX_CONNECTIONS};
use eframe::egui::{
    Align, Align2, Color32, Context, DragValue, Frame, Id, Label, Layout, Margin, Modal, Rect, RichText, ScrollArea,
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

/// Named queues the settings let the user create (besides the main one).
const MAX_QUEUES: usize = 12;
const MAX_LOGINS: usize = 64;

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

/// Grey explanatory text under a setting.
fn note(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add(Label::new(RichText::new(text).font(theme::regular(12.0)).color(p.muted)).wrap());
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

/// What the settings form asks the window to do.
enum FormAction {
    Window(Action),
    TestProxy,
}

impl App<'_> {
    pub(super) fn settings_dialog(&mut self, ctx: &Context) {
        let Some(mut draft) = self.settings.take() else { return };
        let mut secrets = self.secrets.take().unwrap_or_else(|| self.manager.secrets());
        let before = draft.clone();
        let p = Palette::from_ctx(ctx);
        let asking_key = std::mem::take(&mut self.asking_key);
        let mut show_key = self.show_key;
        let modal = Modal::new(Id::new("settings")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(680.0);
            let close = dialog_header(
                ui,
                &p,
                icon::GEAR_SIX,
                p.accent,
                tr!("Paramètres", "Settings"),
                tr!("Chaque changement est appliqué et enregistré aussitôt.", "Every change is applied and saved at once."),
            );
            ui.add_space(16.0);
            // As tall as the window allows; without the minimum, the area would keep the height
            // available when the dialog first appeared (from the screen's middle down).
            let height = (ctx.screen_rect().height() - 210.0).max(240.0);
            let update_state = self.manager.update_state();
            let action = ScrollArea::vertical()
                .min_scrolled_height(height)
                .max_height(height)
                .show(ui, |ui| settings_form(ui, &p, &mut draft, &mut secrets, &mut show_key, asking_key, &update_state))
                .inner;
            (close, action)
        });
        let dismissed = modal.should_close();
        let (close, action) = modal.inner;
        let mut close = close;
        match action {
            Some(FormAction::Window(Action::OpenBrowsers)) => {
                close = true; // one window at a time
                self.open_browsers();
            }
            Some(FormAction::Window(Action::CheckUpdates)) => self.manager.check_updates(true),
            Some(FormAction::Window(Action::InstallUpdate)) => self.install_update(),
            Some(FormAction::TestProxy) => {
                self.manager.apply_settings(draft.clone());
                self.manager.set_secrets(secrets.clone());
                self.manager.test_proxy();
            }
            _ => {}
        }
        if self.manager.update_state().busy() {
            self.animating = true;
        }
        self.show_key = show_key;

        if draft.autostart != before.autostart && autostart::set(draft.autostart).is_err() {
            draft.autostart = before.autostart;
            self.toasts.warn(icon::WARNING, tr!("Impossible de modifier le lancement au démarrage", "Cannot change starting with the system"));
        }
        if draft.theme != before.theme {
            ctx.set_theme(super::preference(draft.theme));
        }
        if draft != before {
            self.manager.apply_settings(draft.clone());
            self.unsaved_since = Some(Instant::now());
        }
        // Passwords are encrypted and written once typing is over (no field focused) or on close.
        let typing = ctx.memory(|m| m.focused().is_some());
        if (close || dismissed || !typing) && secrets != self.manager.secrets() {
            self.manager.set_secrets(secrets.clone());
        }
        if close || dismissed {
            // Closing commits right away.
            self.manager.save_settings();
            self.unsaved_since = None;
        } else {
            self.settings = Some(draft);
            self.secrets = Some(secrets);
        }
    }

    pub(super) fn report_dialog(&mut self, ctx: &Context, actions: &mut Vec<Action>) {
        let Some(id) = self.report else { return };
        let found = self.manager.view(|es| {
            es.iter().find(|e| e.download.id == id).and_then(|e| match &e.scan {
                Scan::Done(r) => Some((e.name.clone(), r.clone())),
                _ => None,
            })
        });
        let Some((name, report)) = found else {
            self.report = None;
            return;
        };
        let p = Palette::from_ctx(ctx);
        let modal = Modal::new(Id::new("virustotal")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(560.0);
            report_view(ui, &p, &name, &report, actions)
        });
        if modal.inner || modal.should_close() {
            self.report = None;
        }
    }

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
        let Some((url, filename, exists, waiting)) = self.manager.to_confirm() else { return };
        let p = Palette::from_ctx(ctx);
        let name = filename.unwrap_or_else(|| engine::suggest_file_name(&url, None));
        // None: cancel; Some(existing): download, with that answer if the file is already there.
        let mut answer: Option<Option<ExistingFile>> = None;
        let mut cancel = false;
        Modal::new(Id::new("confirm-browser")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(500.0);
            let subtitle = if waiting > 1 {
                let more = crate::i18n::count(waiting as u64 - 1, ("autre en attente", "autres en attente"), ("more waiting", "more waiting"));
                trf!("Envoyé par le navigateur · {more}", "Sent by the browser · {more}")
            } else {
                tr!("Envoyé par le navigateur", "Sent by the browser").to_owned()
            };
            if dialog_header(ui, &p, icon::DOWNLOAD_SIMPLE, p.accent, tr!("Nouveau téléchargement", "New download"), &subtitle) {
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
            ui.add_space(12.0);
            toggle(
                ui,
                &mut self.confirm_always,
                tr!("Ne plus demander", "Don't ask again"),
                tr!("Réactivable dans les paramètres", "Can be turned back on in the settings"),
            );
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

fn settings_form(
    ui: &mut Ui,
    p: &Palette,
    s: &mut Settings,
    secrets: &mut Secrets,
    show_key: &mut bool,
    asking_key: bool,
    update_state: &update::State,
) -> Option<FormAction> {
    let mut action = None;
    section(ui, p, icon::TRANSLATE, tr!("Langue et apparence", "Language and appearance"), |ui| {
        segmented(
            ui,
            &mut s.language,
            &[(Language::Auto, icon::GLOBE, tr!("Système", "System")), (Language::English, icon::TRANSLATE, "English"), (Language::French, icon::TRANSLATE, "Français")],
        );
        ui.add_space(6.0);
        segmented(
            ui,
            &mut s.theme,
            &[
                (Theme::System, icon::DESKTOP, tr!("Système", "System")),
                (Theme::Dark, icon::MOON, tr!("Sombre", "Dark")),
                (Theme::Light, icon::SUN, tr!("Clair", "Light")),
            ],
        );
    });

    section(ui, p, icon::FOLDER, tr!("Emplacements", "Locations"), |ui| {
        if let Some(Some(dir)) = folder_row(ui, p, icon::HARD_DRIVES, tr!("Dossier principal", "Main folder"), &s.download_dir, false) {
            s.download_dir = dir;
        }
        toggle(
            ui,
            &mut s.categorize,
            tr!("Ranger chaque type dans son sous-dossier", "Sort each type into its own sub-folder"),
            tr!("Vidéos, Musique, Compressés… dans le dossier principal", "Videos, Music, Archives… inside the main folder"),
        );
        ui.add_space(4.0);
        let english = crate::i18n::english();
        for category in Category::ALL {
            let shown = s.category_dir(category);
            match folder_row(ui, p, category_icon(category), category.label(english), &shown, s.category_dirs.contains_key(&category)) {
                Some(Some(dir)) => {
                    s.category_dirs.insert(category, dir);
                }
                Some(None) => {
                    s.category_dirs.remove(&category);
                }
                None => {}
            }
        }
        ui.add_space(10.0);
        caption(ui, tr!("SI LE FICHIER EXISTE DÉJÀ", "IF THE FILE ALREADY EXISTS"));
        ui.add_space(4.0);
        segmented(
            ui,
            &mut s.existing,
            &[
                (ExistingFile::Rename, icon::COPY, tr!("Renommer", "Rename")),
                (ExistingFile::Overwrite, icon::ARROWS_CLOCKWISE, tr!("Remplacer", "Overwrite")),
                (ExistingFile::Skip, icon::PROHIBIT, tr!("Ignorer", "Skip")),
            ],
        );
        note(
            ui,
            p,
            match s.existing {
                ExistingFile::Rename => tr!("Le nouveau fichier devient « nom (1).ext ».", "The new file becomes \"name (1).ext\"."),
                ExistingFile::Overwrite => tr!("L'ancien fichier est remplacé par le nouveau.", "The old file is replaced by the new one."),
                ExistingFile::Skip => tr!("Le lien n'est pas téléchargé de nouveau.", "The link is not downloaded again."),
            },
        );
    });

    section(ui, p, icon::LIGHTNING, tr!("Performances", "Performance"), |ui| {
        ui.add(Slider::new(&mut s.connections, 1..=MAX_CONNECTIONS).text(tr!("connexions par fichier (au plus)", "connections per file (at most)")));
        ui.add(Slider::new(&mut s.max_parallel, 1..=MAX_PARALLEL).text(tr!("téléchargements simultanés (file principale)", "simultaneous downloads (main queue)")));
        ui.horizontal(|ui| {
            let mut mib = f64::from(s.speed_limit_kib) / 1024.0;
            ui.add(DragValue::new(&mut mib).range(0.0..=10_000.0).speed(0.1).max_decimals(1).suffix(tr!(" Mo/s", " MB/s")));
            let text = if s.speed_limit_kib == 0 { tr!("limite de vitesse globale : aucune (0)", "global speed limit: none (0)") } else { tr!("limite de vitesse globale", "global speed limit") };
            ui.label(RichText::new(text).color(p.muted));
            s.speed_limit_kib = (mib * 1024.0).round() as u32;
        });
        note(
            ui,
            p,
            tr!(
                "RDM adapte seul le nombre de connexions à la connexion et au serveur : ce réglage n'est qu'un plafond.",
                "RDM adapts the number of connections to the network and the server by itself: this setting is only a ceiling."
            ),
        );
    });

    section(ui, p, icon::QUEUE, tr!("Files d'attente", "Queues"), |ui| {
        note(
            ui,
            p,
            tr!(
                "Chaque file a son propre nombre de téléchargements simultanés. Clic droit sur un téléchargement › « Déplacer vers ».",
                "Each queue has its own number of simultaneous downloads. Right-click a download › \"Move to\"."
            ),
        );
        ui.add_space(6.0);
        let mut remove = None;
        for (i, queue) in s.queues.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(text_field(&mut queue.name, tr!("Nom de la file", "Queue name"), p, 220.0).char_limit(40));
                ui.add(Slider::new(&mut queue.max_parallel, 1..=MAX_PARALLEL).text(tr!("simultanés", "at once")));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_button(ui, icon::TRASH, tr!("Supprimer la file (ses téléchargements vont dans la file principale)", "Delete the queue (its downloads go to the main queue)"), None).clicked() {
                        remove = Some(i);
                    }
                });
            });
        }
        if let Some(i) = remove {
            s.queues.remove(i);
        }
        if s.queues.len() < MAX_QUEUES && ghost_button(ui, icon::PLUS, tr!("Nouvelle file", "New queue")).clicked() {
            let id = (1..).find(|id| !s.queues.iter().any(|q| q.id == *id)).unwrap_or(1);
            let n = s.queues.len() + 1;
            s.queues.push(Queue { id, name: trf!("File {n}", "Queue {n}"), max_parallel: 1 });
        }
    });

    section(ui, p, icon::BROWSERS, tr!("Navigateur et presse-papiers", "Browser and clipboard"), |ui| {
        caption(ui, tr!("FORMATS CAPTURÉS", "CAPTURED FORMATS"));
        ui.add_space(4.0);
        ui.add(TextEdit::multiline(&mut s.captured).desired_rows(3).desired_width(f32::INFINITY).font(eframe::egui::TextStyle::Monospace));
        note(
            ui,
            p,
            tr!(
                "Extensions séparées par des espaces. Les archives découpées (.r00, .001…) sont toujours capturées.",
                "Extensions separated by spaces. Split archives (.r00, .001…) are always captured."
            ),
        );
        ui.horizontal(|ui| {
            if ghost_button(ui, icon::ARROW_COUNTER_CLOCKWISE, tr!("Liste par défaut", "Default list")).clicked() {
                s.captured = domain::default_captured();
            }
            if accent_button(ui, icon::PUZZLE_PIECE, tr!("Installer l'extension…", "Install the extension…")).clicked() {
                action = Some(FormAction::Window(Action::OpenBrowsers));
            }
        });
        ui.add_space(6.0);
        toggle(
            ui,
            &mut s.confirm_browser,
            tr!("Confirmer les téléchargements du navigateur", "Confirm downloads from the browser"),
            tr!("RDM passe au premier plan et attend votre accord", "RDM comes to the front and waits for your go-ahead"),
        );
        ui.add_space(10.0);
        caption(ui, tr!("LIENS COPIÉS DANS LE PRESSE-PAPIERS", "LINKS COPIED TO THE CLIPBOARD"));
        ui.add_space(4.0);
        segmented(
            ui,
            &mut s.clipboard,
            &[
                (ClipboardMode::Off, icon::PROHIBIT, tr!("Ignorer", "Ignore")),
                (ClipboardMode::Ask, icon::CHAT_CIRCLE, tr!("Proposer", "Offer")),
                (ClipboardMode::Auto, icon::LIGHTNING, tr!("Télécharger", "Download")),
            ],
        );
        note(
            ui,
            p,
            tr!(
                "Seuls les liens vers un format capturé sont pris en compte ; rien n'est envoyé nulle part.",
                "Only links to a captured format count; nothing is sent anywhere."
            ),
        );
    });

    section(ui, p, icon::GLOBE_HEMISPHERE_WEST, tr!("Réseau et proxy", "Network and proxy"), |ui| {
        segmented(
            ui,
            &mut s.proxy.mode,
            &[
                (ProxyMode::Off, icon::PLUGS_CONNECTED, tr!("Direct", "Direct")),
                (ProxyMode::System, icon::DESKTOP, tr!("Système", "System")),
                (ProxyMode::Manual, icon::SHIELD, tr!("Toujours", "Always")),
                (ProxyMode::Auto, icon::MAGIC_WAND, tr!("Si lent", "If slow")),
            ],
        );
        note(
            ui,
            p,
            match s.proxy.mode {
                ProxyMode::Off => tr!("Connexions directes, quel que soit le réglage du système.", "Direct connections, whatever the system says."),
                ProxyMode::System => tr!("Le proxy configuré dans le système, s'il y en a un.", "The proxy configured in the system, if any."),
                ProxyMode::Manual => tr!("Tous les téléchargements passent par le proxy ci-dessous.", "Every download goes through the proxy below."),
                ProxyMode::Auto => tr!(
                    "Direct d'abord ; un téléchargement bloqué ou très lent bascule tout seul sur le proxy ci-dessous.",
                    "Direct first; a download that is blocked or very slow switches to the proxy below by itself."
                ),
            },
        );
        if matches!(s.proxy.mode, ProxyMode::Manual | ProxyMode::Auto) {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                row_label(ui, p, tr!("Adresse", "Address"));
                ui.add(text_field(&mut s.proxy.url, "socks5h://host:1080 · http://host:3128", p, ui.available_width()).font(eframe::egui::TextStyle::Monospace));
            });
            ui.horizontal(|ui| {
                row_label(ui, p, tr!("Identifiant", "User"));
                ui.add(text_field(&mut s.proxy.user, tr!("(facultatif)", "(optional)"), p, 180.0));
                ui.add(text_field(&mut secrets.proxy_password, tr!("mot de passe", "password"), p, 180.0).password(true));
            });
            note(
                ui,
                p,
                tr!(
                    "HTTP, HTTPS, SOCKS5 et SOCKS4a. Avec socks5h:// ou socks4a://, les noms sont résolus par le proxy (aucune requête DNS locale).",
                    "HTTP, HTTPS, SOCKS5 and SOCKS4a. With socks5h:// or socks4a://, names are resolved by the proxy (no local DNS request)."
                ),
            );
        }
        if s.proxy.mode != ProxyMode::Off {
            ui.add_space(4.0);
            if ghost_button(ui, icon::PLAY, tr!("Tester", "Test")).clicked() {
                action = Some(FormAction::TestProxy);
            }
        }
    });

    section(ui, p, icon::KEY, tr!("Identifiants des sites", "Site logins"), |ui| {
        note(
            ui,
            p,
            tr!(
                "Envoyés seulement à ce site (et ses sous-domaines), en HTTPS ; pour un site en HTTP local, écrivez « http://nom ». Mots de passe chiffrés pour votre seul compte.",
                "Sent only to that site (and its subdomains), over HTTPS; for a local HTTP site, type \"http://name\". Passwords are encrypted for your account only."
            ),
        );
        ui.add_space(6.0);
        let mut remove = None;
        for (i, login) in secrets.sites.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(text_field(&mut login.host, "example.com", p, 200.0));
                ui.add(text_field(&mut login.user, tr!("identifiant", "user"), p, 150.0));
                ui.add(text_field(&mut login.password, tr!("mot de passe", "password"), p, 150.0).password(true));
                if icon_button(ui, icon::TRASH, tr!("Supprimer", "Delete"), None).clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            secrets.sites.remove(i);
        }
        if secrets.sites.len() < MAX_LOGINS && ghost_button(ui, icon::PLUS, tr!("Ajouter un site", "Add a site")).clicked() {
            secrets.sites.push(SiteLogin::default());
        }
    });

    section(ui, p, icon::SHIELD_CHECK, "VirusTotal", |ui| {
        note(
            ui,
            p,
            tr!(
                "Analysez un fichier terminé (650 Mo au maximum) avec plus de 70 antivirus, sans quitter RDM. RDM le cherche d'abord par son empreinte SHA-256 : il n'est envoyé que si VirusTotal ne le connaît pas.",
                "Analyse a completed file (650 MB at most) with more than 70 antivirus engines, without leaving RDM. RDM first looks it up by its SHA-256: it is uploaded only if VirusTotal does not know it."
            ),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let key = ui.add(
                TextEdit::singleline(&mut s.virustotal_key)
                    .password(!*show_key)
                    .hint_text(RichText::new(tr!("Clé API VirusTotal (64 caractères)", "VirusTotal API key (64 characters)")).color(p.faint))
                    .desired_width(ui.available_width() - 90.0)
                    .margin(Margin::symmetric(12, 9))
                    .font(eframe::egui::TextStyle::Monospace),
            );
            if asking_key {
                key.request_focus();
                key.scroll_to_me(Some(Align::Center));
            }
            let (glyph, tip) = if *show_key { (icon::EYE_SLASH, tr!("Masquer la clé", "Hide the key")) } else { (icon::EYE, tr!("Afficher la clé", "Show the key")) };
            if icon_button(ui, glyph, tip, None).clicked() {
                *show_key = !*show_key;
            }
        });
        s.virustotal_key = s.virustotal_key.trim().to_owned();
        ui.horizontal(|ui| {
            if ui.link(RichText::new(format!("{}  {}", icon::KEY, tr!("Obtenir une clé gratuite", "Get a free key"))).font(theme::semibold(13.0))).clicked() {
                open_link(virustotal::KEY_PAGE.to_owned());
            }
            ui.label(RichText::new(tr!("(compte VirusTotal gratuit, onglet « API key »)", "(free VirusTotal account, \"API key\" tab)")).font(theme::regular(12.0)).color(p.faint));
        });
        ui.add_space(6.0);
        widgets::icon_text(
            ui,
            icon::WARNING,
            p.warning,
            tr!(
                "Un fichier envoyé est partagé avec les éditeurs d'antivirus : n'analysez pas vos documents personnels.",
                "An uploaded file is shared with antivirus vendors: do not analyse your personal documents."
            ),
            p.muted,
            12.0,
        );
    });

    section(ui, p, icon::DESKTOP, tr!("Système", "System"), |ui| {
        toggle(ui, &mut s.autostart, tr!("Lancer au démarrage", "Start with the system"), tr!("Démarre réduit dans la zone de notification", "Starts minimised in the notification area"));
        toggle(
            ui,
            &mut s.close_to_tray,
            tr!("Fermer la fenêtre garde RDM actif", "Closing the window keeps RDM running"),
            tr!("Les téléchargements continuent depuis la zone de notification", "Downloads go on from the notification area"),
        );
        toggle(ui, &mut s.notify, tr!("Notifications", "Notifications"), tr!("Téléchargement terminé, verdict VirusTotal", "Download complete, VirusTotal verdict"));
    });

    section(ui, p, icon::ROCKET_LAUNCH, tr!("Mises à jour", "Updates"), |ui| {
        toggle(
            ui,
            &mut s.check_updates,
            tr!("Rechercher automatiquement", "Check automatically"),
            tr!("Au démarrage puis une fois par jour, sur GitHub (une seule requête)", "At start then once a day, on GitHub (a single request)"),
        );
        let silent = matches!(update::method(), update::Method::Msi | update::Method::Binary);
        if silent {
            toggle(
                ui,
                &mut s.auto_update,
                tr!("Installer automatiquement", "Install automatically"),
                tr!("En silence, dès qu'aucun téléchargement n'est en cours ; RDM redémarre tout seul", "Silently, as soon as nothing is downloading; RDM restarts by itself"),
            );
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.add_enabled_ui(!update_state.busy(), |ui| ghost_button(ui, icon::ARROWS_CLOCKWISE, tr!("Rechercher maintenant", "Check now"))).inner.clicked() {
                action = Some(FormAction::Window(Action::CheckUpdates));
            }
            let version = env!("CARGO_PKG_VERSION");
            let (glyph, text, color) = match update_state {
                update::State::Idle => (icon::INFO, trf!("Version installée : {version}", "Installed version: {version}"), p.muted),
                update::State::Checking => (icon::CIRCLE_NOTCH, tr!("Recherche en cours…", "Checking…").to_owned(), p.accent),
                update::State::UpToDate => (icon::CHECK_CIRCLE, trf!("RDM est à jour ({version})", "RDM is up to date ({version})"), p.success),
                update::State::Available(r) => (icon::ROCKET_LAUNCH, trf!("Version {} disponible", "Version {} available", r.version), p.accent),
                update::State::Downloading(f) => {
                    let percent = (f * 100.0) as u32;
                    (icon::DOWNLOAD_SIMPLE, trf!("Téléchargement {percent} %", "Downloading {percent}%"), p.accent)
                }
                update::State::Installing => (icon::ROCKET_LAUNCH, tr!("Installation : RDM redémarre tout seul", "Installing: RDM restarts by itself").to_owned(), p.accent),
                update::State::InstallFailed(r, reason) => (icon::WARNING, trf!("Version {} : {reason}", "Version {}: {reason}", r.version), p.danger),
                update::State::Failed(reason) => (icon::WARNING, reason.clone(), p.danger),
            };
            ui.add(Label::new(RichText::new(format!("{glyph}  {text}")).font(theme::regular(13.0)).color(color)).wrap());
        });
        if let Some(r) = update_state.release() {
            ui.add_space(6.0);
            let label = match (update::installs_itself(r), update_state) {
                (true, update::State::InstallFailed(..)) => tr!("Réessayer l'installation", "Retry the installation"),
                (true, _) => tr!("Installer la mise à jour", "Install the update"),
                (false, _) => tr!("Voir la nouvelle version", "See the new version"),
            };
            if accent_button(ui, icon::ROCKET_LAUNCH, label).clicked() {
                action = Some(FormAction::Window(Action::InstallUpdate));
            }
        }
    });

    ui.add_space(2.0);
    widgets::icon_text(
        ui,
        icon::LOCK,
        p.success,
        tr!("Aucune télémétrie. Les cookies ne sont jamais écrits sur le disque.", "No telemetry. Cookies are never written to disk."),
        p.muted,
        12.0,
    );
    action
}

/// `icon  label  …path…  [Browse…] [↺]` — returns the new folder, or `Some(None)` to reset.
fn folder_row(ui: &mut Ui, p: &Palette, glyph: &str, label: &str, shown: &std::path::Path, custom: bool) -> Option<Option<std::path::PathBuf>> {
    let mut change = None;
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(vec2(170.0, 30.0), Layout::left_to_right(Align::Center), |ui| {
            widgets::icon_text(ui, glyph, p.muted, label, p.text, 13.5);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if custom && icon_button(ui, icon::ARROW_COUNTER_CLOCKWISE, tr!("Revenir au dossier par défaut", "Back to the default folder"), None).clicked() {
                change = Some(None);
            }
            if ghost_button(ui, icon::FOLDER_OPEN, tr!("Parcourir", "Browse")).clicked()
                && let Some(dir) = rfd::FileDialog::new().set_directory(shown).pick_folder()
            {
                change = Some(Some(dir));
            }
            let text = RichText::new(path_tail(shown)).monospace().color(if custom { p.accent } else { p.muted });
            ui.add(Label::new(text).truncate()).on_hover_text(shown.display().to_string());
        });
    });
    change
}

/// The distinguishing end of a path: `…\Downloads\Videos`.
fn path_tail(path: &std::path::Path) -> String {
    let parts: Vec<_> = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let sep = std::path::MAIN_SEPARATOR_STR;
    if parts.len() <= 3 { path.display().to_string() } else { format!("…{sep}{}", parts[parts.len() - 2..].join(sep)) }
}

/// Verdict, ring chart, detections; `true` when closed.
fn report_view(ui: &mut Ui, p: &Palette, name: &str, r: &Report, actions: &mut Vec<Action>) -> bool {
    let clean = r.flagged() == 0;
    let color = if clean {
        p.success
    } else if r.malicious > 0 {
        p.danger
    } else {
        p.warning
    };
    let (glyph, title) = if clean {
        (icon::SHIELD_CHECK, tr!("Aucune menace détectée", "No threat detected").to_owned())
    } else {
        let n = r.flagged();
        (icon::SHIELD_WARNING, trf!("{n} antivirus signalent ce fichier", "{n} antivirus engines flag this file"))
    };
    let close = dialog_header(ui, p, glyph, color, &title, name);
    ui.add_space(18.0);

    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(150.0), Sense::hover());
        let center = rect.center();
        widgets::glow(ui.painter(), center, Vec2::splat(80.0), color.gamma_multiply(0.18));
        let segments = [
            (r.malicious as f32, p.danger),
            (r.suspicious as f32, p.warning),
            (r.undetected as f32, p.success.gamma_multiply(0.55)),
            (r.harmless as f32, p.success),
        ];
        widgets::donut(ui.painter(), center, 58.0, 14.0, &segments, p.border);
        ui.painter().text(center - vec2(0.0, 8.0), Align2::CENTER_CENTER, format!("{}/{}", r.flagged(), r.engines()), theme::bold(24.0), color);
        ui.painter().text(center + vec2(0.0, 16.0), Align2::CENTER_CENTER, tr!("détections", "detections"), theme::regular(11.5), p.muted);
        ui.add_space(14.0);
        ui.vertical(|ui| {
            ui.add_space(12.0);
            for (label, value, dot) in [
                (tr!("Malveillant", "Malicious"), r.malicious, p.danger),
                (tr!("Suspect", "Suspicious"), r.suspicious, p.warning),
                (tr!("Non détecté", "Undetected"), r.undetected, p.success.gamma_multiply(0.55)),
                (tr!("Sain", "Harmless"), r.harmless, p.success),
                (tr!("Non pris en charge", "Unsupported"), r.unavailable, p.faint),
            ] {
                ui.horizontal(|ui| {
                    let (d, _) = ui.allocate_exact_size(vec2(10.0, 18.0), Sense::hover());
                    ui.painter().circle_filled(d.center(), 4.5, dot);
                    ui.label(RichText::new(label).color(p.muted));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(8.0);
                        ui.label(RichText::new(value.to_string()).font(theme::semibold(14.0)).color(p.text));
                    });
                });
            }
        });
    });
    ui.add_space(14.0);

    if r.detections.is_empty() {
        Frame::new().fill(p.tint(p.success, 0.12)).corner_radius(12).inner_margin(Margin::same(14)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let n = r.engines();
            widgets::icon_text(
                ui,
                icon::CHECK_CIRCLE,
                p.success,
                &trf!("Aucun des {n} antivirus n'a détecté de menace dans ce fichier.", "None of the {n} antivirus engines detected a threat in this file."),
                p.text,
                13.5,
            );
        });
    } else {
        caption(ui, tr!("DÉTECTIONS", "DETECTIONS"));
        ui.add_space(4.0);
        Frame::new().fill(p.raised).corner_radius(12).inner_margin(Margin::symmetric(14, 8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ScrollArea::vertical().max_height(190.0).show(ui, |ui| {
                for d in &r.detections {
                    ui.horizontal(|ui| {
                        let c = if d.malicious { p.danger } else { p.warning };
                        ui.label(RichText::new(if d.malicious { icon::BUG } else { icon::WARNING }).color(c));
                        ui.label(RichText::new(&d.engine).font(theme::semibold(13.0)).color(p.text));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add(Label::new(RichText::new(&d.label).font(theme::regular(12.5)).color(c)).truncate());
                        });
                    });
                }
            });
        });
    }
    ui.add_space(14.0);

    // Fingerprint and actions.
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    ui.painter().rect(rect, 10, p.raised, Stroke::new(theme::HAIRLINE, p.border), StrokeKind::Inside);
    ui.painter().text(pos2(rect.left() + 14.0, rect.center().y), Align2::LEFT_CENTER, icon::FINGERPRINT, theme::regular(16.0), p.muted);
    let hash = Rect::from_min_max(pos2(rect.left() + 38.0, rect.top()), pos2(rect.right() - 44.0, rect.bottom()));
    widgets::place(ui, hash, |ui| ui.add(Label::new(RichText::new(format!("SHA-256  {}", r.sha256)).monospace().color(p.muted)).truncate()));
    let copy = Rect::from_center_size(pos2(rect.right() - 22.0, rect.center().y), Vec2::splat(32.0));
    if widgets::place(ui, copy, |ui| icon_button(ui, icon::COPY, tr!("Copier le SHA-256", "Copy the SHA-256"), None)).clicked() {
        actions.push(Action::Copy(r.sha256.clone(), "SHA-256"));
    }
    ui.add_space(16.0);
    let mut done = close;
    ui.horizontal(|ui| {
        widgets::icon_text(ui, icon::SHIELD, p.faint, tr!("Analyse par VirusTotal", "Analysis by VirusTotal"), p.faint, 12.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if accent_button(ui, icon::CHECK, tr!("Fermer", "Close")).clicked() {
                done = true;
            }
            if ghost_button(ui, icon::ARROW_SQUARE_OUT, tr!("Rapport complet", "Full report")).on_hover_text(tr!("Ouvre la page de VirusTotal", "Opens VirusTotal's page")).clicked() {
                open_link(r.link());
            }
        });
    });
    done
}
