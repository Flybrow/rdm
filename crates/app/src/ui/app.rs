//! One window's interface: its state, its frame (sidebar, header, list, dialogs, toasts), and when
//! it draws the next one.

use std::{sync::Arc, time::{Duration, Instant}};

use domain::DownloadId;
use eframe::egui::{self, CentralPanel, Context, Frame, Key, LayerId, Margin, TextureHandle, vec2};
use egui_phosphor::regular as icon;
use url::Url;

use super::{DownloadWindows, Memo, browsers, desktop::open_link, dialogs, edit, theme, toast::Toasts, tray_summary, widgets};
use crate::{
    manager::{AddRequest, Manager},
    settings::Settings,
    shell::Shell,
    tr, trf,
    tray::Tray,
    window::Window,
};
use theme::Palette;

/// Frame rate while something moves (bars, spinners, toasts); nothing is redrawn when idle.
const ANIMATION_FRAME: Duration = Duration::from_millis(33);
/// Frame rate while only progress changes: 15 per second with the window in front, 2 behind it
/// (numbers still move; the processor and the battery are spared).
const PROGRESS_FRAME: Duration = Duration::from_millis(66);
const BACKGROUND_FRAME: Duration = Duration::from_millis(500);
/// Settings are written once the user stops fiddling (sliders fire every frame).
const SAVE_DEBOUNCE: Duration = Duration::from_millis(600);
/// Frame pacing when rendering without vsync (see `super::self_paced`): one frame per 60 Hz refresh.
const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

pub(super) const SEARCH_ID: &str = "search";
pub(super) const ADD_ID: &str = "add-url";

pub(super) struct App<'a> {
    pub(super) window: Window,
    /// The theme the title bar was last painted for.
    pub(super) title_bar_dark: Option<bool>,
    pub(super) manager: Arc<Manager>,
    pub(super) shell: Arc<Shell>,
    pub(super) tray: Option<&'a mut Tray>,
    pub(super) memo: &'a mut Memo,
    pub(super) logo: TextureHandle,
    /// Settings dialog draft, while open.
    pub(super) settings: Option<Settings>,
    /// Passwords edited in the settings (proxy, site logins), while open.
    pub(super) secrets: Option<crate::secrets::Secrets>,
    pub(super) unsaved_since: Option<Instant>,
    /// Reveal the VirusTotal key in the settings.
    pub(super) show_key: bool,
    /// The settings tab shown (the last one used, while RDM runs).
    pub(super) settings_tab: dialogs::SettingsTab,
    /// The settings were opened to ask for the VirusTotal key.
    pub(super) asking_key: bool,
    /// The browser-extension window, while open.
    pub(super) browsers: Option<browsers::Browsers>,
    /// Download whose VirusTotal report is open.
    pub(super) report: Option<DownloadId>,
    /// The small editor open on one download, if any.
    pub(super) edit: Option<edit::Editor>,
    /// "Delete the file" asked: waiting for the user's confirmation.
    pub(super) confirm_delete: Option<DownloadId>,
    /// Browser downloads in windows of their own (children of this window while it is open).
    pub(super) windows: DownloadWindows,
    pub(super) toasts: Toasts,
    /// Something on screen moves this frame (progress, spinner): keep redrawing.
    pub(super) animating: bool,
    /// Frames still to draw right away after a dialog or menu closed.
    pub(super) settle_frames: u8,
    /// The language the fonts were installed for (see `theme::install_fonts`).
    pub(super) fonts_language: crate::i18n::Language,
    /// Rendering without vsync: frames are spaced out by `pace`.
    pub(super) paced: bool,
    pub(super) last_frame: Option<Instant>,
}

