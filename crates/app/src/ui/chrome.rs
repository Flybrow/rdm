//! The frame around the list: sidebar (brand, filters, speed limit), header (add links, search)
//! and the dashboard (current speed, last minute's chart).

use eframe::egui::{
    Align, Align2, Color32, Context, Frame, Id, Image, Layout, Margin, Rect, RichText, Sense, SidePanel, Stroke,
    StrokeKind, TopBottomPanel, Ui, UiBuilder, Vec2, pos2, vec2,
};
use egui_phosphor::regular as icon;

use super::{
    ADD_ID, Action, App, Filter, NAV, SEARCH_ID, STATUS_FILTERS,
    theme::{self, Palette},
    widgets::{self, accent_button, caption, field, icon_button, speed},
};
use crate::{manager::Stats, settings::Theme, tr, trf, update};

impl App<'_> {
    pub(super) fn sidebar(&mut self, ctx: &Context, actions: &mut Vec<Action>) {
        let p = Palette::from_ctx(ctx);
        // One pass over the whole history per frame, whatever its length.
        let counts = self.manager.view(|es| {
            let mut counts = [0usize; NAV.len()];
            for e in es {
                for (n, (filter, _)) in counts.iter_mut().zip(NAV) {
                    *n += usize::from(filter.accepts(e));
                }
            }
            counts
        });
        let frame = Frame::new()
            .fill(p.sidebar)
            .inner_margin(Margin { left: 14, right: 14, top: 18, bottom: 16 })
            .stroke(Stroke::new(theme::HAIRLINE, p.border));
        let named = self.manager.with_settings(|s| s.queues.clone());
        // Named queues: shown once there is more than the main one.
        let queues: Vec<(Filter, usize)> = if named.is_empty() {
            Vec::new()
        } else {
            let ids: Vec<u32> = std::iter::once(0).chain(named.iter().map(|q| q.id)).collect();
            self.manager.view(|es| {
                ids.iter()
                    .map(|&q| (Filter::Queue(q), es.iter().filter(|e| Filter::Queue(q).accepts(e) && e.download.status() != &domain::Status::Completed).count()))
                    .collect()
            })
        };
        if matches!(self.memo.filter, Filter::Queue(q) if q != 0 && !named.iter().any(|x| x.id == q)) {
            self.memo.filter = Filter::All; // its queue was deleted
        }
        SidePanel::left("nav").exact_width(240.0).resizable(false).frame(frame).show(ctx, |ui| {
            self.brand(ui, &p);
            ui.add_space(24.0);
            // The filters scroll if the window is short; the cards below always stay visible.
            let update = self.manager.update_state();
            let reserved = if update.release().is_some() || update.busy() { 250.0 } else { 170.0 };
            let height = (ui.available_height() - reserved).max(120.0);
            eframe::egui::ScrollArea::vertical().max_height(height).auto_shrink([false, true]).show(ui, |ui| {
                for (i, ((filter, glyph), n)) in NAV.into_iter().zip(counts).enumerate() {
                    if i == STATUS_FILTERS {
                        section_caption(ui, tr!("CATÉGORIES", "CATEGORIES"));
                    }
                    if matches!(filter, Filter::Kind(_) | Filter::Failed) && n == 0 && self.memo.filter != filter {
                        continue;
                    }
                    let hue = match filter {
                        Filter::Kind(c) => Some(p.category(c)),
                        Filter::Failed => Some(p.danger),
                        _ => None,
                    };
                    if nav_item(ui, &p, glyph, hue, &filter.title_short(&named), n, self.memo.filter == filter) {
                        self.memo.filter = filter;
                    }
                }
                if !queues.is_empty() {
                    section_caption(ui, tr!("FILES D'ATTENTE", "QUEUES"));
                    for (filter, n) in queues {
                        if nav_item(ui, &p, icon::QUEUE, None, &filter.title(&named), n, self.memo.filter == filter) {
                            self.memo.filter = filter;
                        }
                    }
                }
            });
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.label(RichText::new(concat!("RDM ", env!("CARGO_PKG_VERSION"))).font(theme::regular(11.0)).color(p.faint));
                ui.add_space(6.0);
                if self.limit_card(ui, &p).clicked() {
                    actions.push(Action::OpenSettings);
                }
                ui.add_space(2.0);
                if super::browsers::card(ui, &p, super::browsers::connected(&self.manager)) {
                    actions.push(Action::OpenBrowsers);
                }
                if let Some(action) = self.update_card(ui, &p) {
                    actions.push(action);
                }
            });
        });
    }

    fn brand(&self, ui: &mut Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
            widgets::glow(ui.painter(), rect.center() + vec2(0.0, 4.0), Vec2::splat(34.0), p.accent2.gamma_multiply(0.35));
            Image::new(&self.logo).paint_at(ui, rect);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(RichText::new("RDM").font(theme::bold(19.0)).color(p.text));
                ui.add_space(-6.0);
                ui.label(RichText::new("Download Manager").font(theme::regular(11.5)).color(p.muted));
            });
        });
    }

    /// A newer release is out (or being downloaded): brand-gradient card with one button.
    fn update_card(&mut self, ui: &mut Ui, p: &Palette) -> Option<Action> {
        // (title, detail, clickable, moving, tooltip)
        let (title, detail, clickable, moving, tip) = match self.manager.update_state() {
            update::State::Available(r) => {
                let detail = if update::installs_itself(&r) { tr!("Cliquer pour installer", "Click to install") } else { tr!("Voir la nouvelle version", "See the new version") };
                let version = &r.version;
                (trf!("Version {version} disponible", "Version {version} available", version = version), detail.to_owned(), true, false, None)
            }
            update::State::Downloading(f) => {
                let pct = (f * 100.0) as u32;
                (tr!("Mise à jour…", "Updating…").to_owned(), trf!("téléchargement {pct} %", "downloading {pct} %", pct = pct), false, true, None)
            }
            update::State::Installing => (tr!("Installation…", "Installing…").to_owned(), tr!("RDM redémarre tout seul", "RDM restarts by itself").to_owned(), false, true, None),
            update::State::InstallFailed(r, reason) => {
                let version = &r.version;
                (trf!("Version {version} : échec", "Version {version}: failed", version = version), tr!("Cliquer pour réessayer", "Click to retry").to_owned(), true, false, Some(reason))
            }
            _ => return None,
        };
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 72.0), Sense::click());
        ui.add_space(8.0);
        let hover = ui.ctx().animate_bool_with_time(response.id, response.hovered(), 0.12);
        let painter = ui.painter();
        widgets::soft_shadow(painter, rect, 14, 0.6 + 0.4 * hover, p.dark);
        let lift = |c: Color32| c.lerp_to_gamma(Color32::WHITE, 0.08 * hover);
        painter.add(widgets::gradient(ui, rect, 14, lift(p.accent), lift(p.accent2), vec2(0.9, 0.4)));
        painter.text(pos2(rect.left() + 16.0, rect.top() + 22.0), Align2::LEFT_CENTER, icon::ROCKET_LAUNCH, theme::regular(18.0), Color32::WHITE);
        painter.text(pos2(rect.left() + 42.0, rect.top() + 22.0), Align2::LEFT_CENTER, title, theme::semibold(13.5), Color32::WHITE);
        painter.text(pos2(rect.left() + 42.0, rect.top() + 46.0), Align2::LEFT_CENTER, detail, theme::regular(12.0), Color32::from_white_alpha(210));
        if moving {
            self.animating = true;
        }
        let response = match tip {
            Some(reason) => response.on_hover_text(reason),
            None => response,
        };
        (response.clicked() && clickable).then_some(Action::InstallUpdate)
    }

    /// Current speed limit; opens the settings.
    fn limit_card(&self, ui: &mut Ui, p: &Palette) -> eframe::egui::Response {
        let limit = self.manager.with_settings(|s| s.speed_limit_kib);
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 58.0), Sense::click());
        let hover = ui.ctx().animate_bool_with_time(response.id, response.hovered(), 0.12);
        let painter = ui.painter();
        painter.rect(
            rect,
            14,
            p.surface.lerp_to_gamma(p.raised, hover),
            Stroke::new(theme::HAIRLINE, p.border),
            StrokeKind::Inside,
        );
        let tile = Rect::from_center_size(pos2(rect.left() + 28.0, rect.center().y), Vec2::splat(34.0));
        let color = if limit > 0 { p.warning } else { p.success };
        painter.rect_filled(tile, 10, p.tint(color, 0.16));
        painter.text(tile.center(), Align2::CENTER_CENTER, icon::GAUGE, theme::regular(18.0), color);
        painter.text(pos2(tile.right() + 12.0, rect.top() + 19.0), Align2::LEFT_CENTER, tr!("Limite de vitesse", "Speed limit"), theme::regular(12.0), p.muted);
        let value = if limit == 0 { tr!("Illimitée", "Unlimited").to_owned() } else { speed(f64::from(limit) * 1024.0) };
        painter.text(pos2(tile.right() + 12.0, rect.top() + 39.0), Align2::LEFT_CENTER, value, theme::semibold(14.0), p.text);
        response.on_hover_text(tr!("Modifier dans les paramètres", "Change in the settings"))
    }

    pub(super) fn header(&mut self, ctx: &Context) {
        TopBottomPanel::top("header")
            .exact_height(74.0)
            .frame(Frame::new().inner_margin(Margin { left: 26, right: 22, top: 18, bottom: 14 }))
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let right = 250.0 + 2.0 * 32.0 + 40.0;
                    let add_width = (ui.available_width() - right - 170.0).max(200.0);
                    let add = field(ui, Id::new(ADD_ID), &mut self.memo.url, tr!("Collez un ou plusieurs liens à télécharger…", "Paste one or more links to download…"), icon::LINK, add_width);
                    let enter = add.lost_focus() && ui.input(|i| i.key_pressed(eframe::egui::Key::Enter));
                    if (accent_button(ui, icon::DOWNLOAD_SIMPLE, tr!("Télécharger", "Download")).clicked() || enter) && !self.memo.url.trim().is_empty() {
                        let text = std::mem::take(&mut self.memo.url);
                        if self.submit(&text) == 0 {
                            self.memo.url = text;
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, icon::GEAR_SIX, tr!("Paramètres", "Settings"), None).clicked() {
                            self.open_settings();
                        }
                        let dark = ui.visuals().dark_mode;
                        let (glyph, tip, next) =
                            if dark { (icon::SUN, tr!("Thème clair", "Light theme"), Theme::Light) } else { (icon::MOON, tr!("Thème sombre", "Dark theme"), Theme::Dark) };
                        if icon_button(ui, glyph, tip, None).clicked() {
                            self.set_theme(ctx, next);
                        }
                        field(ui, Id::new(SEARCH_ID), &mut self.memo.search, tr!("Rechercher  (Ctrl+F)", "Search  (Ctrl+F)"), icon::MAGNIFYING_GLASS, 240.0);
                    });
                });
            });
    }

    pub(super) fn set_theme(&mut self, ctx: &Context, theme: Theme) {
        let mut settings = self.manager.settings();
        settings.theme = theme;
        ctx.set_theme(super::preference(theme));
        if let Some(draft) = &mut self.settings {
            draft.theme = theme;
        }
        self.manager.apply_settings(settings);
        self.manager.save_settings();
    }

    /// Current speed in large type, the last minute as a chart, what is running and waiting.
    pub(super) fn dashboard(&mut self, ui: &mut Ui, stats: Stats) {
        let p = Palette::of(ui);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 150.0), Sense::hover());
        let painter = ui.painter();
        widgets::soft_shadow(painter, rect, 22, 1.0, p.dark);
        let (from, to) = if p.dark {
            (p.surface.lerp_to_gamma(p.accent, 0.14), p.surface.lerp_to_gamma(p.accent2, 0.12))
        } else {
            (p.surface.lerp_to_gamma(p.accent, 0.06), p.surface.lerp_to_gamma(p.accent2, 0.07))
        };
        painter.add(widgets::gradient(ui, rect, 22, from, to, vec2(0.9, 0.44)));
        painter.rect_stroke(rect, 22, Stroke::new(theme::HAIRLINE, p.accent.gamma_multiply(0.28)), StrokeKind::Inside);

        let inner = rect.shrink2(vec2(28.0, 22.0));
        let left = Rect::from_min_size(inner.min, vec2(300.0, inner.height()));
        ui.scope_builder(UiBuilder::new().max_rect(left).layout(Layout::top_down(Align::Min)), |ui| {
            caption(ui, tr!("VITESSE DE TÉLÉCHARGEMENT", "DOWNLOAD SPEED"));
            ui.add_space(-2.0);
            let (value, unit) = split_speed(stats.speed);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(value).font(theme::bold(40.0)).color(p.text));
                ui.label(RichText::new(unit).font(theme::semibold(16.0)).color(p.muted));
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                stat(ui, &p, icon::ARROW_CIRCLE_DOWN, &crate::i18n::count(stats.running as u64, ("actif", "actifs"), ("active", "active")), p.accent);
                stat(ui, &p, icon::HOURGLASS, &trf!("{} en attente", "{} waiting", stats.queued), p.muted);
                if let Some(eta) = (stats.speed > 1.0 && stats.total > stats.done).then(|| ((stats.total - stats.done) as f64 / stats.speed) as u64) {
                    stat(ui, &p, icon::TIMER, &trf!("reste {}", "{} left", widgets::duration(eta)), p.muted);
                }
            });
        });

        let chart = Rect::from_min_max(pos2(left.right() + 30.0, inner.top() + 22.0), inner.max);
        let history = self.manager.speed_history();
        let peak = history.iter().copied().fold(0.0f32, f32::max);
        let painter = ui.painter();
        painter.text(pos2(chart.left(), inner.top() + 6.0), Align2::LEFT_CENTER, tr!("Dernière minute", "Last minute"), theme::semibold(11.5), p.muted);
        if peak > 0.0 {
            painter.text(
                pos2(chart.right(), inner.top() + 6.0),
                Align2::RIGHT_CENTER,
                trf!("pic {}", "peak {}", speed(f64::from(peak))),
                theme::regular(11.5),
                p.muted,
            );
        }
        widgets::area_chart(ui, chart, &history);
        ui.add_space(22.0);
    }
}

