//! The browser-extension window: which browsers are on this computer, which ones the extension
//! is connected from, and one-click installation into each of them.

use std::path::{Path, PathBuf};

use eframe::egui::{Align, Color32, Context, Frame, Id, Label, Layout, Margin, Modal, RichText, ScrollArea, Sense, Stroke, Ui, Vec2, vec2};
use egui_phosphor::regular as icon;

use super::{
    App, reveal_file,
    dialogs::{backdrop, dialog_frame, dialog_header},
    theme::{self, Palette},
    widgets::{self, accent_button, ghost_button},
};
use crate::{
    extension::{self, Browser, Flavour},
    manager::{Install, Installed},
};

/// Heard from within this long: shown as connected.
const LIVE_SECS: u64 = 15 * 60;

pub(super) struct Browsers {
    /// Each browser and its executable, if found (looked up when the window opens).
    found: Vec<(Browser, Option<PathBuf>)>,
    /// The browser whose instructions are unfolded.
    open: Option<Browser>,
}

impl App<'_> {
    pub(super) fn open_browsers(&mut self) {
        let mut found: Vec<_> = Browser::ALL.into_iter().map(|b| (b, b.find())).collect();
        // Browsers found here or already connected first (a portable browser is not "found").
        found.sort_by_key(|(b, exe)| exe.is_none() && self.manager.browser_last_seen(*b).is_none());
        self.browsers = Some(Browsers { found, open: None });
    }

    pub(super) fn browsers_dialog(&mut self, ctx: &Context) {
        let Some(mut state) = self.browsers.take() else { return };
        let p = Palette::from_ctx(ctx);
        let mut copied = None;
        let modal = Modal::new(Id::new("browsers")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(660.0);
            let close = dialog_header(
                ui,
                &p,
                icon::PUZZLE_PIECE,
                p.accent,
                "Extension du navigateur",
                "Capture les téléchargements et les vidéos des pages, et les envoie à RDM.",
            );
            ui.add_space(14.0);
            // As tall as the window allows (see the settings dialog: without the minimum, the area
            // keeps the height it had when the dialog first appeared).
            let height = (ctx.screen_rect().height() - 220.0).max(260.0);
            ScrollArea::vertical().min_scrolled_height(height).max_height(height).auto_shrink([false, true]).show(ui, |ui| {
                for (browser, exe) in &state.found {
                    self.browser_row(ui, &p, *browser, exe.as_deref(), &mut state.open, &mut copied);
                    ui.add_space(10.0);
                }
                widgets::icon_text(
                    ui,
                    icon::INFO,
                    p.faint,
                    &format!("Extension {} · une seule pour Chrome, Brave, Opera, Edge et Chromium ; une pour Firefox et Waterfox.", extension::version()),
                    p.faint,
                    12.0,
                );
            });
            close
        });
        if let Some(text) = copied {
            ctx.copy_text(text);
            self.toasts.info(icon::COPY, "Chemin copié : collez-le avec Ctrl+V");
        }
        if Browser::ALL.into_iter().any(|b| matches!(self.manager.install_state(b), Some(Install::Working))) {
            self.animating = true; // spinner
        }
        if !(modal.inner || modal.should_close()) {
            self.browsers = Some(state);
        }
    }

    fn browser_row(
        &self,
        ui: &mut Ui,
        p: &Palette,
        browser: Browser,
        exe: Option<&Path>,
        open: &mut Option<Browser>,
        copied: &mut Option<String>,
    ) {
        let seen = self.manager.browser_last_seen(browser).map(|t| unix_now().saturating_sub(t));
        let install = self.manager.install_state(browser);
        let (status, color) = match (seen, exe) {
            (Some(ago), _) if ago < LIVE_SECS => (format!("Connectée · dernier échange il y a {}", ago_text(ago)), p.success),
            (Some(ago), _) => (format!("Installée · dernier échange il y a {}", ago_text(ago)), p.muted),
            (None, Some(_)) => ("Détecté · extension pas encore installée".to_owned(), p.muted),
            (None, None) => ("Non détecté sur cet ordinateur".to_owned(), p.faint),
        };
        Frame::new()
            .fill(if p.dark { p.bg.lerp_to_gamma(p.surface, 0.55) } else { p.raised })
            .corner_radius(16)
            .inner_margin(Margin::same(14))
            .stroke(Stroke::new(theme::HAIRLINE, if seen.is_some_and(|a| a < LIVE_SECS) { p.tint(p.success, 0.45) } else { p.border }))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (tile, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                    let hue = if exe.is_some() || seen.is_some() { p.accent } else { p.faint };
                    ui.painter().add(widgets::gradient(ui, tile, 12, p.tint(hue, 0.30), p.tint(hue, 0.10), vec2(0.7, 0.7)));
                    let glyph = if matches!(browser, Browser::Chrome | Browser::Chromium) { icon::GOOGLE_CHROME_LOGO } else { icon::BROWSER };
                    ui.painter().text(tile.center(), eframe::egui::Align2::CENTER_CENTER, glyph, theme::regular(21.0), hue);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.add_space(2.0);
                        ui.label(RichText::new(browser.name()).font(theme::semibold(14.5)).color(p.text));
                        widgets::icon_text(ui, if color == p.success { icon::CHECK_CIRCLE } else { icon::CIRCLE }, color, &status, color, 12.5);
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if matches!(install, Some(Install::Working)) {
                            let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                            widgets::spinner(ui.painter(), r.center(), 8.0, p.accent, ui.input(|i| i.time) as f32);
                            ui.label(RichText::new("Préparation…").color(p.muted));
                        } else {
                            let label = if seen.is_some() { "Réinstaller" } else { "Installer" };
                            let clicked = if seen.is_some() {
                                ghost_button(ui, icon::ARROW_CLOCKWISE, label).clicked()
                            } else {
                                accent_button(ui, icon::DOWNLOAD_SIMPLE, label).clicked()
                            };
                            if clicked {
                                self.manager.install_extension(browser);
                                *open = Some(browser);
                            }
                            if install.is_some() && *open != Some(browser) && ghost_button(ui, icon::LIST_BULLETS, "Étapes").clicked() {
                                *open = Some(browser);
                            }
                        }
                    });
                });
                if *open == Some(browser) {
                    match &install {
                        Some(Install::Done(done)) => {
                            ui.add_space(10.0);
                            steps(ui, p, browser, exe, done, copied);
                        }
                        Some(Install::Failed(reason)) => {
                            ui.add_space(8.0);
                            widgets::icon_text(ui, icon::WARNING, p.danger, reason, p.danger, 13.0);
                        }
                        _ => {}
                    }
                }
            });
    }
}

