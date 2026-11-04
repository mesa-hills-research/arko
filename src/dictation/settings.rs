//! Dictation's settings, in `dictation.json` in Arko's folder. They're read
//! at each recording, so a change any front end makes applies to the next
//! one. Every field may be left out. A front end changes the cleanup with
//! [`set_cleanup`].

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
    /// Favour the session's words, such as its names, identifiers and file
    /// names ([`session_words`](super::session_words())).
    pub session_words: bool,
    /// Leave out of the session's words the values that context marks as
    /// secret, such as what follows `PASSWORD=` or `--token`.
    pub leave_out_secrets: bool,
    /// What to take out of English speech, a [`Cleanup`]'s name. A name it
    /// doesn't know means none, and leaves the other settings as they are.
    pub cleanup: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model: "automatic".into(),
            language: None,
            idle_minutes: 10,
            download: "ask".into(),
            session_words: true,
            leave_out_secrets: false,
            cleanup: Cleanup::Off.name().into(),
        }
    }
}

/// What dictation takes out of English speech.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cleanup {
    /// Nothing.
    Off,
    /// Hesitations such as um and uh, and stutters such as "the the".
    Light,
    /// Also repeats, false starts and openers such as "So" or "I mean",
    /// with Telmo's hand-written tagger. Each utterance's words stay
    /// tentative until it ends.
    Medium,
    /// The same as [`Medium`](Cleanup::Medium), caught more often, with a
    /// trained tagger whose model is kept in Arko's folder
    /// ([`tagger_dir`](super::models::tagger_dir)).
    Full,
}

impl Cleanup {
    /// Every level, from the least taken out.
    pub const ALL: [Cleanup; 4] = [Cleanup::Off, Cleanup::Light, Cleanup::Medium, Cleanup::Full];

    /// Its name in `dictation.json`.
    pub fn name(self) -> &'static str {
        match self {
            Cleanup::Off => "off",
            Cleanup::Light => "light",
            Cleanup::Medium => "medium",
            Cleanup::Full => "full",
        }
    }

    /// The level `name` names, if any.
    pub fn named(name: &str) -> Option<Cleanup> {
        Cleanup::ALL.into_iter().find(|level| level.name() == name)
    }
}

/// Sets `cleanup` in the settings in Arko's folder `root`, keeping the
/// file's other fields. A file that isn't a JSON object stays as it is, and
/// the error says so.
pub fn set_cleanup(root: &Path, cleanup: Cleanup) -> std::io::Result<()> {
    let path = root.join(FILE);
    let mut fields = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{FILE} isn't valid JSON"),
                )
            })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(error) => return Err(error),
    };
    fields.insert("cleanup".into(), cleanup.name().into());
    let mut text = serde_json::to_string_pretty(&fields)?;
    text.push('\n');
    std::fs::create_dir_all(root)?;
    crate::session::write_whole(&path, text.as_bytes())
}

impl Settings {
    /// The settings in Arko's folder `root`, with the defaults where the file is
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

    /// The cleanup the settings choose, [`Cleanup::Off`] for a name it
    /// doesn't know.
    pub fn cleanup(&self) -> Cleanup {
        Cleanup::named(&self.cleanup).unwrap_or(Cleanup::Off)
    }

    /// The language to choose the model by, meaning the setting's, else the
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
        assert!(settings.session_words);
        assert!(!settings.leave_out_secrets);
        let settings: Settings =
            serde_json::from_str(r#"{"session_words": false, "leave_out_secrets": true}"#).unwrap();
        assert!(!settings.session_words);
        assert!(settings.leave_out_secrets);
    }

    #[test]
    fn cleanup_is_off_unless_the_settings_name_a_level() {
        assert_eq!(Settings::default().cleanup(), Cleanup::Off);
        for level in Cleanup::ALL {
            let text = format!(r#"{{"cleanup": "{}"}}"#, level.name());
            let settings: Settings = serde_json::from_str(&text).unwrap();
            assert_eq!(settings.cleanup(), level);
        }
        // A level this version doesn't know keeps the other settings.
        let settings: Settings =
            serde_json::from_str(r#"{"cleanup": "thorough", "model": "parakeet"}"#).unwrap();
        assert_eq!(settings.cleanup(), Cleanup::Off);
        assert_eq!(settings.model, "parakeet");
    }

    #[test]
    fn setting_the_cleanup_keeps_the_other_fields() {
        let root = tempfile::tempdir().unwrap();
        let read = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(root.path().join(FILE)).unwrap()).unwrap()
        };
        // A missing file is made.
        set_cleanup(root.path(), Cleanup::Light).unwrap();
        assert_eq!(read(), serde_json::json!({"cleanup": "light"}));
        std::fs::write(
            root.path().join(FILE),
            r#"{"model": "parakeet", "later": [1, 2], "cleanup": "light"}"#,
        )
        .unwrap();
        set_cleanup(root.path(), Cleanup::Off).unwrap();
        assert_eq!(
            read(),
            serde_json::json!({"model": "parakeet", "later": [1, 2], "cleanup": "off"})
        );
        assert_eq!(Settings::load(root.path()).cleanup(), Cleanup::Off);
        // A file that isn't JSON is left for its owner to mend.
        std::fs::write(root.path().join(FILE), "{\"model\": ").unwrap();
        let error = set_cleanup(root.path(), Cleanup::Light).unwrap_err();
        assert_eq!(error.to_string(), "dictation.json isn't valid JSON");
        assert_eq!(
            std::fs::read_to_string(root.path().join(FILE)).unwrap(),
            "{\"model\": "
        );
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
