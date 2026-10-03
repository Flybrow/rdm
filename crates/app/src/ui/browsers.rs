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
    manager::{Install, Installed, unix_now},
    tr, trf,
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
        // Every Chromium- or Firefox-based browser of the computer, known to RDM or not.
        let mut found: Vec<_> = self.manager.browsers().into_iter().map(|b| (b, b.find())).collect();
        // Browsers found here or already connected first (a portable browser is not "found").
        found.sort_by_key(|(b, exe)| exe.is_none() && self.manager.browser_last_seen(*b).is_none());
        self.browsers = Some(Browsers { found, open: None });
    }

    pub(super) fn browsers_dialog(&mut self, ctx: &Context) {
        let Some(mut state) = self.browsers.take() else { return };
        let p = Palette::from_ctx(ctx);
        let mut copied = None;
        let mut add = false;
        let modal = Modal::new(Id::new("browsers")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(660.0);
            let close = dialog_header(
                ui,
                &p,
                icon::PUZZLE_PIECE,
                p.accent,
                tr!("Extension du navigateur", "Browser extension"),
                tr!("Capture les téléchargements et les vidéos des pages, et les envoie à RDM.", "Captures downloads and videos from pages, and sends them to RDM."),
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
                // A browser Windows does not list (a portable one): picked by its executable, as in IDM.
                if ghost_button(ui, icon::PLUS, tr!("Ajouter un navigateur…", "Add a browser…")).clicked() {
                    add = true;
                }
                ui.add_space(8.0);
                widgets::icon_text(
                    ui,
                    icon::INFO,
                    p.faint,
                    &trf!(
                        "Extension {} · une pour tous les navigateurs basés sur Chromium (Chrome, Edge, Brave, Opera, Vivaldi…), une pour tous ceux basés sur Firefox (Waterfox, LibreWolf, Floorp, Zen…).",
                        "Extension {} · one for every Chromium-based browser (Chrome, Edge, Brave, Opera, Vivaldi…), one for every Firefox-based browser (Waterfox, LibreWolf, Floorp, Zen…).",
                        extension::version()
                    ),
                    p.faint,
                    12.0,
                );
            });
            close
        });
        if let Some(text) = copied {
            ctx.copy_text(text);
            self.toasts.info(icon::COPY, tr!("Chemin copié : collez-le avec Ctrl+V", "Path copied: paste it with Ctrl+V"));
        }
        if add {
            self.add_browser(&mut state);
        }
        if self.manager.installing() {
            self.animating = true; // spinner
        }
        if !(modal.inner || modal.should_close()) {
            self.browsers = Some(state);
        }
    }

    /// "Add a browser…": its executable, picked by the user; then listed like the others.
    fn add_browser(&mut self, state: &mut Browsers) {
        let mut dialog = rfd::FileDialog::new().set_title(tr!("Choisir le programme du navigateur", "Choose the browser's program"));
        if cfg!(windows) {
            dialog = dialog.add_filter(tr!("Programmes", "Programs"), &["exe"]);
        }
        let Some(exe) = dialog.pick_file() else { return };
        match self.manager.add_browser(exe.clone()) {
            Some(browser) => {
                state.found.retain(|(b, _)| *b != browser);
                state.found.insert(0, (browser, Some(exe)));
                state.open = None;
            }
            None => self.toasts.warn(
                icon::WARNING,
                tr!(
                    "Ce programme n'est pas un navigateur basé sur Chromium ou Firefox.",
                    "This program is not a Chromium- or Firefox-based browser."
                ),
            ),
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
            (Some(ago), _) if ago < LIVE_SECS => (trf!("Connectée · dernier échange il y a {}", "Connected · last heard {} ago", ago_text(ago)), p.success),
            (Some(ago), _) => (trf!("Installée · dernier échange il y a {}", "Installed · last heard {} ago", ago_text(ago)), p.muted),
            (None, Some(_)) => (tr!("Détecté · extension pas encore installée", "Found · extension not installed yet").to_owned(), p.muted),
            (None, None) => (tr!("Non détecté sur cet ordinateur", "Not found on this computer").to_owned(), p.faint),
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
                    let glyph = if matches!(browser.key(), "chrome" | "chromium") { icon::GOOGLE_CHROME_LOGO } else { icon::BROWSER };
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
                            ui.label(RichText::new(tr!("Préparation…", "Preparing…")).color(p.muted));
                        } else {
                            let label = if seen.is_some() { tr!("Réinstaller", "Reinstall") } else { tr!("Installer", "Install") };
                            let clicked = if seen.is_some() {
                                ghost_button(ui, icon::ARROW_CLOCKWISE, label).clicked()
                            } else {
                                accent_button(ui, icon::DOWNLOAD_SIMPLE, label).clicked()
                            };
                            if clicked {
                                self.manager.install_extension(browser);
                                *open = Some(browser);
                            }
                            // An icon: beside "Reinstall", a second labelled button left no room for the status line.
                            if seen.is_some()
                                && widgets::icon_button(ui, icon::TRASH, tr!("Supprimer l'extension", "Remove the extension"), Some(p.danger))
                                    .on_hover_text(tr!(
                                        "La désinstalle de ce navigateur à son prochain échange avec RDM (déjà supprimée : RDM l'oublie).",
                                        "Uninstalls it from this browser at its next exchange with RDM (already removed: RDM forgets it)."
                                    ))
                                    .clicked()
                            {
                                self.manager.remove_extension(browser);
                                if *open == Some(browser) {
                                    *open = None;
                                }
                            }
                            if install.is_some() && *open != Some(browser) && ghost_button(ui, icon::LIST_BULLETS, tr!("Étapes", "Steps")).clicked() {
                                *open = Some(browser);
                            }
                        }
                    });
                });
                if *open == Some(browser) {
                    match &install {
                        Some(Install::Done(done)) => {
                            ui.add_space(10.0);
                            if steps(ui, p, browser, exe, done, copied) {
                                self.manager.install_extension_without_store(browser);
                            }
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

/// What the user does in the browser to finish, with the folder / file at hand. `true`: the user
/// asked to install without the Firefox store.
fn steps(ui: &mut Ui, p: &Palette, browser: Browser, exe: Option<&Path>, done: &Installed, copied: &mut Option<String>) -> bool {
    let mut without_store = false;
    let name = browser.name();
    // What "reopen" opens again: the page, or the package the browser was handed.
    let mut reopen = (browser.extensions_page().to_owned(), tr!("Rouvrir la page des extensions", "Reopen the extensions page"));
    let opened = |what: &str| {
        if done.launched { trf!("{name} vient de s'ouvrir sur {what}.", "{name} just opened {what}.", name = name, what = what) } else { trf!("Ouvrez {what} dans {name}.", "Open {what} in {name}.", name = name, what = what) }
    };
    let reopen_package = tr!("Rouvrir le paquet", "Reopen the package");
    match browser.flavour() {
        Flavour::Chromium => {
            let page = opened(browser.extensions_page());
            step(ui, p, 1, &trf!("{page} Activez le « Mode développeur » (interrupteur de la page).", "{page} Turn on \"Developer mode\" (the page's switch).", page = page));
            step(ui, p, 2, tr!("Cliquez sur « Charger l'extension non empaquetée » et choisissez ce dossier :", "Click \"Load unpacked\" and choose this folder:"));
            path_row(ui, p, &done.folder, copied);
            step(
                ui,
                p,
                3,
                tr!(
                    "L'icône RDM apparaît dans la barre d'outils : ce navigateur passe « Connectée » ici dès son premier échange.",
                    "The RDM icon appears in the toolbar: this browser shows \"Connected\" here as soon as it first talks to RDM."
                ),
            );
            note(
                ui,
                p,
                tr!(
                    "RDM tient ce dossier à jour : l'extension suit les nouvelles versions au prochain démarrage du navigateur. Ne le supprimez pas.",
                    "RDM keeps this folder up to date: the extension follows new versions at the browser's next start. Do not delete it."
                ),
            );
        }
        Flavour::Firefox if done.store => {
            step(
                ui,
                p,
                1,
                &trf!(
                    "{name} vient de s'ouvrir sur la page de RDM du store Firefox : cliquez sur « Ajouter à Firefox », puis sur « Ajouter ».",
                    "{name} just opened RDM's page in the Firefox store: click \"Add to Firefox\", then \"Add\".",
                    name = name
                ),
            );
            step(ui, p, 2, connects_itself());
            note(ui, p, tr!("Depuis le store Firefox : installée pour de bon, mise à jour par le navigateur.", "From the Firefox store: installed for good, updated by the browser."));
            reopen = (extension::FIREFOX_STORE.to_owned(), tr!("Rouvrir la page du store", "Reopen the store page"));
            ui.add_space(4.0);
            without_store = ghost_button(ui, icon::PACKAGE, tr!("Le store ne s'ouvre pas ? Installer sans le store", "Store not opening? Install without the store")).clicked();
        }
        Flavour::Firefox if done.signed => {
            let package = extension::base().join("rdm-firefox-signed.xpi");
            let first = if done.launched {
                trf!("{name} demande de confirmer l'ajout de « RDM » : cliquez sur « Ajouter ».", "{name} asks to confirm adding \"RDM\": click \"Add\".", name = name)
            } else {
                trf!("Ouvrez ce fichier avec {name}, puis cliquez sur « Ajouter » :", "Open this file with {name}, then click \"Add\":", name = name)
            };
            step(ui, p, 1, &first);
            if !done.launched {
                path_row(ui, p, &package, copied);
            }
            step(ui, p, 2, connects_itself());
            note(ui, p, tr!("Version signée par Mozilla : installée pour de bon, mise à jour avec les versions de RDM.", "Signed by Mozilla: installed for good, updated with RDM's versions."));
            reopen = (package.to_string_lossy().into_owned(), reopen_package);
        }
        // Waterfox installs an unsigned package for good (its signature check is off by default).
        Flavour::Firefox if browser.key() == "waterfox" && done.xpi.is_some() => {
            let xpi = done.xpi.as_deref().expect("checked");
            let first = if done.launched {
                tr!("Waterfox propose d'ajouter « RDM » : cliquez sur « Ajouter ».", "Waterfox offers to add \"RDM\": click \"Add\".")
            } else {
                tr!(
                    "Ouvrez ce fichier avec Waterfox (ou glissez-le dans sa fenêtre), puis cliquez sur « Ajouter » :",
                    "Open this file with Waterfox (or drop it into its window), then click \"Add\":"
                )
            };
            step(ui, p, 1, first);
            if !done.launched {
                path_row(ui, p, xpi, copied);
            }
            step(ui, p, 2, connects_itself());
            note(
                ui,
                p,
                tr!(
                    "Installée pour de bon. Après une mise à jour de RDM, réinstallez-la ici pour passer à sa nouvelle version.",
                    "Installed for good. After an RDM update, reinstall it here to get its new version."
                ),
            );
            reopen = (xpi.to_string_lossy().into_owned(), reopen_package);
        }
        Flavour::Firefox => {
            let page = opened(tr!("about:debugging (« Ce Firefox »)", "about:debugging (\"This Firefox\")"));
            step(ui, p, 1, &trf!("{page} Cliquez sur « Charger un module complémentaire temporaire… ».", "{page} Click \"Load Temporary Add-on…\".", page = page));
            step(ui, p, 2, tr!("Choisissez le fichier manifest.json de ce dossier :", "Choose the manifest.json file of this folder:"));
            path_row(ui, p, &done.folder, copied);
            step(ui, p, 3, connects_itself());
            note(
                ui,
                p,
                tr!(
                    "Firefox n'installe durablement que les extensions signées par Mozilla : un module temporaire disparaît à la fermeture de Firefox. \
                     Firefox Developer Edition, Nightly, ESR, Waterfox et LibreWolf installent durablement ce fichier :",
                    "Firefox only keeps extensions signed by Mozilla: a temporary add-on goes away when Firefox closes. \
                     Firefox Developer Edition, Nightly, ESR, Waterfox and LibreWolf keep this file installed for good:"
                ),
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
    without_store
}

/// The extension reaches RDM through the connector RDM registered with the browser; only when the
/// browser cannot start it (a sandboxed Flatpak or Snap browser) does RDM ask for approval.
fn connects_itself() -> &'static str {
    tr!(
        "L'extension se connecte seule à RDM. Si RDM vous demande d'autoriser cette installation, cliquez sur « Autoriser ».",
        "The extension connects to RDM by itself. If RDM asks you to allow this installation, click \"Allow\"."
    )
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
                if widgets::icon_button(ui, icon::FOLDER_OPEN, tr!("Afficher dans le dossier", "Show in folder"), None).clicked() {
                    // Selects the item: a folder opens in its parent with it highlighted.
                    reveal_file(path.to_path_buf());
                }
                if widgets::icon_button(ui, icon::COPY, tr!("Copier le chemin", "Copy the path"), None).clicked() {
                    *copied = Some(text.clone());
                }
            });
        });
    });
    ui.add_space(6.0);
}