/// What the user does in the browser to finish, with the folder / file at hand.
fn steps(ui: &mut Ui, p: &Palette, browser: Browser, exe: Option<&Path>, done: &Installed, copied: &mut Option<String>) {
    let name = browser.name();
    // What "reopen" opens again: the page, or the package the browser was handed.
    let mut reopen = (browser.extensions_page().to_owned(), "Rouvrir la page des extensions");
    let opened = |what: &str| if done.launched { format!("{name} vient de s'ouvrir sur {what}.") } else { format!("Ouvrez {what} dans {name}.") };
    match browser.flavour() {
        Flavour::Chromium => {
            step(ui, p, 1, &format!("{} Activez le « Mode développeur » (interrupteur de la page).", opened(browser.extensions_page())));
            step(ui, p, 2, "Cliquez sur « Charger l'extension non empaquetée » et choisissez ce dossier :");
            path_row(ui, p, &done.folder, copied);
            step(ui, p, 3, "L'icône RDM apparaît dans la barre d'outils : ce navigateur passe « Connectée » ici dès son premier échange.");
            note(ui, p, "RDM tient ce dossier à jour : l'extension suit les nouvelles versions au prochain démarrage du navigateur. Ne le supprimez pas.");
        }
        Flavour::Firefox if done.signed => {
            let package = extension::base().join("rdm-firefox-signed.xpi");
            let first = if done.launched {
                format!("{name} demande de confirmer l'ajout de « RDM » : cliquez sur « Ajouter ».")
            } else {
                format!("Ouvrez ce fichier avec {name}, puis cliquez sur « Ajouter » :")
            };
            step(ui, p, 1, &first);
            if !done.launched {
                path_row(ui, p, &package, copied);
            }
            step(ui, p, 2, &format!("Au premier échange, RDM vous demande d'autoriser cette installation de {name} : cliquez sur « Autoriser »."));
            note(ui, p, "Version signée par Mozilla : installée pour de bon, mise à jour avec les versions de RDM.");
            reopen = (package.to_string_lossy().into_owned(), "Rouvrir le paquet");
        }
        // Waterfox installs an unsigned package for good (its signature check is off by default).
        Flavour::Firefox if browser == Browser::Waterfox && done.xpi.is_some() => {
            let xpi = done.xpi.as_deref().expect("checked");
            let first = if done.launched {
                "Waterfox propose d'ajouter « RDM » : cliquez sur « Ajouter »."
            } else {
                "Ouvrez ce fichier avec Waterfox (ou glissez-le dans sa fenêtre), puis cliquez sur « Ajouter » :"
            };
            step(ui, p, 1, first);
            if !done.launched {
                path_row(ui, p, xpi, copied);
            }
            step(ui, p, 2, "Au premier échange, RDM vous demande d'autoriser cette installation de Waterfox : cliquez sur « Autoriser ».");
            note(ui, p, "Installée pour de bon. Après une mise à jour de RDM, réinstallez-la ici pour passer à sa nouvelle version.");
            reopen = (xpi.to_string_lossy().into_owned(), "Rouvrir le paquet");
        }
        Flavour::Firefox => {
            step(ui, p, 1, &format!("{} Cliquez sur « Charger un module complémentaire temporaire… ».", opened("about:debugging (« Ce Firefox »)")));
            step(ui, p, 2, "Choisissez le fichier manifest.json de ce dossier :");
            path_row(ui, p, &done.folder, copied);
            step(ui, p, 3, "RDM vous demande alors d'autoriser l'extension : cliquez sur « Autoriser ».");
            note(
                ui,
                p,
                "Firefox n'installe durablement que les extensions signées par Mozilla : un module temporaire disparaît à la fermeture de Firefox. \
                 Firefox Developer Edition, Nightly, ESR, Waterfox et LibreWolf installent durablement ce fichier :",
            );
            if let Some(xpi) = &done.xpi {
                path_row(ui, p, xpi, copied);
            }
        }
    }
    if let Some(exe) = exe {
        ui.add_space(6.0);
        let (target, label) = reopen;
        if ghost_button(ui, icon::ARROW_SQUARE_OUT, label).clicked() {
            let _ = extension::launch(exe, &target);
        }
    }
}

