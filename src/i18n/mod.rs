//! UI language for the tray, settings window and notifications (R60).
//! The language is passed as a parameter, not held globally: it changes at
//! runtime and tests run in parallel. CLI output is never translated.

use std::sync::LazyLock;

use i18n_embed::LanguageLoader as _;
use i18n_embed::fluent::FluentLanguageLoader;
use rust_embed::RustEmbed;
use unic_langid::{LanguageIdentifier, langid};

pub use i18n_embed_fl::fl;

#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Catalogues;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En,
    Ru,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::En, Lang::Ru];

    pub fn tag(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
        }
    }

    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ru => "Русский",
        }
    }

    /// Accepts `ru`, `ru-RU`, `ru_RU.UTF-8`, `ru_RU@euro`.
    pub fn from_tag(tag: &str) -> Option<Lang> {
        let primary = tag.split(['_', '-', '.', '@']).next()?;
        match primary.to_ascii_lowercase().as_str() {
            "en" => Some(Lang::En),
            "ru" => Some(Lang::Ru),
            _ => None,
        }
    }

    fn id(self) -> LanguageIdentifier {
        match self {
            Lang::En => langid!("en"),
            Lang::Ru => langid!("ru"),
        }
    }
}

/// The configured language, else the session's (also for an unknown tag).
pub fn resolve(configured: Option<&str>) -> Lang {
    configured.and_then(Lang::from_tag).unwrap_or_else(system)
}

pub fn system() -> Lang {
    *SYSTEM
}

static SYSTEM: LazyLock<Lang> = LazyLock::new(|| from_env(|key| std::env::var(key).ok()));

/// gettext precedence: `LC_ALL` > `LC_MESSAGES` > `LANG`; `LANGUAGE` overrides
/// them unless the locale is C/POSIX.
fn from_env(var: impl Fn(&str) -> Option<String>) -> Lang {
    let Some(locale) = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(&var)
        .find(|value| !value.is_empty())
    else {
        return Lang::En;
    };
    if locale == "C" || locale == "POSIX" || locale.starts_with("C.") {
        return Lang::En;
    }
    let preferences = var("LANGUAGE").unwrap_or_default();
    preferences
        .split(':')
        .find_map(Lang::from_tag)
        .or_else(|| Lang::from_tag(&locale))
        .unwrap_or(Lang::En)
}

static CATALOGUES: LazyLock<FluentLanguageLoader> = LazyLock::new(|| {
    let loader = FluentLanguageLoader::new("rigbat", Lang::En.id());
    if let Err(e) = loader.load_languages(&Catalogues, &Lang::ALL.map(Lang::id)) {
        tracing::error!("failed to load the UI translations: {e}");
    }
    // Otherwise Fluent wraps every argument in invisible U+2068/U+2069 marks.
    loader.set_use_isolating(false);
    loader
});

static EN: LazyLock<FluentLanguageLoader> =
    LazyLock::new(|| CATALOGUES.select_languages(&[Lang::En.id()]));
static RU: LazyLock<FluentLanguageLoader> =
    LazyLock::new(|| CATALOGUES.select_languages(&[Lang::Ru.id()]));

pub fn loader(lang: Lang) -> &'static FluentLanguageLoader {
    match lang {
        Lang::En => &EN,
        Lang::Ru => &RU,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use super::*;

    fn message_ids(lang: Lang) -> BTreeSet<String> {
        CATALOGUES.with_message_iter(&lang.id(), |messages| {
            messages.map(|m| m.id.name.to_owned()).collect()
        })
    }

    // i18n-embed only logs a syntax error, so parse the files ourselves.
    #[test]
    fn every_catalogue_parses() {
        let mut seen = 0;
        for path in Catalogues::iter() {
            let file = Catalogues::get(&path).expect("listed file is embedded");
            let source = std::str::from_utf8(&file.data).expect("catalogue is UTF-8");
            if let Err((_, errors)) = fluent_syntax::parser::parse(source) {
                assert!(errors.is_empty(), "{path} does not parse: {errors:?}");
            }
            seen += 1;
        }
        assert_eq!(seen, Lang::ALL.len(), "one catalogue per language");
    }

    #[test]
    fn catalogues_define_the_same_messages() {
        let reference = message_ids(Lang::En);
        assert!(!reference.is_empty());
        for lang in Lang::ALL {
            let ids = message_ids(lang);
            let missing: Vec<_> = reference.difference(&ids).collect();
            let extra: Vec<_> = ids.difference(&reference).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "{}: missing {missing:?}, not in English {extra:?}",
                lang.tag()
            );
        }
    }

    #[test]
    fn each_language_renders_from_its_own_catalogue() {
        assert_eq!(fl!(loader(Lang::En), "tray-quit"), "Quit");
        assert_eq!(fl!(loader(Lang::Ru), "tray-quit"), "Выход");
    }

    #[test]
    fn substitutions_carry_no_isolation_marks() {
        assert_eq!(
            fl!(loader(Lang::Ru), "entry-offline", name = "MX Master 3"),
            "MX Master 3: не на связи"
        );
    }

    #[test]
    fn from_tag_reads_tags_and_locale_names() {
        for (tag, expected) in [
            ("en", Some(Lang::En)),
            ("ru", Some(Lang::Ru)),
            ("ru-RU", Some(Lang::Ru)),
            ("ru_RU.UTF-8", Some(Lang::Ru)),
            ("ru_RU@euro", Some(Lang::Ru)),
            ("RU", Some(Lang::Ru)),
            ("en_GB.UTF-8", Some(Lang::En)),
            ("de_DE.UTF-8", None),
            ("C", None),
            ("", None),
        ] {
            assert_eq!(Lang::from_tag(tag), expected, "{tag:?}");
        }
    }

    #[test]
    fn tag_round_trips() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_tag(lang.tag()), Some(lang));
        }
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        move |key| map.get(key).map(|v| (*v).to_owned())
    }

    #[test]
    fn from_env_follows_gettext_precedence() {
        for (vars, expected) in [
            (&[][..], Lang::En),
            (&[("LANG", "ru_RU.UTF-8")][..], Lang::Ru),
            (&[("LANG", "de_DE.UTF-8")][..], Lang::En),
            (&[("LANG", "C.UTF-8")][..], Lang::En),
            (&[("LC_ALL", "C"), ("LANG", "ru_RU.UTF-8")][..], Lang::En),
            (
                &[("LC_ALL", "en_US.UTF-8"), ("LANG", "ru_RU.UTF-8")][..],
                Lang::En,
            ),
            (&[("LC_ALL", ""), ("LANG", "ru_RU.UTF-8")][..], Lang::Ru),
            (
                &[("LC_MESSAGES", "ru_RU.UTF-8"), ("LANG", "en_US.UTF-8")][..],
                Lang::Ru,
            ),
            (
                &[("LANGUAGE", "de:ru:en"), ("LANG", "en_US.UTF-8")][..],
                Lang::Ru,
            ),
            (&[("LANGUAGE", "ru"), ("LANG", "C")][..], Lang::En),
            (&[("LANGUAGE", "ru")][..], Lang::En),
        ] {
            assert_eq!(from_env(env(vars)), expected, "{vars:?}");
        }
    }

    #[test]
    fn resolve_prefers_the_configured_language() {
        assert_eq!(resolve(Some("ru")), Lang::Ru);
        assert_eq!(resolve(Some("en")), Lang::En);
    }

    #[test]
    fn resolve_falls_back_to_the_session_language() {
        assert_eq!(resolve(Some("de")), system());
        assert_eq!(resolve(None), system());
    }
}
