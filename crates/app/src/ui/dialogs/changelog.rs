//! "What's new": after an update, the release notes of the version now running (in English, as
//! published), with a button that opens them translated in the browser.

use super::*;
use crate::i18n;

/// The release notes of this version (`docs/releases/vX.Y.Z.md`, written for each release).
const NOTES: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/releases/v", env!("CARGO_PKG_VERSION"), ".md"));

/// The notes without their "Install" section (the files of the release: of no use once installed)
/// nor their `**` and backticks. Worked out once: the dialog draws them at every frame.
fn notes() -> &'static str {
    static SHOWN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SHOWN.get_or_init(|| without_install().replace("**", "").replace('`', ""))
}

fn without_install() -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in NOTES.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            skipping = title.trim().eq_ignore_ascii_case("install");
        }
        if !skipping {
            out.push(line);
        }
    }
    out.join("\n").trim().to_owned()
}

/// This version's release page, translated into `language` by Google Translate. A short link:
/// the notes themselves in the link would make it several thousand characters long, which the
/// system may refuse to open without a word.
fn translate_url(language: Language) -> Option<String> {
    let code = match language {
        Language::French => "fr",
        Language::Spanish => "es",
        Language::German => "de",
        Language::Italian => "it",
        Language::Portuguese => "pt",
        Language::Dutch => "nl",
        Language::Polish => "pl",
        Language::Russian => "ru",
        Language::Ukrainian => "uk",
        Language::Turkish => "tr",
        Language::Vietnamese => "vi",
        Language::Indonesian => "id",
        Language::Chinese => "zh-CN",
        Language::Japanese => "ja",
        Language::Korean => "ko",
        _ => return None,
    };
    let page = format!("https://github.com/{}/releases/tag/v{}", update::repo()?, env!("CARGO_PKG_VERSION"));
    Some(format!("https://translate.google.com/translate?sl=en&tl={code}&u={page}"))
}

/// One line of the notes: headings, bullets and paragraphs.
fn line(ui: &mut Ui, p: &Palette, text: &str) {
    if text.trim().is_empty() {
        ui.add_space(6.0);
    } else if let Some(title) = text.strip_prefix("## ").or_else(|| text.strip_prefix("# ")) {
        ui.add_space(6.0);
        ui.label(RichText::new(title).font(theme::semibold(15.0)).color(p.text));
        ui.add_space(2.0);
    } else if let Some(item) = text.trim_start().strip_prefix("- ") {
        ui.horizontal_top(|ui| {
            ui.label(RichText::new("•").color(p.accent));
            ui.add(Label::new(RichText::new(item).color(p.text)).wrap());
        });
    } else {
        ui.add(Label::new(RichText::new(text).color(p.muted)).wrap());
    }
}

impl App<'_> {
    pub(in crate::ui) fn changelog_dialog(&mut self, ctx: &Context) {
        if !self.changelog {
            return;
        }
        let p = Palette::from_ctx(ctx);
        let mut close = false;
        let modal = Modal::new(Id::new("changelog")).frame(dialog_frame(&p)).backdrop_color(backdrop(&p)).show(ctx, |ui| {
            ui.set_width(560.0);
            let title = trf!("RDM {v} est installé", "RDM {v} is installed", v = env!("CARGO_PKG_VERSION"));
            if dialog_header(ui, &p, icon::SPARKLE, p.accent, &title, tr!("Nouveautés de cette version", "What's new in this version")) {
                close = true;
            }
            ui.add_space(14.0);
            ScrollArea::vertical().max_height(380.0).auto_shrink([false, true]).show(ui, |ui| {
                for l in notes().lines() {
                    line(ui, &p, l);
                }
            });
            ui.add_space(16.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if accent_button(ui, icon::CHECK, tr!("OK", "OK")).clicked() {
                    close = true;
                }
                if let Some(url) = translate_url(i18n::active())
                    && ghost_button(ui, icon::TRANSLATE, tr!("Traduire", "Translate")).clicked()
                {
                    open_link(url);
                }
            });
        });
        if close || modal.should_close() {
            self.changelog = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_drop_the_install_section() {
        assert!(!notes().contains("## Install"));
        assert!(!notes().is_empty());
    }

    #[test]
    fn translation_links() {
        let url = translate_url(Language::French).expect("French is translated");
        assert!(url.starts_with("https://translate.google.com/translate?sl=en&tl=fr&u=https://github.com/"));
        assert!(url.ends_with(concat!("/releases/tag/v", env!("CARGO_PKG_VERSION"))));
        assert_eq!(translate_url(Language::English), None);
    }
}
