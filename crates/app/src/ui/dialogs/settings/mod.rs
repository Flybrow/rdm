//! The settings dialog: one tab at a time, each in its own file.

mod advanced;
mod browser;
mod downloads;
mod general;
mod security;

use super::*;
use advanced::advanced_tab;
use browser::browser_tab;
use downloads::downloads_tab;
use general::general_tab;
use security::security_tab;

/// The settings, one tab at a time: what everyone uses first, the expert settings last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(in crate::ui) enum SettingsTab {
    #[default]
    General,
    Downloads,
    Browser,
    Security,
    Advanced,
}

/// Puts the settings of `tab` back to their defaults (named queues, site logins and the
/// VirusTotal key are the user's own data: kept).
fn reset_tab(tab: SettingsTab, s: &mut Settings) {
    let d = Settings::default();
    match tab {
        SettingsTab::General => {
            (s.language, s.theme, s.autostart, s.close_to_tray, s.notify) = (d.language, d.theme, d.autostart, d.close_to_tray, d.notify);
        }
        SettingsTab::Downloads => {
            (s.download_dir, s.categorize, s.category_dirs, s.existing, s.speed_limit_kib) =
                (d.download_dir, d.categorize, d.category_dirs, d.existing, d.speed_limit_kib);
        }
        SettingsTab::Browser => {
            (s.captured, s.confirm_browser, s.progress_window, s.mini_button, s.offer_extension, s.clipboard) =
                (d.captured, d.confirm_browser, d.progress_window, d.mini_button, d.offer_extension, d.clipboard);
        }
        SettingsTab::Security => {}
        SettingsTab::Advanced => {
            (s.connections, s.max_parallel, s.proxy, s.check_updates, s.auto_update) = (d.connections, d.max_parallel, d.proxy, d.check_updates, d.auto_update);
        }
    }
}

/// The VirusTotal key's field: shown in clear, and focused (the user was asked for the key).
struct KeyField<'a> {
    shown: &'a mut bool,
    focus: bool,
}

/// What the settings form asks the window to do.
enum FormAction {
    Window(Action),
    TestProxy,
}

impl App<'_> {
    pub(in crate::ui) fn settings_dialog(&mut self, ctx: &Context) {
        let Some(mut draft) = self.settings.take() else { return };
        let mut secrets = self.secrets.take().unwrap_or_else(|| self.manager.secrets());
        let before = draft.clone();
        let p = Palette::from_ctx(ctx);
        let asking_key = std::mem::take(&mut self.asking_key);
        let mut show_key = self.show_key;
        // "Set up VirusTotal" (from a download) opens the tab where the key goes.
        if asking_key {
            self.settings_tab = SettingsTab::Security;
        }
        let mut tab = self.settings_tab;
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
            widgets::tab_bar(
                ui,
                &mut tab,
                &[
                    (SettingsTab::General, icon::SLIDERS_HORIZONTAL, tr!("Général", "General")),
                    (SettingsTab::Downloads, icon::DOWNLOAD_SIMPLE, tr!("Téléchargements", "Downloads")),
                    (SettingsTab::Browser, icon::BROWSERS, tr!("Navigateur", "Browser")),
                    (SettingsTab::Security, icon::SHIELD_CHECK, tr!("Sécurité", "Security")),
                    (SettingsTab::Advanced, icon::WRENCH, tr!("Avancé", "Advanced")),
                ],
            );
            ui.add_space(14.0);
            // As tall as the window allows; without the minimum, the area would keep the height
            // available when the dialog first appeared (from the screen's middle down).
            let height = (ctx.content_rect().height() - 270.0).max(240.0);
            let update_state = self.manager.update_state();
            let action = ScrollArea::vertical()
                .id_salt(tab) // each tab keeps its own scroll position
                .min_scrolled_height(height)
                .max_height(height)
                .show(ui, |ui| settings_form(ui, &p, &mut draft, &mut secrets, KeyField { shown: &mut show_key, focus: asking_key }, &update_state, tab))
                .inner;
            (close, action)
        });
        self.settings_tab = tab;
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
            ctx.set_theme(crate::ui::preference(draft.theme));
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
}

fn settings_form(
    ui: &mut Ui,
    p: &Palette,
    s: &mut Settings,
    secrets: &mut Secrets,
    key: KeyField<'_>,
    update_state: &update::State,
    tab: SettingsTab,
) -> Option<FormAction> {
    let action = match tab {
        SettingsTab::General => {
            general_tab(ui, p, s);
            None
        }
        SettingsTab::Downloads => {
            downloads_tab(ui, p, s);
            None
        }
        SettingsTab::Browser => browser_tab(ui, p, s),
        SettingsTab::Security => {
            security_tab(ui, p, secrets, key);
            None
        }
        SettingsTab::Advanced => advanced_tab(ui, p, s, secrets, update_state),
    };

    // Everything but the passwords and the VirusTotal key (the Security tab) can go back to its default.
    if tab != SettingsTab::Security
        && ghost_button(ui, icon::ARROW_COUNTER_CLOCKWISE, tr!("Rétablir les réglages par défaut de cet onglet", "Restore this tab's defaults")).clicked()
    {
        reset_tab(tab, s);
    }
    ui.add_space(8.0);

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
