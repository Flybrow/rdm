//! The VirusTotal report dialog.

use super::*;

impl App<'_> {
    pub(in crate::ui) fn report_dialog(&mut self, ctx: &Context, actions: &mut Vec<Action>) {
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
        (icon::SHIELD_WARNING, trf!("{n} antivirus signalent ce fichier", "{n} antivirus engines flag this file", n = n))
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
                &trf!("Aucun des {n} antivirus n'a détecté de menace dans ce fichier.", "None of the {n} antivirus engines detected a threat in this file.", n = n),
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
