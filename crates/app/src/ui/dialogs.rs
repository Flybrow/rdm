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
    manager::Scan,
    settings::{Settings, Theme},
    update,
    virustotal::{self, Report},
};

fn dialog_frame(p: &Palette) -> Frame {
    Frame::new()
        .fill(p.surface)
        .corner_radius(22)
        .inner_margin(Margin::same(26))
        .stroke(Stroke::new(theme::HAIRLINE, p.border_strong))
        .shadow(eframe::egui::Shadow { offset: [0, 24], blur: 64, spread: 0, color: Color32::from_black_alpha(if p.dark { 170 } else { 60 }) })
}

fn backdrop(p: &Palette) -> Color32 {
    Color32::from_black_alpha(if p.dark { 150 } else { 80 })
}

/// Icon tile, title, subtitle and a close button; `true` when the button is clicked.
fn dialog_header(ui: &mut Ui, p: &Palette, glyph: &str, color: Color32, title: &str, subtitle: &str) -> bool {
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
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| icon_button(ui, icon::X, "Fermer (Échap)", None).clicked()).inner
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

impl App<'_> {
    pub(super) fn settings_dialog(&mut self, ctx: &Context) {
        let Some(mut draft) = self.settings.take() else { return };
        let before = draft.clone();
        let p = Palette::from_ctx(ctx);
        let asking_key = std::mem::take(&mut self.asking_key);
        let mut show_key = self.show_key;
        let modal = Modal::new(Id::new("settings")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(660.0);
            let close = dialog_header(ui, &p, icon::GEAR_SIX, p.accent, "Paramètres", "Chaque changement est appliqué et enregistré aussitôt.");
            ui.add_space(16.0);
            // As tall as the window allows; without the minimum, the area would keep the height
            // available when the dialog first appeared (from the screen's middle down).
            let height = (ctx.screen_rect().height() - 210.0).max(240.0);
            let update_state = self.manager.update_state();
            let action = ScrollArea::vertical()
                .min_scrolled_height(height)
                .max_height(height)
                .show(ui, |ui| settings_form(ui, &p, &mut draft, &mut show_key, asking_key, &update_state))
                .inner;
            (close, action)
        });
        let dismissed = modal.should_close();
        let (close, action) = modal.inner;
        match action {
            Some(Action::CheckUpdates) => self.manager.check_updates(true),
            Some(Action::InstallUpdate) => {
                if !self.manager.install_update()
                    && let update::State::Available(release) = self.manager.update_state()
                {
                    open_link(release.page);
                }
            }
            _ => {}
        }
        if matches!(self.manager.update_state(), update::State::Checking | update::State::Downloading(_)) {
            self.animating = true;
        }
        self.show_key = show_key;

        if draft.autostart != before.autostart && autostart::set(draft.autostart).is_err() {
            draft.autostart = before.autostart;
            self.toasts.warn(icon::WARNING, "Impossible de modifier le lancement au démarrage");
        }
        if draft.theme != before.theme {
            ctx.set_theme(super::preference(draft.theme));
        }
        if draft != before {
            self.manager.apply_settings(draft.clone());
            self.unsaved_since = Some(Instant::now());
        }
        if close || dismissed {
            // Closing commits right away.
            self.manager.save_settings();
            self.unsaved_since = None;
        } else {
            self.settings = Some(draft);
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

    /// A Firefox extension asked to use the bridge: the user approves its (per-install) origin once.
    pub(super) fn firefox_prompt(&self, ctx: &Context) {
        let Some(origin) = self.manager.firefox_pending() else { return };
        let p = Palette::from_ctx(ctx);
        Modal::new(Id::new("firefox")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(500.0);
            if dialog_header(ui, &p, icon::PUZZLE_PIECE, p.warning, "Extension Firefox", "Une extension demande à envoyer des téléchargements à RDM.") {
                self.manager.answer_firefox(None);
            }
            ui.add_space(14.0);
            Frame::new().fill(p.raised).corner_radius(12).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(Label::new(RichText::new(&origin).monospace().color(p.text)).wrap());
            });
            ui.add_space(10.0);
            ui.label(
                RichText::new("Autorisez-la seulement si vous venez d'installer ou de recharger l'extension RDM dans Firefox.")
                    .color(p.muted),
            );
            ui.add_space(16.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if accent_button(ui, icon::CHECK, "Autoriser").clicked() {
                    self.manager.answer_firefox(Some(true));
                }
                if ghost_button(ui, icon::X, "Refuser").clicked() {
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
    show_key: &mut bool,
    asking_key: bool,
    update_state: &update::State,
) -> Option<Action> {
    let mut action = None;
    section(ui, p, icon::FOLDER, "Emplacements", |ui| {
        if let Some(Some(dir)) = folder_row(ui, p, icon::HARD_DRIVES, "Dossier principal", &s.download_dir, false) {
            s.download_dir = dir;
        }
        toggle(ui, &mut s.categorize, "Ranger chaque type dans son sous-dossier", "Vidéos, Musique, Compressés… dans le dossier principal");
        ui.add_space(4.0);
        for category in Category::ALL {
            let shown = s.category_dir(category);
            match folder_row(ui, p, category_icon(category), category.label(), &shown, s.category_dirs.contains_key(&category)) {
                Some(Some(dir)) => {
                    s.category_dirs.insert(category, dir);
                }
                Some(None) => {
                    s.category_dirs.remove(&category);
                }
                None => {}
            }
        }
    });

    section(ui, p, icon::LIGHTNING, "Performances", |ui| {
        ui.add(Slider::new(&mut s.connections, 1..=MAX_CONNECTIONS).text("connexions par fichier"));
        ui.add(Slider::new(&mut s.max_parallel, 1..=16).text("téléchargements simultanés"));
        ui.horizontal(|ui| {
            let mut mib = f64::from(s.speed_limit_kib) / 1024.0;
            ui.add(DragValue::new(&mut mib).range(0.0..=10_000.0).speed(0.1).max_decimals(1).suffix(" Mo/s"));
            ui.label(RichText::new(if s.speed_limit_kib == 0 { "limite de vitesse : aucune (0)" } else { "limite de vitesse" }).color(p.muted));
            s.speed_limit_kib = (mib * 1024.0).round() as u32;
        });
    });

    section(ui, p, icon::BROWSERS, "Navigateur — formats capturés", |ui| {
        ui.add(TextEdit::multiline(&mut s.captured).desired_rows(3).desired_width(f32::INFINITY).font(eframe::egui::TextStyle::Monospace));
        ui.horizontal(|ui| {
            ui.add(
                Label::new(
                    RichText::new("Extensions séparées par des espaces. Les archives découpées (.r00, .001…) sont toujours capturées.")
                        .font(theme::regular(12.0))
                        .color(p.muted),
                )
                .wrap(),
            );
        });
        if ghost_button(ui, icon::ARROW_COUNTER_CLOCKWISE, "Liste par défaut").clicked() {
            s.captured = domain::default_captured();
        }
    });

    section(ui, p, icon::SHIELD_CHECK, "VirusTotal", |ui| {
        ui.add(
            Label::new(
                RichText::new(
                    "Analysez un fichier terminé (650 Mo au maximum) avec plus de 70 antivirus, sans quitter RDM. \
                     RDM le cherche d'abord par son empreinte SHA-256 : il n'est envoyé que si VirusTotal ne le connaît pas.",
                )
                .color(p.muted),
            )
            .wrap(),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let key = ui.add(
                TextEdit::singleline(&mut s.virustotal_key)
                    .password(!*show_key)
                    .hint_text(RichText::new("Clé API VirusTotal (64 caractères)").color(p.faint))
                    .desired_width(ui.available_width() - 90.0)
                    .margin(Margin::symmetric(12, 9))
                    .font(eframe::egui::TextStyle::Monospace),
            );
            if asking_key {
                key.request_focus();
                key.scroll_to_me(Some(Align::Center));
            }
            let (glyph, tip) = if *show_key { (icon::EYE_SLASH, "Masquer la clé") } else { (icon::EYE, "Afficher la clé") };
            if icon_button(ui, glyph, tip, None).clicked() {
                *show_key = !*show_key;
            }
        });
        s.virustotal_key = s.virustotal_key.trim().to_owned();
        ui.horizontal(|ui| {
            if ui.link(RichText::new(format!("{}  Obtenir une clé gratuite", icon::KEY)).font(theme::semibold(13.0))).clicked() {
                open_link(virustotal::KEY_PAGE.to_owned());
            }
            ui.label(RichText::new("(compte VirusTotal gratuit, onglet « API key »)").font(theme::regular(12.0)).color(p.faint));
        });
        ui.add_space(6.0);
        widgets::icon_text(
            ui,
            icon::WARNING,
            p.warning,
            "Un fichier envoyé est partagé avec les éditeurs d'antivirus : n'analysez pas vos documents personnels.",
            p.muted,
            12.0,
        );
    });

    section(ui, p, icon::PALETTE, "Apparence", |ui| {
        segmented(ui, &mut s.theme, &[(Theme::System, icon::DESKTOP, "Système"), (Theme::Dark, icon::MOON, "Sombre"), (Theme::Light, icon::SUN, "Clair")]);
    });

    section(ui, p, icon::DESKTOP, "Système", |ui| {
        toggle(ui, &mut s.autostart, "Lancer au démarrage", "Démarre réduit dans la zone de notification");
        toggle(ui, &mut s.close_to_tray, "Fermer la fenêtre garde RDM actif", "Les téléchargements continuent depuis la zone de notification");
        toggle(ui, &mut s.notify, "Notifications", "Téléchargement terminé, verdict VirusTotal");
    });

    section(ui, p, icon::ROCKET_LAUNCH, "Mises à jour", |ui| {
        toggle(ui, &mut s.check_updates, "Rechercher automatiquement", "Au démarrage puis une fois par jour, sur GitHub (une seule requête)");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let busy = matches!(update_state, update::State::Checking | update::State::Downloading(_));
            if ui.add_enabled_ui(!busy, |ui| ghost_button(ui, icon::ARROWS_CLOCKWISE, "Rechercher maintenant")).inner.clicked() {
                action = Some(Action::CheckUpdates);
            }
            let (glyph, text, color) = match update_state {
                update::State::Idle => (icon::INFO, format!("Version installée : {}", env!("CARGO_PKG_VERSION")), p.muted),
                update::State::Checking => (icon::CIRCLE_NOTCH, "Recherche en cours…".to_owned(), p.accent),
                update::State::UpToDate => (icon::CHECK_CIRCLE, format!("RDM est à jour ({})", env!("CARGO_PKG_VERSION")), p.success),
                update::State::Available(r) => (icon::ROCKET_LAUNCH, format!("Version {} disponible", r.version), p.accent),
                update::State::Downloading(f) => (icon::DOWNLOAD_SIMPLE, format!("Téléchargement {} %", (f * 100.0) as u32), p.accent),
                update::State::Failed(reason) => (icon::WARNING, reason.clone(), p.danger),
            };
            widgets::icon_text(ui, glyph, color, &text, p.muted, 13.0);
        });
        if let update::State::Available(r) = update_state {
            ui.add_space(6.0);
            let label = if r.msi.is_some() && cfg!(windows) { "Installer la mise à jour" } else { "Voir la nouvelle version" };
            if accent_button(ui, icon::ROCKET_LAUNCH, label).clicked() {
                action = Some(Action::InstallUpdate);
            }
        }
    });

    ui.add_space(2.0);
    widgets::icon_text(
        ui,
        icon::LOCK,
        p.success,
        "Aucune télémétrie. Les cookies ne sont jamais écrits sur le disque.",
        p.muted,
        12.0,
    );
    action
}

