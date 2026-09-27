//! Toasts: short confirmations in the bottom-right corner, fading in and out.

use std::time::{Duration, Instant};

use eframe::egui::{Align2, Area, Color32, Context, Id, Order, Rect, Sense, Stroke, StrokeKind, pos2, vec2};

use super::{
    theme::{self, Palette},
    widgets,
};

const LIFETIME: Duration = Duration::from_millis(3600);
const FADE: f32 = 0.25;
const MAX_SHOWN: usize = 4;

struct Toast {
    glyph: &'static str,
    text: String,
    warning: bool,
    born: Instant,
}

#[derive(Default)]
pub struct Toasts(Vec<Toast>);

impl Toasts {
    pub fn info(&mut self, glyph: &'static str, text: &str) {
        self.push(glyph, text, false);
    }

    pub fn warn(&mut self, glyph: &'static str, text: &str) {
        self.push(glyph, text, true);
    }

    fn push(&mut self, glyph: &'static str, text: &str, warning: bool) {
        self.0.push(Toast { glyph, text: text.to_owned(), warning, born: Instant::now() });
        if self.0.len() > MAX_SHOWN {
            self.0.remove(0);
        }
    }

    /// Draws the live toasts; `true` while any is on screen (they animate).
    pub fn show(&mut self, ctx: &Context) -> bool {
        self.0.retain(|t| t.born.elapsed() < LIFETIME);
        if self.0.is_empty() {
            return false;
        }
        let p = Palette::from_ctx(ctx);
        let screen = ctx.screen_rect();
        let mut bottom = screen.bottom() - 22.0;
        for (i, toast) in self.0.iter().enumerate().rev() {
            let age = toast.born.elapsed().as_secs_f32();
            let left = LIFETIME.as_secs_f32() - age;
            let alpha = (age / FADE).min(left / FADE).clamp(0.0, 1.0);
            let color = if toast.warning { p.warning } else { p.accent };
            let galley = ctx.fonts(|f| f.layout_no_wrap(toast.text.clone(), theme::regular(13.5), p.text.gamma_multiply(alpha)));
            let size = vec2(galley.size().x + 64.0, 46.0);
            let slide = (1.0 - alpha) * 16.0;
            let rect = Rect::from_min_size(pos2(screen.right() - 24.0 - size.x + slide, bottom - size.y), size);
            bottom = rect.top() - 10.0;
            Area::new(Id::new(("toast", i)))
                .order(Order::Tooltip)
                .fixed_pos(rect.min)
                .interactable(false)
                .show(ctx, |ui| {
                    let (_, _) = ui.allocate_exact_size(size, Sense::hover());
                    let painter = ui.painter();
                    widgets::soft_shadow(painter, rect, 14, alpha, p.dark);
                    painter.rect(
                        rect,
                        14,
                        p.surface.gamma_multiply(alpha),
                        Stroke::new(theme::HAIRLINE, p.border_strong.gamma_multiply(alpha)),
                        StrokeKind::Inside,
                    );
                    let dot = pos2(rect.left() + 24.0, rect.center().y);
                    painter.circle_filled(dot, 14.0, p.tint(color, 0.18 * alpha));
                    painter.text(dot, Align2::CENTER_CENTER, toast.glyph, theme::regular(16.0), color.gamma_multiply(alpha));
                    painter.galley(pos2(rect.left() + 48.0, rect.center().y - galley.size().y / 2.0), galley, Color32::PLACEHOLDER);
                });
        }
        true
    }
}