impl<'a> App<'a> {
    pub(super) fn new(
        manager: Arc<Manager>,
        shell: Arc<Shell>,
        tray: Option<&'a mut Tray>,
        memo: &'a mut Memo,
        logo: TextureHandle,
        paced: bool,
        window: Window,
    ) -> Self {
        let mut app = Self {
            window,
            title_bar_dark: None,
            manager,
            shell,
            tray,
            memo,
            logo,
            settings: None,
            secrets: None,
            unsaved_since: None,
            show_key: false,
            settings_tab: dialogs::SettingsTab::default(),
            asking_key: false,
            report: None,
            browsers: None,
            edit: None,
            confirm_delete: None,
            windows: DownloadWindows::default(),
            toasts: Toasts::default(),
            animating: false,
            settle_frames: 0,
            fonts_language: crate::i18n::active(),
            paced,
            last_frame: None,
        };
        app.offer_extension();
        app
    }

    /// First launch without the extension anywhere: the extension window opens by itself, once.
    fn offer_extension(&mut self) {
        if self.manager.with_settings(|s| s.extension_offered) {
            return;
        }
        let mut settings = self.manager.settings();
        settings.extension_offered = true;
        self.manager.apply_settings(settings);
        self.manager.save_settings();
        if self.manager.browsers_seen().is_empty() {
            self.open_browsers();
        }
    }

    /// What vsync does, minus its hang (see `super::self_paced`): without it, moving the mouse would
    /// redraw at the mouse's polling rate, up to 1000 frames per second.
    fn pace(&mut self) {
        if !self.paced {
            return;
        }
        if let Some(wait) = self.last_frame.and_then(|t| FRAME_INTERVAL.checked_sub(t.elapsed())) {
            std::thread::sleep(wait);
        }
        self.last_frame = Some(Instant::now());
    }