fn step(ui: &mut Ui, p: &Palette, n: u32, text: &str) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 10.0, p.tint(p.accent, 0.18));
        ui.painter().text(r.center(), eframe::egui::Align2::CENTER_CENTER, n.to_string(), theme::semibold(11.5), p.accent);
        ui.add(Label::new(RichText::new(text).font(theme::regular(13.0)).color(p.text)).wrap());
    });
    ui.add_space(4.0);
}

fn note(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(2.0);
    ui.add(Label::new(RichText::new(text).font(theme::regular(12.0)).color(p.muted)).wrap());
    ui.add_space(4.0);
}

/// A path to paste into the browser's file picker: copy it, or show it in the file manager.
fn path_row(ui: &mut Ui, p: &Palette, path: &Path, copied: &mut Option<String>) {
    Frame::new().fill(p.surface).corner_radius(10).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let text = path.display().to_string();
            let width = (ui.available_width() - 84.0).max(60.0);
            ui.allocate_ui_with_layout(vec2(width, 24.0), Layout::left_to_right(Align::Center), |ui| {
                ui.add(Label::new(RichText::new(&text).monospace().color(p.text)).truncate()).on_hover_text(&text);
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::icon_button(ui, icon::FOLDER_OPEN, "Afficher dans le dossier", None).clicked() {
                    // Selects the item: a folder opens in its parent with it highlighted.
                    reveal_file(path.to_path_buf());
                }
                if widgets::icon_button(ui, icon::COPY, "Copier le chemin", None).clicked() {
                    *copied = Some(text.clone());
                }
            });
        });
    });
    ui.add_space(6.0);
}

