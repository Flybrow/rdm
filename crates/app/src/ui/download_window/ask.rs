//! The first view of a download window: what the browser sent and where it goes, until the user's
//! go-ahead. Kept to the essentials — the name and the folder — the rest of this download's settings
//! (connections, speed, queue, checksum) folded under "More options".

use std::path::{Path, PathBuf};

use domain::{Category, Checksum, MAX_CONNECTIONS};
use eframe::egui::{ComboBox, DragValue, Key, Margin, RichText, Slider, TextEdit, Ui};
use egui_phosphor::regular as icon;
use url::Url;

use super::{Heading, Next, buttons, fold, header, label_cell};
use crate::{
    i18n::bytes,
    manager::{Choice, Inspection, Manager, Ticket},
    settings::{ExistingFile, Queue},
    tr, trf,
    ui::{
        category_icon, queue_name,
        theme::{self, Palette},
        widgets::{accent_button, ghost_button, segmented},
    },
};

/// The labels' column, before the fields.
const LABEL: f32 = 110.0;

pub(in crate::ui) struct Form {
    pub(super) ticket: Ticket,
    url: Url,
    /// A video with its separate audio track.
    merged: bool,
    inspection: Inspection,
    name: String,
    /// The user typed a name: the server's no longer replaces it.
    name_edited: bool,
    /// The page gave the name: it wins over the server's.
    named_by_page: bool,
    category: Category,
    dir: String,
    /// The user chose the folder: changing the name's type no longer moves it.
    dir_edited: bool,
    /// Make `dir` the folder of `category` from now on.
    remember: bool,
    /// "More options" unfolded.
    more: bool,
    connections: u8,
    /// Its own speed limit in MB/s (0 = none).
    mbps: f64,
    queue: u32,
    queues: Vec<Queue>,
    checksum: String,
    /// For a file of that name already in the folder: keep both, or replace it.
    existing: ExistingFile,
    /// The last path looked up on the disk, and whether a file is there (not at every frame).
    looked_up: Option<(PathBuf, bool)>,
    /// Stop asking for the next downloads.
    always: bool,
    error: Option<&'static str>,
}

impl Form {
    pub(super) fn new(ticket: Ticket, manager: &Manager) -> Option<Self> {
        let asking = manager.asking(ticket)?;
        let settings = manager.settings();
        let named_by_page = asking.given_name.is_some();
        let name = asking.given_name.unwrap_or_else(|| engine::suggest_file_name(&asking.url, None));
        let category = Category::of(&name);
        let mut form = Self {
            ticket,
            url: asking.url,
            merged: asking.merged,
            inspection: Inspection::Running,
            dir: settings.category_dir(category).display().to_string(),
            name,
            name_edited: false,
            named_by_page,
            category,
            dir_edited: false,
            remember: false,
            more: false,
            connections: settings.connections,
            mbps: 0.0,
            queue: 0,
            queues: settings.queues.clone(),
            checksum: String::new(),
            existing: ExistingFile::Rename,
            looked_up: None,
            always: false,
            error: None,
        };
        form.inspected(asking.inspection, manager);
        Some(form)
    }

    /// The server answered: its name for the file (unless the page or the user gave one), and the
    /// folder of that name's type (unless the user chose one).
    fn inspected(&mut self, inspection: Inspection, manager: &Manager) {
        if let Inspection::Done(found) = &inspection
            && !self.name_edited
            && !self.named_by_page
            && found.name != self.name
        {
            self.name.clone_from(&found.name);
            self.name_changed(manager);
        }
        self.inspection = inspection;
    }

    /// A new name may be another type of file, which goes to another folder.
    fn name_changed(&mut self, manager: &Manager) {
        let category = Category::of(self.name.trim());
        if category != self.category && !self.dir_edited {
            self.category = category;
            self.dir = manager.with_settings(|s| s.category_dir(category)).display().to_string();
            self.remember = false;
        }
    }

    /// Under the name in the header: the site, the size, whether it resumes.
    fn summary(&self) -> String {
        let mut parts = vec![self.url.host_str().unwrap_or_default().to_owned()];
        match &self.inspection {
            Inspection::Running => parts.push(tr!("taille : question au serveur…", "size: asking the server…").to_owned()),
            Inspection::Done(found) => {
                parts.push(found.size.map_or_else(|| tr!("taille inconnue", "unknown size").to_owned(), bytes));
                match found.resumable {
                    Some(true) => parts.push(tr!("reprise possible", "can resume").to_owned()),
                    Some(false) => parts.push(tr!("sans reprise", "cannot resume").to_owned()),
                    None => {}
                }
            }
            Inspection::Failed => parts.push(tr!("taille inconnue", "unknown size").to_owned()),
        }
        if self.merged {
            parts.push(tr!("vidéo + audio", "video + audio").to_owned());
        }
        parts.join(" · ")
    }

