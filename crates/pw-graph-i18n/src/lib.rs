//! Small catalog-based localization layer with English fallback.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
pub enum Locale {
    #[default]
    English,
    Spanish,
    French,
}

impl Locale {
    pub const ALL: [Self; 3] = [Self::English, Self::Spanish, Self::French];

    pub fn parse(value: &str) -> Self {
        let language = value.trim().to_ascii_lowercase();
        if language == "es" || language.starts_with("es-") || language.starts_with("es_") {
            Self::Spanish
        } else if language == "fr" || language.starts_with("fr-") || language.starts_with("fr_") {
            Self::French
        } else {
            Self::English
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Spanish => "es",
            Self::French => "fr",
        }
    }

    pub fn native_name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Spanish => "Español",
            Self::French => "Français",
        }
    }
}

#[derive(Clone, Debug)]
pub struct I18n {
    locale: Locale,
    english: BTreeMap<String, String>,
    current: BTreeMap<String, String>,
}

impl Default for I18n {
    fn default() -> Self {
        Self::new(Locale::default())
    }
}

/// Translation modules: one file per key-prefix section under
/// `locales/<lang>/<module>.json`. This list is the single source of truth for
/// which module files are merged into each locale catalog; keep it sorted and
/// mirror every entry in all three locale directories (see `locales/README.md`).
macro_rules! define_catalog {
    ($($module:literal),+ $(,)?) => {
        pub const MODULES: &[&str] = &[$($module),+];
        const MODULE_COUNT: usize = MODULES.len();

        fn locale_sources(locale: Locale) -> [&'static str; MODULE_COUNT] {
            match locale {
                Locale::English => {
                    [$(include_str!(concat!("../locales/en/", $module, ".json"))),+]
                }
                Locale::Spanish => {
                    [$(include_str!(concat!("../locales/es/", $module, ".json"))),+]
                }
                Locale::French => {
                    [$(include_str!(concat!("../locales/fr/", $module, ".json"))),+]
                }
            }
        }
    };
}

define_catalog!(
    "app",
    "canvas",
    "cli",
    "connect",
    "debug",
    "effects",
    "filter",
    "help",
    "history",
    "inspector",
    "language",
    "meters",
    "nav",
    "patchbay",
    "port",
    "preferences",
    "recorder",
    "relay",
    "screen",
    "search",
    "shortcuts",
    "sort",
    "status",
    "toolbar",
    "tray",
    "video",
);

impl I18n {
    pub fn new(locale: Locale) -> Self {
        let english = load_locale(Locale::English);
        let current = if locale == Locale::English {
            english.clone()
        } else {
            load_locale(locale)
        };
        Self {
            locale,
            english,
            current,
        }
    }

    pub fn from_language(value: &str) -> Self {
        Self::new(Locale::parse(value))
    }

    pub fn locale(&self) -> Locale {
        self.locale
    }

    pub fn set_locale(&mut self, locale: Locale) {
        self.locale = locale;
        self.current = if locale == Locale::English {
            self.english.clone()
        } else {
            load_locale(locale)
        };
    }

    pub fn text(&self, key: &str) -> String {
        self.current
            .get(key)
            .or_else(|| self.english.get(key))
            .cloned()
            .unwrap_or_else(|| key.to_owned())
    }

    pub fn format(&self, key: &str, variables: &[(&str, String)]) -> String {
        let mut message = self.text(key);
        for (name, value) in variables {
            message = message.replace(&format!("{{{name}}}"), value);
        }
        message
    }
}

fn load_catalog(text: &str) -> BTreeMap<String, String> {
    serde_json::from_str(text).expect("bundled locale catalog must be valid JSON")
}

fn load_locale(locale: Locale) -> BTreeMap<String, String> {
    let mut catalog = BTreeMap::new();
    for text in locale_sources(locale) {
        catalog.extend(load_catalog(text));
    }
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spanish_translates_and_falls_back() {
        let i18n = I18n::new(Locale::Spanish);
        assert_eq!(i18n.text("toolbar.undo"), "Deshacer");
        assert_eq!(i18n.text("missing.key"), "missing.key");
    }

    #[test]
    fn formatting_replaces_named_variables() {
        let i18n = I18n::default();
        assert_eq!(
            i18n.format(
                "status.connected",
                &[("output", "1".into()), ("input", "2".into())]
            ),
            "Connected port 1 to 2"
        );
    }

    #[test]
    fn locale_catalogs_cover_the_same_keys() {
        let english = load_locale(Locale::English);
        let spanish = load_locale(Locale::Spanish);
        let french = load_locale(Locale::French);
        assert_eq!(
            english.keys().collect::<Vec<_>>(),
            spanish.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            english.keys().collect::<Vec<_>>(),
            french.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn locale_modules_do_not_shadow_keys() {
        for locale in Locale::ALL {
            let mut total = 0;
            for text in locale_sources(locale) {
                total += load_catalog(text).len();
            }
            assert_eq!(
                load_locale(locale).len(),
                total,
                "duplicate keys across {locale:?} modules"
            );
        }
    }

    #[test]
    fn every_module_file_is_loaded() {
        let locales = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        let mut expected: Vec<String> = MODULES
            .iter()
            .map(|module| format!("{module}.json"))
            .collect();
        expected.sort();
        for locale in Locale::ALL {
            let mut files: Vec<String> = std::fs::read_dir(locales.join(locale.code()))
                .expect("locale directory must exist")
                .map(|entry| {
                    entry
                        .expect("locale entry must be readable")
                        .file_name()
                        .to_str()
                        .expect("module file name must be UTF-8")
                        .to_owned()
                })
                .filter(|name| name.ends_with(".json"))
                .collect();
            files.sort();
            assert_eq!(files, expected, "unloaded module file in {locale:?}");
        }
    }

    #[test]
    fn refresh_tooltips_are_translated_in_every_locale() {
        for locale in Locale::ALL {
            let i18n = I18n::new(locale);
            assert_ne!(i18n.text("toolbar.refresh"), "toolbar.refresh");
            assert_ne!(i18n.text("help.refresh"), "help.refresh");
            assert_ne!(i18n.text("toolbar.minimap"), "toolbar.minimap");
            assert_ne!(i18n.text("help.minimap"), "help.minimap");
        }
    }
}
