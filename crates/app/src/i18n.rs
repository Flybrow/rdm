//! Interface language: English or French, the system's by default. Every user-facing text sits in
//! the code in both languages, side by side (`tr!` / `trf!`): nothing to keep in sync elsewhere.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Language {
    /// The system's language: French for a French locale, English otherwise.
    #[default]
    Auto,
    French,
    English,
}

static ENGLISH: AtomicBool = AtomicBool::new(true);

/// Applies `language` (at start, and when changed in the settings).
pub fn set(language: Language) {
    let english = match language {
        Language::Auto => !system_is_french(),
        Language::French => false,
        Language::English => true,
    };
    ENGLISH.store(english, Relaxed);
}

pub fn english() -> bool {
    ENGLISH.load(Relaxed)
}

fn system_is_french() -> bool {
    sys_locale::get_locales().next().is_some_and(|l| is_french(&l))
}

fn is_french(locale: &str) -> bool {
    locale.get(..2).is_some_and(|l| l.eq_ignore_ascii_case("fr"))
}

/// The text in the interface language: `tr!("français", "English")`.
#[macro_export]
macro_rules! tr {
    ($fr:literal, $en:literal $(,)?) => {
        if $crate::i18n::english() { $en } else { $fr }
    };
}

/// A formatted text in the interface language; arguments are captured by name, as in `format!`
/// (`trf!("{n} fichiers", "{n} files")`), or passed after the two texts.
#[macro_export]
macro_rules! trf {
    ($fr:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::english() { format!($en $(, $arg)*) } else { format!($fr $(, $arg)*) }
    };
}

/// `n` and its noun in the interface language, singular or plural as each language wants (French:
/// 0 and 1 are singular; English: only 1): `count(3, ("fichier", "fichiers"), ("file", "files"))`.
pub fn count(n: impl Into<u64>, fr: (&str, &str), en: (&str, &str)) -> String {
    let n = n.into();
    let noun = if english() { if n == 1 { en.0 } else { en.1 } } else if n <= 1 { fr.0 } else { fr.1 };
    format!("{n} {noun}")
}

/// Tests that switch the (global) language run one at a time.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales() {
        assert!(is_french("fr-FR") && is_french("fr") && is_french("FR_ca"));
        assert!(!is_french("en-US") && !is_french("f") && !is_french(""));
    }

    #[test]
    fn switching_language() {
        let _one_at_a_time = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        set(Language::French);
        assert_eq!(tr!("Bonjour", "Hello"), "Bonjour");
        let n = 3;
        assert_eq!(trf!("{n} fichiers", "{n} files"), "3 fichiers");
        set(Language::English);
        assert_eq!(tr!("Bonjour", "Hello"), "Hello");
        assert_eq!(trf!("{n} fichiers", "{n} files"), "3 files");
        assert_eq!((count(1u8, ("a", "as"), ("b", "bs")), count(0u8, ("a", "as"), ("b", "bs"))), ("1 b".into(), "0 bs".into()));
        set(Language::French);
        assert_eq!((count(0u8, ("a", "as"), ("b", "bs")), count(2u8, ("a", "as"), ("b", "bs"))), ("0 a".into(), "2 as".into()));
    }
}