/// "3 min", "2 h", "4 j".
fn ago_text(secs: u64) -> String {
    match secs {
        0..60 => "moins d'une minute".to_owned(),
        60..3600 => format!("{} min", secs / 60),
        3600..86_400 => format!("{} h", secs / 3600),
        _ => format!("{} j", secs / 86_400),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Sidebar summary: which browsers are connected; opens the window. `true` when clicked.
pub(super) fn card(ui: &mut Ui, p: &Palette, (connected, installed): (Vec<Browser>, Vec<Browser>)) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 58.0), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(response.id, response.hovered(), 0.12);
    let painter = ui.painter();
    painter.rect(
        rect,
        14,
        p.surface.lerp_to_gamma(p.raised, hover),
        Stroke::new(theme::HAIRLINE, p.border),
        eframe::egui::StrokeKind::Inside,
    );
    let tile = eframe::egui::Rect::from_center_size(eframe::egui::pos2(rect.left() + 28.0, rect.center().y), Vec2::splat(34.0));
    let color: Color32 = match (connected.is_empty(), installed.is_empty()) {
        (false, _) => p.success,
        (true, false) => p.muted,
        (true, true) => p.warning,
    };
    painter.rect_filled(tile, 10, p.tint(color, 0.16));
    painter.text(tile.center(), eframe::egui::Align2::CENTER_CENTER, icon::PUZZLE_PIECE, theme::regular(18.0), color);
    let x = tile.right() + 12.0;
    painter.text(eframe::egui::pos2(x, rect.top() + 19.0), eframe::egui::Align2::LEFT_CENTER, "Extension navigateur", theme::regular(12.0), p.muted);
    let names = |list: &[Browser]| match list {
        [] => String::new(),
        [one] => one.name().to_owned(),
        [one, rest @ ..] => format!("{} +{}", one.name(), rest.len()),
    };
    let value = match (connected.is_empty(), installed.is_empty()) {
        (false, _) => format!("Connectée · {}", names(&connected)),
        (true, false) => format!("Installée · {}", names(&installed)),
        (true, true) => "À installer".to_owned(),
    };
    let galley = painter.layout_no_wrap(value, theme::semibold(13.0), p.text);
    let clip = eframe::egui::Rect::from_min_max(eframe::egui::pos2(x, rect.top()), eframe::egui::pos2(rect.right() - 8.0, rect.bottom()));
    painter.with_clip_rect(clip).galley(eframe::egui::pos2(x, rect.top() + 39.0 - galley.size().y / 2.0), galley, p.text);
    response.on_hover_text("Installer ou vérifier l'extension").clicked()
}

/// Browsers heard from recently (the extension checks in every few minutes while the browser
/// runs), and browsers heard from at all, for the sidebar card.
pub(super) fn connected(manager: &crate::manager::Manager) -> (Vec<Browser>, Vec<Browser>) {
    let now = unix_now();
    let seen: Vec<(Browser, u64)> = Browser::ALL.into_iter().filter_map(|b| Some((b, manager.browser_last_seen(b)?))).collect();
    let live = seen.iter().filter(|(_, t)| now.saturating_sub(*t) < LIVE_SECS).map(|(b, _)| *b).collect();
    (live, seen.into_iter().map(|(b, _)| b).collect())
}
