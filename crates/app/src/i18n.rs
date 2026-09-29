//! Interface language, the system's by default. French and English sit in the code, side by side
//! (`tr!` / `trf!`); every other language is a table in `locales/<code>.json` mapping the English
//! text to its translation. A text missing from a table shows in English.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use serde::{Deserialize, Serialize};

/// The names are what `settings.json` stores: never rename one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Language {
    /// The system's language, English when RDM has none of the system's.
    #[default]
    Auto,
    English,
    French,
    Spanish,
    German,
    Italian,
    Portuguese,
    Dutch,
    Polish,
    Russian,
    Ukrainian,
    Turkish,
    Vietnamese,
    Indonesian,
    Chinese,
    Japanese,
    Korean,
}

use Language::*;

impl Language {
    /// Every language RDM speaks, in the order of the menu.
    pub const ALL: [Language; 16] =
        [English, French, Spanish, German, Italian, Portuguese, Dutch, Polish, Russian, Ukrainian, Turkish, Vietnamese, Indonesian, Chinese, Japanese, Korean];

    /// Name in the language itself: what its speakers look for in the menu.
    pub const fn native_name(self) -> &'static str {
        match self {
            Auto => "",
            English => "English",
            French => "Français",
            Spanish => "Español",
            German => "Deutsch",
            Italian => "Italiano",
            Portuguese => "Português",
            Dutch => "Nederlands",
            Polish => "Polski",
            Russian => "Русский",
            Ukrainian => "Українська",
            Turkish => "Türkçe",
            Vietnamese => "Tiếng Việt",
            Indonesian => "Bahasa Indonesia",
            Chinese => "简体中文",
            Japanese => "日本語",
            Korean => "한국어",
        }
    }

    /// The language of a system locale such as `pt-BR`, `zh_CN` or `en`.
    fn from_locale(locale: &str) -> Option<Language> {
        let code = locale.split(['-', '_', '.', '@']).next()?.to_ascii_lowercase();
        Some(match code.as_str() {
            "en" => English,
            "fr" => French,
            "es" => Spanish,
            "de" => German,
            "it" => Italian,
            "pt" => Portuguese,
            "nl" => Dutch,
            "pl" => Polish,
            "ru" => Russian,
            "uk" => Ukrainian,
            "tr" => Turkish,
            "vi" => Vietnamese,
            "id" | "in" => Indonesian,
            "zh" => Chinese,
            "ja" => Japanese,
            "ko" => Korean,
            _ => return None,
        })
    }

    /// The system's language: the first of the user's preferred ones that RDM speaks.
    pub fn system() -> Language {
        sys_locale::get_locales().find_map(|l| Language::from_locale(&l)).unwrap_or(English)
    }

    /// Never `Auto`.
    fn resolved(self) -> Language {
        if self == Auto { Language::system() } else { self }
    }

    fn index(self) -> usize {
        Language::ALL.iter().position(|l| *l == self).unwrap_or(0)
    }

    /// The `locales/` table; French and English are in the code.
    pub(crate) fn table(self) -> &'static Table {
        static TABLES: [OnceLock<Table>; 16] = [const { OnceLock::new() }; 16];
        TABLES[self.index()].get_or_init(|| {
            let json = match self {
                Spanish => include_str!("../locales/es.json"),
                German => include_str!("../locales/de.json"),
                Italian => include_str!("../locales/it.json"),
                Portuguese => include_str!("../locales/pt.json"),
                Dutch => include_str!("../locales/nl.json"),
                Polish => include_str!("../locales/pl.json"),
                Russian => include_str!("../locales/ru.json"),
                Ukrainian => include_str!("../locales/uk.json"),
                Turkish => include_str!("../locales/tr.json"),
                Vietnamese => include_str!("../locales/vi.json"),
                Indonesian => include_str!("../locales/id.json"),
                Chinese => include_str!("../locales/zh.json"),
                Japanese => include_str!("../locales/ja.json"),
                Korean => include_str!("../locales/ko.json"),
                Auto | English | French => "{}",
            };
            parse(json)
        })
    }

    /// Chinese, Japanese and Korean share the same characters, drawn differently: the font
    /// (`ui::theme`) picks the regional set of the language.
    pub const fn cjk_font_index(self) -> u32 {
        match self {
            Japanese => 0,
            Korean => 1,
            _ => 2,
        }
    }
}

type Table = HashMap<&'static str, &'static str>;

/// A table lives as long as the program (a handful of languages, a few dozen KB each).
fn parse(json: &str) -> Table {
    let owned: HashMap<String, String> = serde_json::from_str(json).expect("a locales/*.json file is a JSON object of strings");
    owned.into_iter().map(|(k, v)| (&*k.leak(), &*v.leak())).collect()
}

/// Index in `Language::ALL` of the language in use.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Applies `language` (at start, and when changed in the settings).
pub fn set(language: Language) {
    ACTIVE.store(language.resolved().index(), Relaxed);
}

