//! Atoms: small, stateless building blocks, painted to the design system (`theme`).

use std::f32::consts::TAU;

use eframe::egui::{
    Align, Align2, Color32, CornerRadius, Id, Layout, Margin, Mesh, Painter, Pos2, Rect, Response, RichText, Sense,
    Shadow, Shape, Stroke, TextEdit, TextFormat, Ui, UiBuilder, Vec2,
    epaint::{PathStroke, RectShape, TessellationOptions, Tessellator},
    pos2,
    text::LayoutJob,
    vec2,
};

use super::theme::{self, Palette};

// ── Numbers, in the interface language ───────────────────────────────────
pub fn bytes(n: u64) -> String {
    let units: [&str; 5] = if crate::i18n::french() { ["o", "Ko", "Mo", "Go", "To"] } else { ["B", "KB", "MB", "GB", "TB"] };
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < units.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} {}", units[0]) } else { format!("{} {}", decimal(v), units[unit]) }
}

pub fn speed(bytes_per_sec: f64) -> String {
    format!("{}/s", bytes(bytes_per_sec.max(0.0) as u64))
}

/// One decimal: `1,5` in French and most languages, `1.5` in English and East Asian ones.
fn decimal(v: f64) -> String {
    let text = format!("{v:.1}");
    if crate::i18n::decimal_comma() { text.replace('.', ",") } else { text }
}

pub fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min {:02} s", secs / 60, secs % 60),
        _ => format!("{} h {:02} min", secs / 3600, secs % 3600 / 60),
    }
}

// ── Painting helpers ─────────────────────────────────────────────────────
/// A rounded rectangle filled with a gradient along `dir` (a unit vector): tessellated like any
/// filled shape — anti-aliased rounded corners — then recoloured vertex by vertex.
pub fn gradient(ui: &Ui, rect: Rect, radius: impl Into<CornerRadius>, from: Color32, to: Color32, dir: Vec2) -> Shape {
    let mut tessellator = Tessellator::new(ui.ctx().pixels_per_point(), TessellationOptions::default(), [1, 1], Vec::new());
    let mut mesh = Mesh::default();
    tessellator.tessellate_rect(&RectShape::filled(rect, radius, Color32::WHITE), &mut mesh);
    let along = |p: Pos2| p.to_vec2().dot(dir);
    let corners = [rect.left_top(), rect.right_top(), rect.left_bottom(), rect.right_bottom()].map(along);
    let (lo, hi) = corners.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &c| (lo.min(c), hi.max(c)));
    let span = (hi - lo).max(f32::EPSILON);
    for v in &mut mesh.vertices {
        let t = ((along(v.pos) - lo) / span).clamp(0.0, 1.0);
        let coverage = f32::from(v.color.a()) / 255.0; // anti-aliasing feather
        v.color = from.lerp_to_gamma(to, t).gamma_multiply(coverage);
    }
    Shape::mesh(mesh)
}

/// Soft radial light (the "aurora" behind the dashboard, halos behind icons).
pub fn glow(painter: &Painter, center: Pos2, radius: Vec2, color: Color32) {
    const STEPS: u32 = 64;
    let mut mesh = Mesh::default();
    mesh.colored_vertex(center, color);
    for i in 0..=STEPS {
        let a = TAU * i as f32 / STEPS as f32;
        mesh.colored_vertex(center + vec2(a.cos() * radius.x, a.sin() * radius.y), Color32::TRANSPARENT);
    }
    for i in 1..=STEPS {
        mesh.add_triangle(0, i, i + 1);
    }
    painter.add(Shape::mesh(mesh));
}

pub fn soft_shadow(painter: &Painter, rect: Rect, radius: u8, strength: f32, dark: bool) {
    if strength <= 0.0 {
        return;
    }
    let alpha = if dark { 110.0 } else { 34.0 } * strength;
    let shadow = Shadow { offset: [0, 8], blur: 26, spread: 0, color: Color32::from_black_alpha(alpha as u8) };
    painter.add(shadow.as_shape(rect, CornerRadius::same(radius)));
}

