//! Design tokens (colours, type scale), fonts and the egui style, for both themes.

use std::sync::{Arc, LazyLock};

use domain::Category;
use eframe::egui::{
    Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Margin, Shadow, Stroke,
    Style, TextStyle, Theme, Ui, style::ScrollStyle, vec2,
};

pub const HAIRLINE: f32 = 1.0;

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub sidebar: Color32,
    pub surface: Color32,
    /// Hovered cards, inputs.
    pub raised: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    /// Brand gradient: `accent` → `accent2`.
    pub accent: Color32,
    pub accent2: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

const DARK: Palette = Palette {
    dark: true,
    bg: rgb(0x0A0E1A),
    sidebar: rgb(0x0D1222),
    surface: rgb(0x121829),
    raised: rgb(0x192137),
    border: rgb(0x1F2841),
    border_strong: rgb(0x2C3756),
    text: rgb(0xEAEEF8),
    muted: rgb(0x8D97B3),
    faint: rgb(0x566081),
    accent: rgb(0x4F8DFF),
    accent2: rgb(0xA259FF),
    success: rgb(0x34D399),
    warning: rgb(0xFBBF24),
    danger: rgb(0xFB7185),
};

const LIGHT: Palette = Palette {
    dark: false,
    bg: rgb(0xF2F4FA),
    sidebar: rgb(0xFAFBFE),
    surface: rgb(0xFFFFFF),
    raised: rgb(0xF4F6FC),
    border: rgb(0xE2E6F0),
    border_strong: rgb(0xCDD4E4),
    text: rgb(0x141A2E),
    muted: rgb(0x5E6A86),
    faint: rgb(0x9BA4BB),
    accent: rgb(0x3D6BFF),
    accent2: rgb(0x8B3DFF),
    success: rgb(0x0E9F6E),
    warning: rgb(0xD97706),
    danger: rgb(0xE11D48),
};

impl Palette {
    pub fn of(ui: &Ui) -> Self {
        if ui.visuals().dark_mode { DARK } else { LIGHT }
    }

    pub fn from_ctx(ctx: &Context) -> Self {
        if ctx.style().visuals.dark_mode { DARK } else { LIGHT }
    }

    /// Each category has its hue: tiles, sidebar dots.
    pub fn category(&self, c: Category) -> Color32 {
        let (dark, light) = match c {
            Category::Video => (0xF472B6, 0xDB2777),
            Category::Music => (0xFB923C, 0xEA580C),
            Category::Archive => (0xFACC15, 0xCA8A04),
            Category::Program => (0x60A5FA, 0x2563EB),
            Category::Document => (0x34D399, 0x059669),
            Category::Other => (0xA78BFA, 0x7C3AED),
        };
        rgb(if self.dark { dark } else { light })
    }

    /// A translucent version of `c` for tinted backgrounds.
    pub fn tint(&self, c: Color32, strength: f32) -> Color32 {
        c.gamma_multiply(if self.dark { strength } else { strength * 0.8 })
    }
}

// ── Type ─────────────────────────────────────────────────────────────────
static SEMIBOLD: LazyLock<FontFamily> = LazyLock::new(|| FontFamily::Name("semibold".into()));
static BOLD: LazyLock<FontFamily> = LazyLock::new(|| FontFamily::Name("bold".into()));

pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, SEMIBOLD.clone())
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, BOLD.clone())
}