/// The language in use: never `Auto`.
pub fn active() -> Language {
    Language::ALL[ACTIVE.load(Relaxed)]
}

pub fn french() -> bool {
    active() == French
}

/// Whether numbers are written `1,5` rather than `1.5`.
pub fn decimal_comma() -> bool {
    !matches!(active(), English | Chinese | Japanese | Korean)
}

/// The text in the interface language, from its French and English versions.
pub fn text(fr: &'static str, en: &'static str) -> &'static str {
    match active() {
        French => fr,
        English => en,
        other => other.table().get(en).copied().unwrap_or(en),
    }
}

/// `template` with its `{name}` and `{}` placeholders filled from `args` (`""` names a positional
/// one, filled in order).
pub fn render(template: &str, args: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut positional = args.iter().filter(|(name, _)| name.is_empty());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}').map(|c| open + c) else { break };
        out.push_str(&rest[..open]);
        let name = &rest[open + 1..close];
        let value = if name.is_empty() { positional.next() } else { args.iter().find(|(n, _)| *n == name) };
        match value {
            Some((_, value)) => out.push_str(value),
            None => {
                debug_assert!(false, "placeholder {{{name}}} without a value in {template:?}");
                out.push_str(&rest[open..=close]);
            }
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The text in the interface language: `tr!("français", "English")`.
#[macro_export]
macro_rules! tr {
    ($fr:literal, $en:literal $(,)?) => {
        $crate::i18n::text($fr, $en)
    };
}

/// A formatted text in the interface language. Placeholders are `{}` (the positional arguments,
/// in order) and `{name}` (`name = value` arguments): `trf!("{n} fichiers", "{n} files", n = n)`.
/// The translations use the same placeholders as the English text.
#[macro_export]
macro_rules! trf {
    ($fr:literal, $en:literal $(, $($args:tt)*)?) => {
        $crate::trf!(@collect [$fr $en] [] $($($args)*)?)
    };
    (@collect [$fr:literal $en:literal] [$($pairs:tt)*]) => {
        $crate::i18n::render($crate::i18n::text($fr, $en), &[$($pairs)*])
    };
    (@collect $texts:tt [$($pairs:tt)*] $name:ident = $value:expr $(, $($rest:tt)*)?) => {
        $crate::trf!(@collect $texts [$($pairs)* (stringify!($name), ($value).to_string()),] $($($rest)*)?)
    };
    (@collect $texts:tt [$($pairs:tt)*] $value:expr $(, $($rest:tt)*)?) => {
        $crate::trf!(@collect $texts [$($pairs)* ("", ($value).to_string()),] $($($rest)*)?)
    };
}

/// Which noun form goes with `n`: the index among the forms of the language (Russian and
/// Ukrainian: one, few, many; Polish: same; the others: one, other; some: only one form).
fn plural_form(language: Language, n: u64) -> usize {
    let (mod10, mod100) = (n % 10, n % 100);
    match language {
        Chinese | Japanese | Korean | Vietnamese | Indonesian => 0,
        Portuguese => usize::from(n > 1),
        Russian | Ukrainian => match () {
            _ if mod10 == 1 && mod100 != 11 => 0,
            _ if (2..=4).contains(&mod10) && !(12..=14).contains(&mod100) => 1,
            _ => 2,
        },
        Polish => match () {
            _ if n == 1 => 0,
            _ if (2..=4).contains(&mod10) && !(12..=14).contains(&mod100) => 1,
            _ => 2,
        },
        _ => usize::from(n != 1),
    }
}

/// `n` and its noun in the interface language, singular or plural as each language wants (French:
/// 0 and 1 are singular; English: only 1): `count(3, ("fichier", "fichiers"), ("file", "files"))`.
/// The other languages' tables hold the forms after the English pair: `"file|files": "a|b|c"`.
pub fn count(n: impl Into<u64>, fr: (&str, &str), en: (&str, &str)) -> String {
    let n = n.into();
    let english = if n == 1 { en.0 } else { en.1 };
    let noun = match active() {
        French => if n <= 1 { fr.0 } else { fr.1 },
        English => english,
        other => other
            .table()
            .get(format!("{}|{}", en.0, en.1).as_str())
            .and_then(|forms| {
                let mut forms = forms.split('|');
                forms.clone().nth(plural_form(other, n)).or_else(|| forms.next_back())
            })
            .unwrap_or(english),
    };
    format!("{n} {noun}")
}

/// A category's name in the interface language (the folder it is sorted into is named by
/// `Category::label`: French or English only, not to litter the disk with a folder per language).
pub fn category(category: domain::Category) -> &'static str {
    text(category.label(false), category.label(true))
}