// ── Progress ─────────────────────────────────────────────────────────────
/// Rounded bar: gradient fill, a light sweeping across while `moving`; `None` = unknown length
/// (a segment travelling back and forth).
pub fn progress(ui: &Ui, rect: Rect, fraction: Option<f32>, (from, to): (Color32, Color32), moving: bool) {
    let p = Palette::of(ui);
    let painter = ui.painter();
    let radius = CornerRadius::same((rect.height() / 2.0) as u8);
    painter.rect_filled(rect, radius, p.border);
    let time = ui.input(|i| i.time) as f32;
    let fill = match fraction {
        Some(f) if f > 0.0 => {
            let mut fill = rect;
            fill.set_width((rect.width() * f.clamp(0.0, 1.0)).max(rect.height()));
            fill
        }
        Some(_) => return,
        None => {
            let t = (time * 0.7).fract();
            let width = rect.width() * 0.28;
            let left = rect.left() - width + (rect.width() + width) * t;
            Rect::from_min_max(pos2(left.max(rect.left()), rect.top()), pos2((left + width).min(rect.right()), rect.bottom()))
        }
    };
    if fill.width() <= 0.0 {
        return;
    }
    painter.add(gradient(ui, fill, radius, from, to, Vec2::X));
    if moving && fraction.is_some() {
        // A soft highlight sweeping left to right, clipped to the filled part.
        let band = 90.0;
        let x = fill.left() - band + (fill.width() + band * 2.0) * (time * 0.55).fract();
        let clip = painter.with_clip_rect(fill);
        let shine = Color32::from_white_alpha(if p.dark { 70 } else { 110 });
        let half = |l: f32, a: Color32, b: Color32| gradient(ui, Rect::from_min_size(pos2(l, fill.top()), vec2(band / 2.0, fill.height())), 0, a, b, Vec2::X);
        clip.add(half(x, Color32::TRANSPARENT, shine));
        clip.add(half(x + band / 2.0, shine, Color32::TRANSPARENT));
    }
}

// ── Buttons ──────────────────────────────────────────────────────────────
fn hover_anim(ui: &Ui, response: &Response) -> f32 {
    ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered() || response.has_focus(), 0.12)
}

/// Round, frameless icon button; `tint` colours its hover halo and glyph.
pub fn icon_button(ui: &mut Ui, glyph: &str, tooltip: &str, tint: Option<Color32>) -> Response {
    let p = Palette::of(ui);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
    let hover = hover_anim(ui, &response);
    let tint = tint.unwrap_or(p.accent);
    let painter = ui.painter();
    if hover > 0.0 {
        painter.circle_filled(rect.center(), 16.0, tint.gamma_multiply(0.16 * hover));
    }
    let color = p.muted.lerp_to_gamma(if tint == p.accent { p.text } else { tint }, hover);
    painter.text(rect.center(), Align2::CENTER_CENTER, glyph, theme::regular(17.0), color);
    response.on_hover_text(tooltip)
}

/// The primary action: brand gradient, soft coloured glow.
pub fn accent_button(ui: &mut Ui, glyph: &str, label: &str) -> Response {
    let p = Palette::of(ui);
    let galley = ui.painter().layout_no_wrap(format!("{glyph}  {label}"), theme::semibold(14.0), Color32::WHITE);
    let (rect, response) = ui.allocate_exact_size(vec2(galley.size().x + 34.0, 38.0), Sense::click());
    let hover = hover_anim(ui, &response);
    let painter = ui.painter();
    let glow = Shadow { offset: [0, 6], blur: 20, spread: 0, color: p.accent.gamma_multiply(0.30 + 0.25 * hover) };
    painter.add(glow.as_shape(rect, CornerRadius::same(12)));
    let lift = |c: Color32| c.lerp_to_gamma(Color32::WHITE, 0.10 * hover);
    let pressed = if response.is_pointer_button_down_on() { 0.92 } else { 1.0 };
    painter.add(gradient(ui, rect, 12, lift(p.accent).gamma_multiply(pressed), lift(p.accent2).gamma_multiply(pressed), vec2(0.96, 0.28)));
    painter.galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
    response
}

/// Secondary action: quiet until hovered.
pub fn ghost_button(ui: &mut Ui, glyph: &str, label: &str) -> Response {
    let p = Palette::of(ui);
    let text = if label.is_empty() { glyph.to_owned() } else { format!("{glyph}  {label}") };
    let galley = ui.painter().layout_no_wrap(text, theme::semibold(13.0), p.text);
    let (rect, response) = ui.allocate_exact_size(vec2(galley.size().x + 26.0, 34.0), Sense::click());
    let hover = hover_anim(ui, &response);
    let painter = ui.painter();
    painter.rect(
        rect,
        10,
        p.surface.lerp_to_gamma(p.raised, hover),
        Stroke::new(theme::HAIRLINE, p.border.lerp_to_gamma(p.border_strong, hover)),
        eframe::egui::StrokeKind::Inside,
    );
    painter.galley(rect.center() - galley.size() / 2.0, galley, p.muted.lerp_to_gamma(p.text, 0.55 + 0.45 * hover));
    response
}