/// `icon  label  …path…  [Parcourir…] [↺]` — returns the new folder, or `Some(None)` to reset.
fn folder_row(ui: &mut Ui, p: &Palette, glyph: &str, label: &str, shown: &std::path::Path, custom: bool) -> Option<Option<std::path::PathBuf>> {
    let mut change = None;
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(vec2(170.0, 30.0), Layout::left_to_right(Align::Center), |ui| {
            widgets::icon_text(ui, glyph, p.muted, label, p.text, 13.5);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if custom && icon_button(ui, icon::ARROW_COUNTER_CLOCKWISE, "Revenir au dossier par défaut", None).clicked() {
                change = Some(None);
            }
            if ghost_button(ui, icon::FOLDER_OPEN, "Parcourir").clicked()
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

/// The distinguishing end of a path: `…\Téléchargements\Vidéos`.
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
        (icon::SHIELD_CHECK, "Aucune menace détectée".to_owned())
    } else {
        (icon::SHIELD_WARNING, format!("{} antivirus signalent ce fichier", r.flagged()))
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
        ui.painter().text(center + vec2(0.0, 16.0), Align2::CENTER_CENTER, "détections", theme::regular(11.5), p.muted);
        ui.add_space(14.0);
        ui.vertical(|ui| {
            ui.add_space(12.0);
            for (label, value, dot) in [
                ("Malveillant", r.malicious, p.danger),
                ("Suspect", r.suspicious, p.warning),
                ("Non détecté", r.undetected, p.success.gamma_multiply(0.55)),
                ("Sain", r.harmless, p.success),
                ("Non pris en charge", r.unavailable, p.faint),
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
            widgets::icon_text(
                ui,
                icon::CHECK_CIRCLE,
                p.success,
                &format!("Aucun des {} antivirus n'a détecté de menace dans ce fichier.", r.engines()),
                p.text,
                13.5,
            );
        });
    } else {
        caption(ui, "DÉTECTIONS");
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
    if widgets::place(ui, copy, |ui| icon_button(ui, icon::COPY, "Copier le SHA-256", None)).clicked() {
        actions.push(Action::Copy(r.sha256.clone(), "SHA-256"));
    }
    ui.add_space(16.0);
    let mut done = close;
    ui.horizontal(|ui| {
        widgets::icon_text(ui, icon::SHIELD, p.faint, "Analyse par VirusTotal", p.faint, 12.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if accent_button(ui, icon::CHECK, "Fermer").clicked() {
                done = true;
            }
            if ghost_button(ui, icon::ARROW_SQUARE_OUT, "Rapport complet").on_hover_text("Ouvre la page de VirusTotal").clicked() {
                open_link(r.link());
            }
        });
    });
    done
}
