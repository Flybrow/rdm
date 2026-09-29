//! The settings dialog.

use super::*;

/// Named queues the settings let the user create (besides the main one).
const MAX_QUEUES: usize = 12;
const MAX_LOGINS: usize = 64;

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
        SettingsTab::Browser => (s.captured, s.confirm_browser, s.clipboard) = (d.captured, d.confirm_browser, d.clipboard),
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
            let height = (ctx.screen_rect().height() - 270.0).max(240.0);
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
            security_tab(ui, p, s, secrets, key);
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

/// "General": language, theme, system integration.
fn general_tab(ui: &mut Ui, p: &Palette, s: &mut Settings) {
    section(ui, p, icon::TRANSLATE, tr!("Langue et apparence", "Language and appearance"), |ui| {
        language_menu(ui, &mut s.language);
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
}

/// "Downloads": folders, sorting, existing files, speed limit, queues.
fn downloads_tab(ui: &mut Ui, p: &Palette, s: &mut Settings) {
    section(ui, p, icon::FOLDER, tr!("Dossier de téléchargement", "Download folder"), |ui| {
        note(ui, p, tr!("Là où vont vos fichiers.", "Where your files go."));
        ui.add_space(4.0);
        if let Some(Some(dir)) = folder_row(ui, p, icon::HARD_DRIVES, tr!("Dossier principal", "Main folder"), &s.download_dir, false) {
            s.download_dir = dir;
        }
    });

    section(ui, p, icon::FOLDERS, tr!("Classement par type", "Sorting by type"), |ui| {
        toggle(
            ui,
            &mut s.categorize,
            tr!("Ranger chaque type dans son sous-dossier", "Sort each type into its own sub-folder"),
            tr!("Vidéos, Musique, Compressés… dans le dossier principal", "Videos, Music, Archives… inside the main folder"),
        );
        ui.add_space(4.0);
        for category in Category::ALL {
            let shown = s.category_dir(category);
            match folder_row(ui, p, category_icon(category), crate::i18n::category(category), &shown, s.category_dirs.contains_key(&category)) {
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

    section(ui, p, icon::COPY, tr!("Si le fichier existe déjà", "If the file already exists"), |ui| {
        segmented(
            ui,
            &mut s.existing,
            &[
                (ExistingFile::Ask, icon::CHAT_CIRCLE, tr!("Demander", "Ask")),
                (ExistingFile::Rename, icon::COPY, tr!("Renommer", "Rename")),
                (ExistingFile::Overwrite, icon::ARROWS_CLOCKWISE, tr!("Remplacer", "Overwrite")),
                (ExistingFile::Skip, icon::PROHIBIT, tr!("Ignorer", "Skip")),
            ],
        );
        note(
            ui,
            p,
            match s.existing {
                ExistingFile::Ask => tr!(
                    "RDM passe au premier plan et vous laisse choisir : garder les deux fichiers, ou remplacer l'ancien.",
                    "RDM comes to the front and lets you choose: keep both files, or replace the old one."
                ),
                ExistingFile::Rename => tr!("Le nouveau fichier devient « nom (1).ext ».", "The new file becomes \"name (1).ext\"."),
                ExistingFile::Overwrite => tr!("L'ancien fichier est remplacé par le nouveau.", "The old file is replaced by the new one."),
                ExistingFile::Skip => tr!("Le lien n'est pas téléchargé de nouveau.", "The link is not downloaded again."),
            },
        );
        note(
            ui,
            p,
            tr!(
                "S'applique à tous les téléchargements : du navigateur, des liens collés ou copiés.",
                "Applies to every download: from the browser, from pasted or copied links."
            ),
        );
    });

    section(ui, p, icon::GAUGE, tr!("Limite de vitesse", "Speed limit"), |ui| {
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
                "0 = aucune limite. Pratique pour garder du débit pour naviguer ou pour les appels vidéo.",
                "0 = no limit. Useful to keep bandwidth for browsing or video calls."
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
            s.queues.push(Queue { id, name: trf!("File {n}", "Queue {n}", n = n), max_parallel: 1 });
        }
    });
}

/// "Browser": extension, captured types, clipboard.
fn browser_tab(ui: &mut Ui, p: &Palette, s: &mut Settings) -> Option<FormAction> {
    let mut action = None;
    section(ui, p, icon::PUZZLE_PIECE, tr!("Extension du navigateur", "Browser extension"), |ui| {
        note(
            ui,
            p,
            tr!(
                "L'extension envoie à RDM les téléchargements de votre navigateur.",
                "The extension hands your browser's downloads over to RDM."
            ),
        );
        ui.add_space(8.0);
        if accent_button(ui, icon::PUZZLE_PIECE, tr!("Installer l'extension…", "Install the extension…")).clicked() {
            action = Some(FormAction::Window(Action::OpenBrowsers));
        }
        ui.add_space(8.0);
        toggle(
            ui,
            &mut s.confirm_browser,
            tr!("Confirmer les téléchargements du navigateur", "Confirm downloads from the browser"),
            tr!("RDM passe au premier plan et attend votre accord", "RDM comes to the front and waits for your go-ahead"),
        );
        if !s.confirm_browser {
            ui.add_space(4.0);
            unconfirmed_warning(ui, p);
        }
    });

    section(ui, p, icon::FILE_ARROW_DOWN, tr!("Types de fichiers capturés", "Captured file types"), |ui| {
        ui.add(TextEdit::multiline(&mut s.captured).desired_rows(3).desired_width(f32::INFINITY).font(eframe::egui::TextStyle::Monospace));
        note(
            ui,
            p,
            tr!(
                "Extensions séparées par des espaces. Les archives découpées (.r00, .001…) sont toujours capturées.",
                "Extensions separated by spaces. Split archives (.r00, .001…) are always captured."
            ),
        );
        ui.add_space(4.0);
        if ghost_button(ui, icon::ARROW_COUNTER_CLOCKWISE, tr!("Liste par défaut", "Default list")).clicked() {
            s.captured = domain::default_captured();
        }
    });

    section(ui, p, icon::CLIPBOARD_TEXT, tr!("Liens copiés dans le presse-papiers", "Links copied to the clipboard"), |ui| {
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
    action
}

/// "Security": site logins and the VirusTotal key.
fn security_tab(ui: &mut Ui, p: &Palette, s: &mut Settings, secrets: &mut Secrets, key: KeyField<'_>) {
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
            let field = ui.add(
                TextEdit::singleline(&mut s.virustotal_key)
                    .password(!*key.shown)
                    .hint_text(RichText::new(tr!("Clé API VirusTotal (64 caractères)", "VirusTotal API key (64 characters)")).color(p.faint))
                    .desired_width(ui.available_width() - 90.0)
                    .margin(Margin::symmetric(12, 9))
                    .font(eframe::egui::TextStyle::Monospace),
            );
            if key.focus {
                field.request_focus();
                field.scroll_to_me(Some(Align::Center));
            }
            let (glyph, tip) = if *key.shown { (icon::EYE_SLASH, tr!("Masquer la clé", "Hide the key")) } else { (icon::EYE, tr!("Afficher la clé", "Show the key")) };
            if icon_button(ui, glyph, tip, None).clicked() {
                *key.shown = !*key.shown;
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
}

/// "Advanced": performance, proxy, updates.
fn advanced_tab(ui: &mut Ui, p: &Palette, s: &mut Settings, secrets: &mut Secrets, update_state: &update::State) -> Option<FormAction> {
    let mut action = None;
    widgets::icon_text(
        ui,
        icon::WARNING,
        p.warning,
        tr!(
            "Les valeurs par défaut conviennent à la plupart des gens : ne les changez que si vous savez pourquoi.",
            "The default values suit most people: change these only if you know why."
        ),
        p.muted,
        12.5,
    );
    ui.add_space(10.0);
    section(ui, p, icon::LIGHTNING, tr!("Performances", "Performance"), |ui| {
        ui.add(Slider::new(&mut s.connections, 1..=MAX_CONNECTIONS).text(tr!("connexions par fichier (au plus)", "connections per file (at most)")));
        ui.add(Slider::new(&mut s.max_parallel, 1..=MAX_PARALLEL).text(tr!("téléchargements simultanés (file principale)", "simultaneous downloads (main queue)")));
        note(
            ui,
            p,
            tr!(
                "Plus de connexions peuvent accélérer un gros fichier ; certains sites en refusent trop.",
                "More connections can speed up a large file; some sites refuse too many."
            ),
        );
        note(
            ui,
            p,
            tr!(
                "RDM adapte seul le nombre de connexions à la connexion et au serveur : ce réglage n'est qu'un plafond.",
                "RDM adapts the number of connections to the network and the server by itself: this setting is only a ceiling."
            ),
        );
    });

    section(ui, p, icon::GLOBE_HEMISPHERE_WEST, tr!("Connexion à Internet (proxy)", "Internet connection (proxy)"), |ui| {
        note(
            ui,
            p,
            tr!(
                "Un proxy est un serveur intermédiaire par lequel passent les téléchargements (réseau d'entreprise, école…). Dans le doute, gardez « Système ».",
                "A proxy is a go-between server that downloads pass through (company or school network…). If unsure, keep \"System\"."
            ),
        );
        ui.add_space(8.0);
        segmented(
            ui,
            &mut s.proxy.mode,
            &[
                (ProxyMode::Off, icon::PLUGS_CONNECTED, tr!("Aucun", "None")),
                (ProxyMode::System, icon::DESKTOP, tr!("Système", "System")),
                (ProxyMode::Manual, icon::SHIELD, tr!("Toujours", "Always")),
                (ProxyMode::Auto, icon::MAGIC_WAND, tr!("En secours", "As a fallback")),
            ],
        );
        note(
            ui,
            p,
            match s.proxy.mode {
                ProxyMode::Off => tr!(
                    "Aucun proxy : RDM se connecte directement aux sites, même si le système en indique un.",
                    "No proxy: RDM connects to the sites directly, even if the system names one."
                ),
                ProxyMode::System => tr!(
                    "Recommandé. RDM utilise le proxy réglé dans le système, s'il y en a un ; sinon il se connecte directement.",
                    "Recommended. RDM uses the proxy set in the system, if there is one; otherwise it connects directly."
                ),
                ProxyMode::Manual => tr!(
                    "Tous les téléchargements passent par le proxy que vous indiquez ci-dessous.",
                    "Every download goes through the proxy you enter below."
                ),
                ProxyMode::Auto => tr!(
                    "Connexion directe d'abord ; si un téléchargement est bloqué ou très lent, RDM passe tout seul par le proxy ci-dessous.",
                    "Direct connection first; if a download is blocked or very slow, RDM switches to the proxy below by itself."
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
            if ghost_button(ui, icon::PLAY, tr!("Tester la connexion", "Test the connection")).clicked() {
                action = Some(FormAction::TestProxy);
            }
        }
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
                update::State::Idle => (icon::INFO, trf!("Version installée : {version}", "Installed version: {version}", version = version), p.muted),
                update::State::Checking => (icon::CIRCLE_NOTCH, tr!("Recherche en cours…", "Checking…").to_owned(), p.accent),
                update::State::UpToDate => (icon::CHECK_CIRCLE, trf!("RDM est à jour ({version})", "RDM is up to date ({version})", version = version), p.success),
                update::State::Available(r) => (icon::ROCKET_LAUNCH, trf!("Version {} disponible", "Version {} available", r.version), p.accent),
                update::State::Downloading(f) => {
                    let percent = (f * 100.0) as u32;
                    (icon::DOWNLOAD_SIMPLE, trf!("Téléchargement {percent} %", "Downloading {percent}%", percent = percent), p.accent)
                }
                update::State::Installing => (icon::ROCKET_LAUNCH, tr!("Installation : RDM redémarre tout seul", "Installing: RDM restarts by itself").to_owned(), p.accent),
                update::State::InstallFailed(r, reason) => (icon::WARNING, trf!("Version {} : {reason}", "Version {}: {reason}", r.version, reason = reason), p.danger),
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