// ── Pills, labels ────────────────────────────────────────────────────────
/// Paints a status pill whose left edge is at `left`, vertically centred on `y`; returns its rect.
pub fn paint_pill(ui: &Ui, left: f32, y: f32, text: &str, color: Color32) -> Rect {
    let p = Palette::of(ui);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::semibold(11.0), color);
    let rect = Rect::from_min_size(pos2(left, y - 10.0), vec2(galley.size().x + 16.0, 20.0));
    ui.painter().rect_filled(rect, 10, p.tint(color, 0.16));
    ui.painter().galley(pos2(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0), galley, color);
    rect
}

/// Adds widgets inside `rect` (left-aligned, vertically centred) without moving the parent's
/// layout cursor: for cards and rows painted by hand. (`Ui::put` would centre, and advance it.)
pub fn place<R>(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    add(&mut ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center))))
}

/// An icon and its text, each in its own colour, on one baseline.
pub fn icon_text(ui: &mut Ui, glyph: &str, glyph_color: Color32, text: &str, text_color: Color32, size: f32) -> Response {
    let mut job = LayoutJob::default();
    let format = |font, color| TextFormat { font_id: font, color, valign: Align::Center, ..TextFormat::default() };
    job.append(glyph, 0.0, format(theme::regular(size + 2.0), glyph_color));
    job.append(text, 6.0, format(theme::regular(size), text_color));
    ui.label(job)
}

/// `CATEGORIES`-style section label.
pub fn caption(ui: &mut Ui, text: &str) {
    let p = Palette::of(ui);
    ui.label(RichText::new(text).font(theme::semibold(10.5)).color(p.faint).extra_letter_spacing(1.0));
}

/// A keyboard key.
pub fn kbd(ui: &mut Ui, key: &str) {
    let p = Palette::of(ui);
    let galley = ui.painter().layout_no_wrap(key.to_owned(), theme::semibold(12.0), p.text);
    let (rect, _) = ui.allocate_exact_size(vec2(galley.size().x + 16.0, 26.0), Sense::hover());
    let painter = ui.painter();
    painter.rect(rect, 7, p.surface, Stroke::new(theme::HAIRLINE, p.border_strong), eframe::egui::StrokeKind::Inside);
    painter.line_segment([rect.left_bottom() + vec2(3.0, -1.5), rect.right_bottom() + vec2(-3.0, -1.5)], Stroke::new(2.0_f32, p.border));
    painter.galley(rect.center() - galley.size() / 2.0 - vec2(0.0, 1.0), galley, p.text);
}

// ── Inputs ───────────────────────────────────────────────────────────────
/// Single-line field with a leading icon; `Ctrl+F`-style focus goes through `id`.
pub fn field(ui: &mut Ui, id: Id, text: &mut String, hint: &str, glyph: &str, width: f32) -> Response {
    let p = Palette::of(ui);
    let response = ui.add(
        TextEdit::singleline(text)
            .id(id)
            .hint_text(RichText::new(hint).color(p.faint))
            .desired_width(width)
            .margin(Margin { left: 38, right: 12, top: 10, bottom: 10 })
            .font(theme::regular(14.0)),
    );
    let color = if response.has_focus() { p.accent } else { p.muted };
    ui.painter().text(response.rect.left_center() + vec2(19.0, 0.0), Align2::CENTER_CENTER, glyph, theme::regular(17.0), color);
    response
}