    pub(super) fn ui(&mut self, ui: &mut Ui, p: &Palette, manager: &std::sync::Arc<Manager>, closing: bool) -> Next {
        // Answered meanwhile (another window of RDM), or gone.
        let Some(asking) = manager.asking(self.ticket) else { return Next::Close };
        if asking.inspection != self.inspection {
            self.inspected(asking.inspection, manager);
        }
        if closing {
            manager.answer_window(self.ticket, None);
            return Next::Close;
        }
        let title = if self.name.trim().is_empty() { tr!("Nouveau téléchargement", "New download") } else { self.name.trim() };
        let busy = self.inspection == Inspection::Running;
        let summary = self.summary();
        let heading = Heading { glyph: category_icon(self.category), color: p.category(self.category), title, subtitle: &summary, busy, trailing: None };
        header(ui, p, heading);
        ui.add_space(18.0);
        self.where_to(ui, p, manager);
        let exists = self.exists();
        if exists {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(LABEL);
                ui.label(RichText::new(tr!("Un fichier de ce nom est déjà dans ce dossier.", "A file of this name is already in this folder.")).color(p.warning));
            });
            ui.horizontal(|ui| {
                ui.add_space(LABEL);
                segmented(
                    ui,
                    &mut self.existing,
                    &[
                        (ExistingFile::Rename, icon::COPY, tr!("Garder les deux", "Keep both")),
                        (ExistingFile::Overwrite, icon::ARROWS_CLOCKWISE, tr!("Remplacer", "Replace")),
                    ],
                );
            });
        }
        ui.add_space(10.0);
        if fold(ui, p, &mut self.more, tr!("Plus d'options", "More options")) {
            self.options(ui, p);
        }
        if let Some(error) = self.error {
            ui.add_space(6.0);
            ui.label(RichText::new(error).color(p.danger));
        }
        ui.add_space(16.0);
        let mut answer = None;
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.always, RichText::new(tr!("Ne plus demander", "Don't ask again")).font(theme::regular(12.5)).color(p.muted)).on_hover_text(tr!(
                "Les prochains téléchargements du navigateur démarreront sans cette question. Réactivable dans les paramètres.",
                "The next downloads from the browser will start without this question. Can be turned back on in the settings."
            ));
            buttons(ui, |ui| {
                if accent_button(ui, icon::DOWNLOAD_SIMPLE, tr!("Télécharger", "Download")).clicked() {
                    answer = Some(false);
                }
                if ghost_button(ui, icon::CLOCK, tr!("Plus tard", "Later"))
                    .on_hover_text(tr!("Ajouté à la liste en pause : à reprendre quand vous voulez", "Added to the list paused: resume it whenever you like"))
                    .clicked()
                {
                    answer = Some(true);
                }
                if ghost_button(ui, icon::X, tr!("Annuler", "Cancel")).clicked() {
                    cancel = true;
                }
            });
        });
        // Enter starts it, as the window's main button (not while a list is open).
        if answer.is_none() && ui.input(|i| i.key_pressed(Key::Enter)) && !ui.ctx().any_popup_open() {
            answer = Some(false);
        }
        if cancel {
            manager.answer_window(self.ticket, None);
            return Next::Close;
        }
        let Some(later) = answer else { return Next::Stay };
        let Some(choice) = self.choice(later, exists) else { return Next::Stay };
        let progress = manager.with_settings(|s| s.progress_window);
        match manager.answer_window(self.ticket, Some(choice)) {
            Some(id) if progress && !later => Next::Track(id),
            _ => Next::Close,
        }
    }

    /// Name; category and folder.
    fn where_to(&mut self, ui: &mut Ui, p: &Palette, manager: &Manager) {
        ui.horizontal(|ui| {
            label_cell(ui, p, tr!("Nom", "Name"), LABEL);
            let name = ui.add(TextEdit::singleline(&mut self.name).desired_width(f32::INFINITY).margin(Margin::symmetric(10, 6)).font(theme::regular(13.5)));
            if name.changed() {
                self.name_edited = true;
                self.error = None;
                self.name_changed(manager);
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            label_cell(ui, p, tr!("Dossier", "Folder"), LABEL);
            let mut chosen = self.category;
            ComboBox::from_id_salt("category")
                .width(140.0)
                .selected_text(format!("{}  {}", category_icon(chosen), crate::i18n::category(chosen)))
                .show_ui(ui, |ui| {
                    for c in Category::ALL {
                        ui.selectable_value(&mut chosen, c, format!("{}  {}", category_icon(c), crate::i18n::category(c)));
                    }
                })
                .response
                .on_hover_text(tr!("Catégorie : chacune a son dossier", "Category: each has its own folder"));
            if chosen != self.category {
                self.category = chosen;
                self.dir = manager.with_settings(|s| s.category_dir(chosen)).display().to_string();
                self.dir_edited = false;
                self.remember = false;
            }
            let room = ui.available_width() - 44.0;
            let field = ui.add(TextEdit::singleline(&mut self.dir).desired_width(room).margin(Margin::symmetric(10, 6)).font(theme::regular(13.0)));
            if field.changed() {
                self.dir_edited = true;
                self.error = None;
            }
            if crate::ui::widgets::icon_button(ui, icon::FOLDER_OPEN, tr!("Parcourir", "Browse"), None).clicked()
                && let Some(dir) = rfd::FileDialog::new().set_directory(self.dir.trim()).pick_folder()
            {
                self.dir = dir.display().to_string();
                self.dir_edited = true;
                self.error = None;
            }
        });
        let default = manager.with_settings(|s| s.category_dir(self.category));
        if Path::new(self.dir.trim()) != default && !self.dir.trim().is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(LABEL);
                let category = crate::i18n::category(self.category);
                let text = trf!("Toujours utiliser ce dossier pour : {category}", "Always use this folder for: {category}", category = category);
                ui.checkbox(&mut self.remember, RichText::new(text).font(theme::regular(12.5)).color(p.muted));
            });
        }
    }

    /// This download's own connections, speed limit, queue and checksum.
    fn options(&mut self, ui: &mut Ui, p: &Palette) {
        ui.add_space(6.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.horizontal(|ui| {
            label_cell(ui, p, tr!("Connexions", "Connections"), LABEL);
            ui.add(Slider::new(&mut self.connections, 1..=MAX_CONNECTIONS));
        });
        ui.horizontal(|ui| {
            label_cell(ui, p, tr!("Vitesse max.", "Max speed"), LABEL);
            ui.add(DragValue::new(&mut self.mbps).range(0.0..=10_000.0).speed(0.05).max_decimals(2).suffix(tr!(" Mo/s", " MB/s")));
            if self.mbps <= 0.0 {
                ui.label(RichText::new(tr!("aucune limite (0)", "no limit (0)")).color(p.muted));
            }
        });
        if !self.queues.is_empty() {
            ui.horizontal(|ui| {
                label_cell(ui, p, tr!("File d'attente", "Queue"), LABEL);
                let main = tr!("File principale", "Main queue");
                let shown = if self.queue == 0 { main.to_owned() } else { queue_name(&self.queues, self.queue) };
                ComboBox::from_id_salt("queue").width(180.0).selected_text(shown).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.queue, 0, main);
                    for q in &self.queues {
                        ui.selectable_value(&mut self.queue, q.id, queue_name(&self.queues, q.id));
                    }
                });
            });
        }
        ui.horizontal(|ui| {
            label_cell(ui, p, tr!("Empreinte", "Checksum"), LABEL);
            let hint = tr!("MD5, SHA-1, SHA-256 ou SHA-512 (facultatif)", "MD5, SHA-1, SHA-256 or SHA-512 (optional)");
            let field = ui.add(
                TextEdit::singleline(&mut self.checksum)
                    .hint_text(RichText::new(hint).color(p.faint))
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 6))
                    .font(eframe::egui::TextStyle::Monospace),
            );
            if field.changed() {
                self.error = None;
            }
        });
    }

    /// A file of the chosen name is already in the chosen folder (looked up when either changes).
    fn exists(&mut self) -> bool {
        let name = self.name.trim();
        if name.is_empty() || self.dir.trim().is_empty() {
            return false;
        }
        let path = Path::new(self.dir.trim()).join(engine::sanitize_file_name(name));
        match &self.looked_up {
            Some((seen, exists)) if *seen == path => *exists,
            _ => {
                let exists = path.is_file();
                self.looked_up = Some((path, exists));
                exists
            }
        }
    }

    /// The user's answer, once the form makes sense (else what is wrong is said).
    fn choice(&mut self, later: bool, exists: bool) -> Option<Choice> {
        let name = self.name.trim();
        if name.is_empty() {
            self.error = Some(tr!("Donnez un nom au fichier.", "Give the file a name."));
            return None;
        }
        let dir = PathBuf::from(self.dir.trim());
        if !dir.is_absolute() {
            self.error = Some(tr!("Choisissez un dossier (son chemin complet).", "Choose a folder (its full path)."));
            return None;
        }
        let checksum = match self.checksum.trim() {
            "" => None,
            text => match Checksum::parse(text) {
                Some(checksum) => Some(checksum),
                None => {
                    self.more = true;
                    self.error = Some(tr!(
                        "Empreinte non reconnue (MD5, SHA-1, SHA-256 ou SHA-512 en hexadécimal).",
                        "Unrecognised checksum (MD5, SHA-1, SHA-256 or SHA-512, in hexadecimal)."
                    ));
                    return None;
                }
            },
        };
        Some(Choice {
            name: name.to_owned(),
            dir,
            connections: self.connections,
            speed_limit_kib: (self.mbps.max(0.0) * 1024.0).round() as u32,
            queue: self.queue,
            checksum,
            later,
            existing: exists.then_some(self.existing),
            remember: self.remember.then_some(self.category),
            always: self.always,
        })
    }
}