    /// Adds every http(s) link found in `text`; tells how many.
    pub(super) fn submit(&mut self, text: &str) -> usize {
        let urls: Vec<Url> = text
            .split_whitespace()
            .filter_map(|t| t.parse::<Url>().ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .collect();
        let n = urls.len();
        urls.into_iter().for_each(|u| self.manager.add(AddRequest::from_url(u)));
        match n {
            0 => self.toasts.warn(icon::LINK_BREAK, tr!("Aucun lien http(s) à télécharger", "No http(s) link to download")),
            1 => self.toasts.info(icon::DOWNLOAD_SIMPLE, tr!("Téléchargement ajouté", "Download added")),
            n => self.toasts.info(icon::DOWNLOAD_SIMPLE, &trf!("{n} téléchargements ajoutés", "{n} downloads added", n = n)),
        }
        n
    }

    pub(super) fn open_settings(&mut self) {
        self.settings = Some(self.manager.settings());
        self.secrets = Some(self.manager.secrets());
    }

    /// Ctrl+V anywhere (no text field focused) adds the copied link(s), like IDM; Ctrl+F searches.
    fn shortcuts(&mut self, ctx: &Context) {
        if ctx.input(|i| i.modifiers.command && i.key_pressed(Key::F)) {
            ctx.memory_mut(|m| m.request_focus(egui::Id::new(SEARCH_ID)));
        }
        if ctx.memory(|m| m.focused().is_some()) || self.settings.is_some() {
            return;
        }
        let pasted: Vec<String> = ctx.input(|i| {
            i.events.iter().filter_map(|e| if let egui::Event::Paste(t) = e { Some(t.clone()) } else { None }).collect()
        });
        for text in pasted {
            self.submit(&text);
        }
    }

    /// Installs the update on offer, or opens its page when RDM cannot install it itself.
    pub(super) fn install_update(&mut self) {
        if !self.manager.install_update()
            && let Some(release) = self.manager.update_state().release()
        {
            open_link(release.page.clone());
        }
    }

    /// Saves once edits have settled; schedules a wake-up so it happens even if nothing moves.
    fn flush_settings(&mut self, ctx: &Context) {
        match self.unsaved_since {
            Some(t) if t.elapsed() >= SAVE_DEBOUNCE => {
                self.manager.save_settings();
                self.unsaved_since = None;
            }
            Some(t) => ctx.request_repaint_after(SAVE_DEBOUNCE.saturating_sub(t.elapsed())),
            None => {}
        }
    }
}

impl eframe::App for App<'_> {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        // The title bar follows the theme (chosen, or the system's when it changes).
        let dark = ctx.global_style().visuals.dark_mode;
        if self.title_bar_dark != Some(dark) {
            self.title_bar_dark = Some(dark);
            let p = Palette::from_ctx(ctx);
            self.window.paint_title_bar(dark, p.bg, p.text, p.border);
        }
        if self.fonts_language != crate::i18n::active() {
            self.fonts_language = crate::i18n::active();
            theme::install_fonts(ctx);
        }
        self.shortcuts(ctx);
        let stats = self.manager.stats();
        let mut actions = Vec::new();
        // A dialog or menu closing: two more frames at once, without it. egui matches clicks
        // against the previous frame, and keeps a dialog's modal layer one frame longer: otherwise
        // the next click would still hit the closed dialog and be lost.
        let overlays = (self.settings.is_some(), self.report.is_some(), self.browsers.is_some(), self.edit.is_some(), self.confirm_delete.is_some());
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.settle_frames = 2;
        }
        self.animating = false;

        for notice in self.manager.take_notices() {
            if notice.warning {
                self.toasts.warn(icon::WARNING, &notice.text);
            } else {
                self.toasts.info(icon::INFO, &notice.text);
            }
        }
        backdrop(ctx);
        self.sidebar(ui, &mut actions);
        self.header(ui);
        CentralPanel::default()
            .frame(Frame::new().inner_margin(Margin { left: 26, right: 26, top: 6, bottom: 0 }))
            .show(ui, |ui| {
                self.dashboard(ui, stats);
                self.clipboard_banner(ui);
                self.list(ui, &mut actions);
            });
        self.settings_dialog(ctx);
        self.report_dialog(ctx, &mut actions);
        // RDM started the extension's installation in a browser, and the user has steps to follow.
        if let Some(browser) = self.manager.take_extension_offer() {
            self.open_browsers_on(browser);
        }
        self.browsers_dialog(ctx);
        self.edit_dialog(ctx);
        self.delete_dialog(ctx);
        self.firefox_prompt(ctx);
        self.confirm_prompt(ctx);
        self.apply(ctx, actions);
        self.windows.show(ctx, &self.manager);
        if overlays != (self.settings.is_some(), self.report.is_some(), self.browsers.is_some(), self.edit.is_some(), self.confirm_delete.is_some()) {
            self.settle_frames = 2;
        }
        if self.settle_frames > 0 {
            self.settle_frames -= 1;
            ctx.request_repaint();
        }
        let toasts = self.toasts.show(ctx);

        if let Some(tray) = self.tray.as_deref_mut() {
            tray.relabel();
            tray.set_tooltip(tray_summary(stats));
        }
        self.flush_settings(ctx);
        // Animations at full rate; progress alone at a lower one, lower still behind other windows.
        if self.animating || toasts {
            ctx.request_repaint_after(ANIMATION_FRAME);
        } else if stats.running > 0 {
            let focused = ctx.input(|i| i.focused);
            ctx.request_repaint_after(if focused { PROGRESS_FRAME } else { BACKGROUND_FRAME });
        }
        self.memo.keep_geometry(ctx);
        self.pace();
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    /// The window is going away (quit, or closed to the tray): the manager lives on.
    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        self.shell.detach();
        self.manager.save_settings();
    }
}

/// Window background: the base colour and two soft lights of the brand gradient ("aurora").
fn backdrop(ctx: &Context) {
    let p = Palette::from_ctx(ctx);
    let screen = ctx.viewport_rect();
    let painter = ctx.layer_painter(LayerId::background());
    painter.rect_filled(screen, 0, p.bg);
    let strength = if p.dark { 0.13 } else { 0.10 };
    widgets::glow(&painter, screen.right_top() + vec2(-260.0, -40.0), vec2(620.0, 360.0), p.accent2.gamma_multiply(strength));
    widgets::glow(&painter, screen.left_top() + vec2(520.0, -80.0), vec2(560.0, 320.0), p.accent.gamma_multiply(strength));
}