impl Filter {
    fn title_short(self, queues: &[crate::settings::Queue]) -> String {
        match self {
            Self::All => tr!("Tous", "All").to_owned(),
            other => other.title(queues),
        }
    }
}

/// A caption between groups of sidebar entries.
fn section_caption(ui: &mut Ui, text: &str) {
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        caption(ui, text);
    });
    ui.add_space(2.0);
}

/// `12,4` and `Mo/s`, set apart in the dashboard.
fn split_speed(bytes_per_sec: f64) -> (String, String) {
    let text = speed(bytes_per_sec);
    match text.split_once(' ') {
        Some((value, unit)) => (value.to_owned(), unit.to_owned()),
        None => (text, String::new()),
    }
}

fn stat(ui: &mut Ui, p: &Palette, glyph: &str, text: &str, color: Color32) {
    widgets::icon_text(ui, glyph, color, text, p.muted, 13.0);
}

/// A sidebar entry: icon, label, count; the selected one glows with the brand gradient.
fn nav_item(ui: &mut Ui, p: &Palette, glyph: &str, hue: Option<Color32>, label: &str, count: usize, selected: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
    let chosen = ui.ctx().animate_bool_with_time(response.id.with("selected"), selected, 0.18);
    let painter = ui.painter();
    if chosen > 0.0 {
        painter.add(widgets::gradient(
            ui,
            rect,
            11,
            p.accent.gamma_multiply(0.22 * chosen),
            p.accent2.gamma_multiply(0.10 * chosen),
            Vec2::X,
        ));
        let bar = Rect::from_center_size(pos2(rect.left() + 1.5, rect.center().y), vec2(3.0, 18.0 * chosen));
        painter.add(widgets::gradient(ui, bar, 2, p.accent, p.accent2, Vec2::Y));
    }
    if hover > 0.0 && chosen < 1.0 {
        painter.rect_filled(rect, 11, p.raised.gamma_multiply(hover * (1.0 - chosen)));
    }
    let icon_color = if selected { p.accent } else { hue.unwrap_or(p.muted) };
    painter.text(pos2(rect.left() + 20.0, rect.center().y), Align2::CENTER_CENTER, glyph, theme::regular(17.0), icon_color);
    let (font, color) = if selected { (theme::semibold(13.5), p.text) } else { (theme::regular(13.5), p.text.lerp_to_gamma(p.muted, 0.35)) };
    painter.text(pos2(rect.left() + 40.0, rect.center().y), Align2::LEFT_CENTER, label, font, color);
    if count > 0 {
        let galley = painter.layout_no_wrap(count.to_string(), theme::semibold(11.0), if selected { p.accent } else { p.muted });
        let badge = Rect::from_center_size(
            pos2(rect.right() - 12.0 - galley.size().x / 2.0 - 4.0, rect.center().y),
            vec2(galley.size().x + 14.0, 20.0),
        );
        painter.rect_filled(badge, 10, if selected { p.accent.gamma_multiply(0.16) } else { p.raised });
        painter.galley(badge.center() - galley.size() / 2.0, galley, p.muted);
    }
    response.clicked()
}