/// Label on the left, iOS-style switch on the right; the whole row toggles.
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str, detail: &str) -> Response {
    let p = Palette::of(ui);
    let height = if detail.is_empty() { 34.0 } else { 48.0 };
    let (rect, mut response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(response.id, *on, 0.16);
    let painter = ui.painter();
    let text_x = rect.left();
    if detail.is_empty() {
        painter.text(pos2(text_x, rect.center().y), Align2::LEFT_CENTER, label, theme::regular(14.0), p.text);
    } else {
        painter.text(pos2(text_x, rect.top() + 14.0), Align2::LEFT_CENTER, label, theme::regular(14.0), p.text);
        painter.text(pos2(text_x, rect.top() + 34.0), Align2::LEFT_CENTER, detail, theme::regular(12.0), p.muted);
    }
    let track = Rect::from_center_size(pos2(rect.right() - 22.0, rect.center().y), vec2(42.0, 24.0));
    if t > 0.0 {
        painter.add(gradient(ui, track, 12, p.accent.gamma_multiply(t), p.accent2.gamma_multiply(t), Vec2::X));
    }
    if t < 1.0 {
        painter.rect_filled(track, 12, p.border_strong.gamma_multiply(1.0 - t));
    }
    let knob = pos2(track.left() + 12.0 + t * (track.width() - 24.0), track.center().y);
    painter.circle_filled(knob + vec2(0.0, 1.0), 9.0, Color32::from_black_alpha(40));
    painter.circle_filled(knob, 9.0, Color32::WHITE);
    response
}

/// Pick one of a few options (theme): a sunken track, the chosen option raised.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str, &str)]) -> bool {
    let p = Palette::of(ui);
    let width = ui.available_width().min(140.0 * options.len() as f32);
    // Its own (automatic, unique) id: several selectors in one panel must not share their cells'.
    let (rect, own) = ui.allocate_exact_size(vec2(width, 40.0), Sense::hover());
    let track = if p.dark { p.bg } else { p.border.lerp_to_gamma(p.raised, 0.35) };
    ui.painter().rect_filled(rect, 12, track);
    let slot = (rect.width() - 8.0) / options.len() as f32;
    let mut changed = false;
    for (i, (option, glyph, label)) in options.iter().enumerate() {
        let cell = Rect::from_min_size(pos2(rect.left() + 4.0 + slot * i as f32, rect.top() + 4.0), vec2(slot, rect.height() - 8.0));
        let response = ui.interact(cell, own.id.with(("segment", i)), Sense::click());
        if response.clicked() && *value != *option {
            *value = *option;
            changed = true;
        }
        let selected = *value == *option;
        let painter = ui.painter();
        if selected {
            soft_shadow(painter, cell, 9, 0.5, p.dark);
            painter.rect_filled(cell, 9, p.surface);
        } else if response.hovered() {
            painter.rect_filled(cell, 9, p.surface.gamma_multiply(0.5));
        }
        let color = if selected { p.text } else { p.muted };
        let font = if selected { theme::semibold(13.0) } else { theme::regular(13.0) };
        painter.text(cell.center(), Align2::CENTER_CENTER, format!("{glyph}  {label}"), font, color);
    }
    changed
}

/// Tabs: like `segmented`, but each tab as wide as its label (plus padding inside), with a gap
/// between tabs and around them; spare width is shared out so the bar fills its line.
pub fn tab_bar<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str, &str)]) -> bool {
    const INSET: f32 = 5.0; // between the track's edge and the tabs
    const GAP: f32 = 6.0; // between two tabs
    const PAD: f32 = 16.0; // inside a tab, on each side of its label
    let p = Palette::of(ui);
    let label = |i: usize| format!("{}  {}", options[i].1, options[i].2);
    // The selected tab's (semibold) width for every tab: selecting one does not move the others.
    let natural: Vec<f32> = (0..options.len())
        .map(|i| ui.painter().layout_no_wrap(label(i), theme::semibold(13.0), Color32::WHITE).size().x + 2.0 * PAD)
        .collect();
    let gaps = INSET * 2.0 + GAP * options.len().saturating_sub(1) as f32;
    let needed = natural.iter().sum::<f32>() + gaps;
    let width = ui.available_width().max(needed);
    let extra = (width - needed) / options.len() as f32;
    let (rect, own) = ui.allocate_exact_size(vec2(width, 44.0), Sense::hover());
    let track = if p.dark { p.bg } else { p.border.lerp_to_gamma(p.raised, 0.35) };
    ui.painter().rect_filled(rect, 13, track);
    let mut changed = false;
    let mut left = rect.left() + INSET;
    for (i, (option, _, _)) in options.iter().enumerate() {
        let cell = Rect::from_min_size(pos2(left, rect.top() + INSET), vec2(natural[i] + extra, rect.height() - 2.0 * INSET));
        left += cell.width() + GAP;
        let response = ui.interact(cell, own.id.with(("tab", i)), Sense::click());
        if response.clicked() && *value != *option {
            *value = *option;
            changed = true;
        }
        let selected = *value == *option;
        let painter = ui.painter();
        if selected {
            soft_shadow(painter, cell, 9, 0.5, p.dark);
            painter.rect_filled(cell, 9, p.surface);
        } else if response.hovered() {
            painter.rect_filled(cell, 9, p.surface.gamma_multiply(0.5));
        }
        let (color, font) = if selected { (p.text, theme::semibold(13.0)) } else { (p.muted, theme::regular(13.0)) };
        painter.text(cell.center(), Align2::CENTER_CENTER, label(i), font, color);
    }
    changed
}

