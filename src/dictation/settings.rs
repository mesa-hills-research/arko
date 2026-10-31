//! Dictation's settings, in `dictation.json` in Arko's folder. They're read
//! at each recording, so a change any front end makes applies to the next
//! one. Every field may be left out.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// The file, in Arko's folder.
pub const FILE: &str = "dictation.json";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `automatic` (the model for the language), or a model's name
    /// (`granite`, `parakeet`).
    pub model: String,
    /// The language spoken, as an ISO 639-1 code (`fr`, `pt-BR`), or `None`
    /// for the system's.
    pub language: Option<String>,
    /// Minutes without a recording after which the model leaves memory.
    pub idle_minutes: u64,
    /// `ask` before downloading a missing model, or `automatic`.
    pub download: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model: "automatic".into(),
            language: None,
            idle_minutes: 10,
            download: "ask".into(),
        }
    }
}

impl Settings {
    /// The settings in Arko's folder `root`; the defaults where the file is
    /// missing or can't be read.
    pub fn load(root: &Path) -> Self {
        std::fs::read_to_string(root.join(FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn idle(&self) -> Duration {
        Duration::from_secs(self.idle_minutes.max(1) * 60)
    }

    pub fn downloads_without_asking(&self) -> bool {
        self.download == "automatic"
    }

    /// The language to choose the model by: the setting's, else the
    /// system's, from `LC_ALL`, `LC_MESSAGES` or `LANG` (`fr_FR.UTF-8`).
    pub fn language(&self) -> Option<String> {
        self.language
            .clone()
            .filter(|code| !code.is_empty())
            .or_else(|| {
                ["LC_ALL", "LC_MESSAGES", "LANG"]
                    .into_iter()
                    .filter_map(|name| std::env::var(name).ok())
                    .find(|value| !value.is_empty())
                    .and_then(|value| locale_language(&value))
            })
    }
}

/// The language of a POSIX locale (`fr_FR.UTF-8` is `fr_FR`), or `None` for
/// the C locale, which says nothing of it.
fn locale_language(locale: &str) -> Option<String> {
    let code = locale.split(['.', '@']).next().unwrap_or_default();
    (!code.is_empty() && code != "C" && code != "POSIX").then(|| code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_their_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"model": "parakeet"}"#).unwrap();
        assert_eq!(settings.model, "parakeet");
        assert_eq!(settings.idle(), Duration::from_secs(600));
        assert!(!settings.downloads_without_asking());
        let settings: Settings = serde_json::from_str(r#"{"download": "automatic"}"#).unwrap();
        assert!(settings.downloads_without_asking());
        assert_eq!(settings.model, "automatic");
    }

    #[test]
    fn a_locale_gives_its_language() {
        assert_eq!(locale_language("fr_FR.UTF-8").as_deref(), Some("fr_FR"));
        assert_eq!(locale_language("de_DE@euro").as_deref(), Some("de_DE"));
        assert_eq!(locale_language("C.UTF-8"), None);
        assert_eq!(locale_language("POSIX"), None);
    }

    #[test]
    fn the_language_setting_wins_over_the_system() {
        let settings = Settings {
            language: Some("ja".into()),
            ..Settings::default()
        };
        assert_eq!(settings.language().as_deref(), Some("ja"));
    }
}