/// Tests that switch the (global) language run one at a time.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales() {
        assert_eq!(Language::from_locale("fr-FR"), Some(French));
        assert_eq!((Language::from_locale("FR_ca"), Language::from_locale("fr")), (Some(French), Some(French)));
        assert_eq!(Language::from_locale("pt-BR"), Some(Portuguese));
        assert_eq!(Language::from_locale("zh-Hant-TW"), Some(Chinese));
        assert_eq!(Language::from_locale("en_US.UTF-8"), Some(English));
        assert_eq!(Language::from_locale("in"), Some(Indonesian));
        assert_eq!((Language::from_locale("ar-EG"), Language::from_locale("f"), Language::from_locale("")), (None, None, None));
    }

    #[test]
    fn switching_language() {
        let _one_at_a_time = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        set(French);
        assert_eq!(tr!("Bonjour", "Hello"), "Bonjour");
        let n = 3;
        assert_eq!(trf!("{n} fichiers", "{n} files", n = n), "3 fichiers");
        set(English);
        assert_eq!(tr!("Bonjour", "Hello"), "Hello");
        assert_eq!(trf!("{n} fichiers", "{n} files", n = n), "3 files");
        assert_eq!((count(1u8, ("a", "as"), ("b", "bs")), count(0u8, ("a", "as"), ("b", "bs"))), ("1 b".into(), "0 bs".into()));
        set(French);
        assert_eq!((count(0u8, ("a", "as"), ("b", "bs")), count(2u8, ("a", "as"), ("b", "bs"))), ("0 a".into(), "2 as".into()));
    }

    #[test]
    fn translated_texts() {
        let _one_at_a_time = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        set(German);
        assert_eq!(tr!("Annuler", "Cancel"), "Abbrechen");
        assert_eq!(tr!("Introuvable ailleurs", "In no table"), "In no table");
        let flagged = 3;
        assert_eq!(trf!("{flagged} sur {engines}", "VirusTotal: {flagged} out of {engines}", flagged = flagged, engines = 70), "VirusTotal: 3 von 70");
        set(Russian);
        let connections = |n: u8| count(n, ("connexion", "connexions"), ("connection", "connections"));
        assert_eq!([1, 2, 5, 21].map(connections), ["1 соединение", "2 соединения", "5 соединений", "21 соединение"]);
        set(Japanese);
        assert_eq!(connections(2), "2 件の接続");
        set(French);
    }

    #[test]
    fn placeholders() {
        let args = [("", "x".to_owned()), ("n", "3".to_owned())];
        assert_eq!(render("a {} b {n} c", &args), "a x b 3 c");
        assert_eq!(render("no placeholder, a lone { brace", &args), "no placeholder, a lone { brace");
    }

    #[test]
    fn positional_and_named_arguments() {
        let _one_at_a_time = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        set(English);
        assert_eq!(trf!("{} : {why}", "Version {}: {why}", 7, why = "boom"), "Version 7: boom");
    }

    fn placeholders_of(text: &str) -> Vec<&str> {
        let mut found: Vec<&str> = text.split('{').skip(1).filter_map(|rest| rest.split('}').next()).collect();
        found.sort_unstable();
        found
    }

    /// Every table says the same things as the English texts: same keys as the others, same
    /// placeholders, and as many noun forms as the language has.
    #[test]
    fn locale_files_are_consistent() {
        let reference: std::collections::BTreeSet<&str> = Spanish.table().keys().copied().collect();
        assert!(reference.len() > 300, "the Spanish table looks empty");
        for language in Language::ALL.into_iter().filter(|l| !matches!(l, English | French)) {
            let table = language.table();
            let keys: std::collections::BTreeSet<&str> = table.keys().copied().collect();
            let name = language.native_name();
            assert_eq!(keys, reference, "{name}: keys differ from the Spanish table's");
            for (key, value) in table {
                if key.contains('|') {
                    let forms = value.split('|').count();
                    let expected = (0..1000u64).map(|n| plural_form(language, n)).max().unwrap_or(0) + 1;
                    assert_eq!(forms, expected, "{name}: {key:?} needs {expected} forms, has {forms}");
                } else {
                    assert_eq!(placeholders_of(key), placeholders_of(value), "{name}: {key:?} and its translation differ in placeholders");
                }
            }
        }
    }

    #[test]
    fn plural_forms() {
        assert_eq!([1, 2, 5, 11, 21, 22, 25].map(|n| plural_form(Russian, n)), [0, 1, 2, 2, 0, 1, 2]);
        assert_eq!([1, 2, 5, 12, 22, 112].map(|n| plural_form(Polish, n)), [0, 1, 2, 2, 1, 2]);
        assert_eq!([0, 1, 2].map(|n| plural_form(Portuguese, n)), [0, 0, 1]);
        assert_eq!([0, 1, 2].map(|n| plural_form(German, n)), [1, 0, 1]);
        assert_eq!([0, 1, 2].map(|n| plural_form(Japanese, n)), [0, 0, 0]);
    }
}