/// "3 min", "2 h", "4 d".
fn ago_text(secs: u64) -> String {
    match secs {
        0..60 => tr!("moins d'une minute", "less than a minute").to_owned(),
        60..3600 => format!("{} min", secs / 60),
        3600..86_400 => format!("{} h", secs / 3600),
        _ => trf!("{} j", "{} d", secs / 86_400),
    }
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
    painter.text(eframe::egui::pos2(x, rect.top() + 19.0), eframe::egui::Align2::LEFT_CENTER, tr!("Extension navigateur", "Browser extension"), theme::regular(12.0), p.muted);
    let names = |list: &[Browser]| match list {
        [] => String::new(),
        [one] => one.name().to_owned(),
        [one, rest @ ..] => format!("{} +{}", one.name(), rest.len()),
    };
    let value = match (connected.is_empty(), installed.is_empty()) {
        (false, _) => trf!("Connectée · {}", "Connected · {}", names(&connected)),
        (true, false) => trf!("Installée · {}", "Installed · {}", names(&installed)),
        (true, true) => tr!("À installer", "Not installed").to_owned(),
    };
    let galley = painter.layout_no_wrap(value, theme::semibold(13.0), p.text);
    let clip = eframe::egui::Rect::from_min_max(eframe::egui::pos2(x, rect.top()), eframe::egui::pos2(rect.right() - 8.0, rect.bottom()));
    painter.with_clip_rect(clip).galley(eframe::egui::pos2(x, rect.top() + 39.0 - galley.size().y / 2.0), galley, p.text);
    response.on_hover_text(tr!("Installer ou vérifier l'extension", "Install or check the extension")).clicked()
}

/// Browsers heard from recently (the extension checks in every few minutes while the browser
/// runs), and browsers heard from at all, for the sidebar card.
pub(super) fn connected(manager: &crate::manager::Manager) -> (Vec<Browser>, Vec<Browser>) {
    let now = unix_now();
    let mut seen = manager.browsers_seen();
    seen.sort_by_key(|(b, _)| b.name()); // a stable order in the sidebar
    let live = seen.iter().filter(|(_, t)| now.saturating_sub(*t) < LIVE_SECS).map(|(b, _)| *b).collect();
    (live, seen.into_iter().map(|(b, _)| b).collect())
}
