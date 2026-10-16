//! The view's choices, remembered from one run to the next in `tui.json`
//! in Arko's folder: people settle on a view and a theme.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Prefs {
    /// `full`, `quiet` or `messages`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    /// A theme's name, or `terminal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Whether the mouse scrolls the transcript (off: the terminal selects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouse: Option<bool>,
    /// Whether the theme picker previews the theme it is on (on unless
    /// turned off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_previews: Option<bool>,
    /// The notice about flashing was shown (once is enough).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flash_notice: bool,
}

impl Prefs {
    /// The choices saved at `path`; none when it can't be read.
    pub fn load(path: Option<&Path>) -> Self {
        path.and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Saves them at `path` (a failure only loses the choice).
    pub fn save(&self, path: Option<&Path>) {
        let Some(path) = path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, text + "\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_are_kept_and_a_bad_file_is_none() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tui.json");
        assert_eq!(Prefs::load(Some(&path)), Prefs::default());
        let prefs = Prefs {
            view: Some("quiet".into()),
            theme: Some("Gruvbox Dark".into()),
            mouse: Some(false),
            theme_previews: Some(false),
            flash_notice: true,
        };
        prefs.save(Some(&path));
        assert_eq!(Prefs::load(Some(&path)), prefs);
        std::fs::write(&path, "{").unwrap();
        assert_eq!(Prefs::load(Some(&path)), Prefs::default());
    }
}