/// Inter (regular, semibold, bold) with Phosphor icons in every family, then egui's own fonts as
/// fallbacks for scripts Inter's Latin subset lacks (file names can be anything).
pub fn install(ctx: &Context) {
    let mut fonts = FontDefinitions::default();
    let fallbacks = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let font = |bytes: &'static [u8]| Arc::new(FontData::from_static(bytes));
    // Phosphor glyphs sit a touch high next to Inter's x-height.
    let icons = |bytes: &'static [u8]| {
        Arc::new(FontData::from_static(bytes).tweak(FontTweak { y_offset_factor: 0.1, ..FontTweak::default() }))
    };
    fonts.font_data.insert("inter".into(), font(include_bytes!("../../assets/fonts/Inter-400.ttf")));
    fonts.font_data.insert("inter-semibold".into(), font(include_bytes!("../../assets/fonts/Inter-600.ttf")));
    fonts.font_data.insert("inter-bold".into(), font(include_bytes!("../../assets/fonts/Inter-700.ttf")));
    fonts.font_data.insert("phosphor".into(), icons(egui_phosphor::Variant::Regular.font_bytes()));

    let family = |main: &str| {
        [main.to_owned(), "phosphor".to_owned()].into_iter().chain(fallbacks.iter().cloned()).collect::<Vec<_>>()
    };
    fonts.families.insert(FontFamily::Proportional, family("inter"));
    fonts.families.insert(SEMIBOLD.clone(), family("inter-semibold"));
    fonts.families.insert(BOLD.clone(), family("inter-bold"));
    if let Some(mono) = fonts.families.get_mut(&FontFamily::Monospace) {
        mono.push("phosphor".into());
    }
    ctx.set_fonts(fonts);
    ctx.style_mut_of(Theme::Dark, |s| apply(s, &DARK));
    ctx.style_mut_of(Theme::Light, |s| apply(s, &LIGHT));
}

fn apply(s: &mut Style, p: &Palette) {
    s.text_styles = [
        (TextStyle::Heading, semibold(20.0)),
        (TextStyle::Body, regular(14.0)),
        (TextStyle::Button, semibold(13.5)),
        (TextStyle::Small, regular(12.0)),
        (TextStyle::Monospace, FontId::monospace(12.5)),
    ]
    .into();

    // Selectable labels would swallow the cards' right-click and double-click (copying goes
    // through the context menu instead).
    s.interaction.selectable_labels = false;
    s.spacing.item_spacing = vec2(8.0, 8.0);
    s.spacing.button_padding = vec2(14.0, 7.0);
    s.spacing.interact_size.y = 32.0;
    s.spacing.slider_width = 220.0;
    s.spacing.menu_margin = Margin::same(8);
    s.spacing.window_margin = Margin::same(20);
    s.spacing.scroll = ScrollStyle::floating();

    let v = &mut s.visuals;
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.raised; // text fields
    v.faint_bg_color = p.raised;
    v.code_bg_color = p.raised;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    // Also the focus ring of text fields.
    v.selection.stroke = Stroke::new(HAIRLINE, p.accent);
    v.text_cursor.stroke = Stroke::new(2.0_f32, p.accent);
    v.window_corner_radius = CornerRadius::same(18);
    v.menu_corner_radius = CornerRadius::same(12);
    v.window_stroke = Stroke::new(HAIRLINE, p.border_strong);
    let shadow_alpha = if p.dark { 150 } else { 40 };
    v.window_shadow = Shadow { offset: [0, 16], blur: 48, spread: 0, color: Color32::from_black_alpha(shadow_alpha) };
    v.popup_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(shadow_alpha / 2) };
    v.indent_has_left_vline = false;
    v.slider_trailing_fill = true;

    let w = &mut v.widgets;
    for (state, fill) in [
        (&mut w.noninteractive, p.surface),
        (&mut w.inactive, p.raised),
        (&mut w.hovered, p.raised),
        (&mut w.active, p.raised),
        (&mut w.open, p.raised),
    ] {
        state.corner_radius = CornerRadius::same(10);
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(HAIRLINE, p.border);
        state.fg_stroke = Stroke::new(HAIRLINE, p.text);
        state.expansion = 0.0;
    }
    w.noninteractive.fg_stroke = Stroke::new(HAIRLINE, p.muted);
    w.noninteractive.bg_stroke = Stroke::new(HAIRLINE, p.border);
    // `bg_fill` paints slider rails and checkbox boxes: must contrast with the surface.
    w.inactive.bg_fill = p.border_strong;
    w.hovered.bg_fill = p.accent;
    w.active.bg_fill = p.accent;
    w.hovered.bg_stroke = Stroke::new(HAIRLINE, p.accent.gamma_multiply(0.7));
    w.active.bg_stroke = Stroke::new(HAIRLINE, p.accent);
    w.hovered.weak_bg_fill = if p.dark { rgb(0x202A45) } else { rgb(0xEBEFF9) };
}