// ── Charts ───────────────────────────────────────────────────────────────
/// Area chart of `samples` (oldest first): gradient line, fill fading downwards, a glowing head.
pub fn area_chart(ui: &Ui, rect: Rect, samples: &[f32]) {
    let p = Palette::of(ui);
    let painter = ui.painter_at(rect.expand(6.0));
    for i in 1..4 {
        let y = rect.top() + rect.height() * i as f32 / 4.0;
        painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], Stroke::new(1.0_f32, p.border.gamma_multiply(0.6)));
    }
    let n = samples.len();
    if n < 2 {
        return;
    }
    // Light smoothing: sampled every 0.5 s, raw speeds are jagged.
    let smooth: Vec<f32> = (0..n)
        .map(|i| {
            let (a, b) = (i.saturating_sub(1), (i + 1).min(n - 1));
            (samples[a] + 2.0 * samples[i] + samples[b]) / 4.0
        })
        .collect();
    let max = smooth.iter().copied().fold(0.0f32, f32::max);
    let scale = if max > 0.0 { max * 1.2 } else { 1.0 };
    let points: Vec<Pos2> = smooth
        .iter()
        .enumerate()
        .map(|(i, &s)| pos2(rect.left() + rect.width() * i as f32 / (n - 1) as f32, rect.bottom() - rect.height() * s / scale))
        .collect();

    let mut fill = Mesh::default();
    for (i, point) in points.iter().enumerate() {
        let t = i as f32 / (n - 1) as f32;
        let top = p.accent.lerp_to_gamma(p.accent2, t).gamma_multiply(if p.dark { 0.42 } else { 0.30 });
        fill.colored_vertex(*point, top);
        fill.colored_vertex(pos2(point.x, rect.bottom()), Color32::TRANSPARENT);
        if i > 0 {
            let k = (i * 2) as u32;
            fill.add_triangle(k - 2, k - 1, k);
            fill.add_triangle(k - 1, k + 1, k);
        }
    }
    painter.add(Shape::mesh(fill));
    let (a, b) = (p.accent, p.accent2);
    let stroke = PathStroke::new_uv(2.4_f32, move |r: Rect, pos: Pos2| a.lerp_to_gamma(b, ((pos.x - r.left()) / r.width().max(1.0)).clamp(0.0, 1.0)));
    painter.add(Shape::line(points.clone(), stroke));
    if let Some(&head) = points.last()
        && max > 0.0
    {
        glow(&painter, head, Vec2::splat(14.0), p.accent2.gamma_multiply(0.5));
        painter.circle_filled(head, 4.0, p.accent2);
        painter.circle_filled(head, 1.8, Color32::WHITE);
    }
}

/// Ring chart: `segments` are (value, colour), drawn clockwise from the top.
pub fn donut(painter: &Painter, center: Pos2, radius: f32, width: f32, segments: &[(f32, Color32)], track: Color32) {
    painter.circle_stroke(center, radius, Stroke::new(width, track));
    let total: f32 = segments.iter().map(|(v, _)| v).sum();
    if total <= 0.0 {
        return;
    }
    let mut start = -TAU / 4.0;
    for &(value, color) in segments {
        if value <= 0.0 {
            continue;
        }
        let sweep = TAU * value / total;
        let steps = ((sweep * radius / 3.0) as usize).max(2);
        let points: Vec<Pos2> = (0..=steps)
            .map(|i| {
                let a = start + sweep * i as f32 / steps as f32;
                center + vec2(a.cos(), a.sin()) * radius
            })
            .collect();
        painter.add(Shape::line(points, Stroke::new(width, color)));
        start += sweep;
    }
}

/// A rotating three-quarter ring.
pub fn spinner(painter: &Painter, center: Pos2, radius: f32, color: Color32, time: f32) {
    let start = time * TAU * 1.2;
    let points: Vec<Pos2> = (0..=24)
        .map(|i| {
            let a = start + TAU * 0.72 * i as f32 / 24.0;
            center + vec2(a.cos(), a.sin()) * radius
        })
        .collect();
    painter.add(Shape::line(points, Stroke::new(2.0_f32, color)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let _one_at_a_time = crate::i18n::TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::i18n::set(crate::i18n::Language::English);
        assert_eq!(bytes(1536), "1.5 KB");
        crate::i18n::set(crate::i18n::Language::French);
        assert_eq!(bytes(512), "512 o");
        assert_eq!(bytes(1536), "1,5 Ko");
        assert_eq!(speed(3.5 * 1024.0 * 1024.0), "3,5 Mo/s");
        assert_eq!(duration(59), "59 s");
        assert_eq!(duration(125), "2 min 05 s");
        assert_eq!(duration(7260), "2 h 01 min");
    }
}
