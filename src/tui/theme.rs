//! The full-screen view's colours: named roles (text, dim, accent, warning,
//! ...) filled from a theme. The themes are GPUI Kit's, bundled from
//! `themes/` (see NOTICE), and the user's own in the same format in the
//! `themes` folder of Arko's folder; `terminal` keeps the terminal's own
//! colours instead.
//!
//! A theme paints the whole screen, its background too, so the view looks
//! the same whatever the console's palette is (Windows PowerShell's blue
//! one included). That takes more than the 16 colours: with 24-bit colour
//! the theme's own, with 256 the nearest of xterm's, and with fewer the
//! `terminal` theme. The console's palette itself is never changed: leaving
//! the alternate screen brings its colours back.

use std::collections::HashMap;
use std::path::Path;

use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;

use crate::glyphs::{self, Symbols};

/// The theme Arko starts with when none is chosen.
pub const DEFAULT: &str = "macOS Classic Dark";
/// The theme of the terminal's own colours and background.
pub const TERMINAL: &str = "terminal";
/// The setting that chooses the theme: a variant's name, or `terminal`.
pub const SETTING: &str = crate::credentials::THEME;

include!("themes.rs");

/// A colour as red, green and blue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// `#RRGGBB`, or `#RRGGBBAA` blended onto `under`.
    fn parse(text: &str, under: Option<Rgb>) -> Option<Rgb> {
        let hex = text.trim().strip_prefix('#')?;
        let byte = |at: usize| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok();
        let color = Rgb(byte(0)?, byte(2)?, byte(4)?);
        match hex.len() {
            6 => Some(color),
            8 => {
                let alpha = byte(6)? as f64 / 255.0;
                Some(match under {
                    Some(under) => under.mix(color, alpha),
                    None => color,
                })
            }
            _ => None,
        }
    }

    /// This colour moved `amount` (0 to 1) of the way to `other`.
    fn mix(self, other: Rgb, amount: f64) -> Rgb {
        let channel = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * amount).round() as u8;
        Rgb(
            channel(self.0, other.0),
            channel(self.1, other.1),
            channel(self.2, other.2),
        )
    }

    /// WCAG's relative luminance.
    fn luminance(self) -> f64 {
        let linear = |c: u8| {
            let c = c as f64 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.0) + 0.7152 * linear(self.1) + 0.0722 * linear(self.2)
    }

    /// WCAG's contrast ratio of two colours, from 1 to 21.
    pub fn contrast(self, other: Rgb) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// The xterm-256 colour nearest this one (of the colour cube and the
    /// grays: the first 16 follow the terminal's palette).
    pub fn xterm256(self) -> u8 {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let distance = |other: Rgb| {
            let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
            d(self.0, other.0) + d(self.1, other.1) + d(self.2, other.2)
        };
        let nearest_level = |c: u8| {
            (0..6)
                .min_by_key(|&i| (LEVELS[i] as i32 - c as i32).abs())
                .unwrap_or(0)
        };
        let (r, g, b) = (
            nearest_level(self.0),
            nearest_level(self.1),
            nearest_level(self.2),
        );
        let cube = (16 + 36 * r + 6 * g + b) as u8;
        let cube_color = Rgb(LEVELS[r], LEVELS[g], LEVELS[b]);
        let gray = (0..24u8)
            .min_by_key(|&i| distance(Rgb(8 + 10 * i, 8 + 10 * i, 8 + 10 * i)))
            .unwrap_or(0);
        let gray_color = Rgb(8 + 10 * gray, 8 + 10 * gray, 8 + 10 * gray);
        if distance(gray_color) < distance(cube_color) {
            232 + gray
        } else {
            cube
        }
    }
}

/// How many colours the terminal shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// 24-bit colour.
    Rgb,
    /// xterm's 256.
    Ansi256,
    /// The 16 of the terminal's palette: themes need more.
    Ansi16,
}

/// The colours this terminal shows: 24-bit where COLORTERM says so
/// (GNOME Terminal sets it) or where Windows' console takes escape codes
/// (Windows Terminal, and Windows 10's console: crossterm turns them on,
/// style.rs:163-170); 256 where TERM says so without COLORTERM (macOS
/// Terminal.app); else 16. NO_COLOR asks for none, so the terminal's
/// colours are kept (crossterm drops colours then anyway).
pub fn depth() -> Depth {
    let var = |name: &str| std::env::var(name).unwrap_or_default().to_ascii_lowercase();
    if !var("NO_COLOR").is_empty() {
        return Depth::Ansi16;
    }
    #[cfg(windows)]
    if crossterm::style::available_color_count() == u16::MAX {
        return Depth::Rgb;
    }
    let colorterm = var("COLORTERM");
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        Depth::Rgb
    } else if var("TERM").contains("256color") {
        Depth::Ansi256
    } else {
        Depth::Ansi16
    }
}

/// The corners of bordered elements (overlays, panels, menus).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corners {
    Square,
    /// Only debug builds choose it, for now.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    Rounded,
}

impl Corners {
    /// Square, for every theme for now: whether rounded corners suit the
    /// terminal is still open. (A theme's `radius` could choose later.)
    /// Debug builds take ARKO_CORNERS=rounded, to compare the two.
    fn chosen() -> Self {
        #[cfg(debug_assertions)]
        if std::env::var("ARKO_CORNERS").is_ok_and(|value| value == "rounded") {
            return Corners::Rounded;
        }
        Corners::Square
    }
}

/// A theme's light or dark variant, as its file has it.
#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    /// Its corners' radius (6 when the file doesn't say); not used yet:
    /// see [`Corners`].
    #[allow(dead_code)]
    pub radius: f64,
    /// Its colours by key (`background`, `muted.foreground`, ...).
    colors: HashMap<String, String>,
}

/// A theme file's variants.
#[derive(Debug, Clone)]
pub struct Family {
    pub name: String,
    pub variants: Vec<Variant>,
}

/// The themes there are: the bundled ones and the user's, and what was
/// wrong with the user's files.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub families: Vec<Family>,
    pub warnings: Vec<String>,
}

#[derive(serde::Deserialize)]
struct File {
    name: String,
    themes: Vec<FileVariant>,
}

#[derive(serde::Deserialize)]
struct FileVariant {
    name: String,
    #[serde(default)]
    colors: HashMap<String, serde_json::Value>,
    radius: Option<f64>,
}

/// The variants of a theme file's `text`; an error when it isn't one.
fn family(text: &str) -> Result<Family, String> {
    let file: File = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let mut variants = Vec::new();
    for variant in file.themes {
        let colors: HashMap<String, String> = variant
            .colors
            .into_iter()
            .filter_map(|(key, value)| Some((key, value.as_str()?.to_string())))
            .collect();
        for key in ["background", "foreground"] {
            if colors
                .get(key)
                .and_then(|value| Rgb::parse(value, None))
                .is_none()
            {
                return Err(format!("{} has no {key} colour", variant.name));
            }
        }
        variants.push(Variant {
            name: variant.name,
            radius: variant.radius.unwrap_or(6.0),
            colors,
        });
    }
    if variants.is_empty() {
        return Err("it has no themes".into());
    }
    Ok(Family {
        name: file.name,
        variants,
    })
}

impl Catalog {
    /// The bundled themes, then those in `folder` (`*.json`): a theme
    /// there with a bundled one's name takes its place.
    pub fn load(folder: Option<&Path>) -> Self {
        let mut catalog = Catalog::default();
        for (file, text) in BUNDLED {
            match family(text) {
                Ok(family) => catalog.families.push(family),
                Err(error) => catalog
                    .warnings
                    .push(format!("the bundled theme {file} is broken: {error}")),
            }
        }
        let Some(folder) = folder else {
            return catalog;
        };
        let Ok(entries) = std::fs::read_dir(folder) else {
            return catalog;
        };
        let mut paths: Vec<_> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        for path in paths {
            let read = std::fs::read_to_string(&path).map_err(|error| error.to_string());
            match read.and_then(|text| family(&text)) {
                Ok(family) => {
                    let names: Vec<String> = family
                        .variants
                        .iter()
                        .map(|variant| variant.name.to_lowercase())
                        .collect();
                    for other in &mut catalog.families {
                        other
                            .variants
                            .retain(|variant| !names.contains(&variant.name.to_lowercase()));
                    }
                    catalog
                        .families
                        .retain(|family| !family.variants.is_empty());
                    catalog.families.push(family);
                }
                Err(error) => catalog.warnings.push(format!(
                    "The theme file {} was left out: {error}",
                    path.display()
                )),
            }
        }
        catalog
    }

    /// The variant named `name`, in any case.
    pub fn find(&self, name: &str) -> Option<&Variant> {
        let name = name.trim();
        self.families
            .iter()
            .flat_map(|family| &family.variants)
            .find(|variant| variant.name.eq_ignore_ascii_case(name))
    }

    /// The theme named `name` (or `terminal`) for a terminal of `depth`;
    /// the default for a name not known, and `terminal` where the terminal
    /// shows too few colours.
    pub fn theme(&self, name: &str, depth: Depth) -> Theme {
        if depth == Depth::Ansi16 || name.trim().eq_ignore_ascii_case(TERMINAL) {
            return Theme::terminal();
        }
        match self.find(name).or_else(|| self.find(DEFAULT)) {
            Some(variant) => Theme::of(variant, depth),
            None => Theme::terminal(),
        }
    }

    /// Every theme's name in the order the picker lists them, by family,
    /// with `terminal` last.
    pub fn names(&self) -> Vec<(String, String)> {
        let mut names: Vec<(String, String)> = self
            .families
            .iter()
            .flat_map(|family| {
                family
                    .variants
                    .iter()
                    .map(|variant| (family.name.clone(), variant.name.clone()))
            })
            .collect();
        names.push(("Terminal".into(), TERMINAL.into()));
        names
    }
}

/// The view's colours, by role.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// The variant's name, or `terminal`.
    pub name: String,
    /// The whole screen's; `Reset` keeps the terminal's.
    pub background: Color,
    pub text: Color,
    /// Text that matters less: tool activity, notices, the status line.
    pub dim: Color,
    /// The one accent, as text or lines on the background: the user's
    /// margin bar, the prompt, links, what is selected.
    pub accent: Color,
    /// The accent as a filled background (a menu's selected row), and the
    /// text on it.
    pub fill: Color,
    pub on_fill: Color,
    /// A surface a little apart from the background: the input box, code,
    /// keys.
    pub surface: Color,
    /// Overlays and menus.
    pub raised: Color,
    pub border: Color,
    /// A selected message's background.
    pub selection: Color,
    /// The user's messages' background: a quiet band, a little apart from
    /// the background, so they stand out from the agent's work.
    pub user: Color,
    pub warning: Color,
    pub error: Color,
    pub success: Color,
    /// The corners of bordered elements.
    pub corners: Corners,
    /// Dim text is the terminal's faint style, not a colour.
    faint: bool,
}

impl Theme {
    /// The terminal's own colours: its 16, and its background.
    pub fn terminal() -> Self {
        // The classic Windows console doesn't draw faint text.
        let faint = !(cfg!(windows) && crate::glyphs::classic_console());
        Self {
            name: TERMINAL.into(),
            background: Color::Reset,
            text: Color::Reset,
            dim: if faint { Color::Reset } else { Color::DarkGray },
            accent: Color::Cyan,
            fill: Color::Cyan,
            on_fill: Color::Black,
            surface: Color::Reset,
            raised: Color::Reset,
            border: Color::Reset,
            selection: Color::Reset,
            user: Color::Reset,
            warning: Color::Yellow,
            error: Color::Red,
            success: Color::Green,
            corners: Corners::chosen(),
            faint,
        }
    }

    /// `variant`'s colours mapped to the roles, each readable on its
    /// background, for a terminal of `depth`.
    pub fn of(variant: &Variant, depth: Depth) -> Self {
        let roles = Roles::of(variant);
        let paint = |color: Rgb| match depth {
            Depth::Rgb => Color::Rgb(color.0, color.1, color.2),
            _ => Color::Indexed(color.xterm256()),
        };
        Self {
            name: variant.name.clone(),
            background: paint(roles.background),
            text: paint(roles.text),
            dim: paint(roles.dim),
            accent: paint(roles.accent),
            fill: paint(roles.fill),
            on_fill: paint(roles.on_fill),
            surface: paint(roles.surface),
            raised: paint(roles.raised),
            border: paint(roles.border),
            selection: paint(roles.selection),
            user: paint(roles.user),
            warning: paint(roles.warning),
            error: paint(roles.error),
            success: paint(roles.success),
            corners: Corners::chosen(),
            faint: false,
        }
    }

    /// Whether this is the terminal's own colours.
    pub fn is_terminal(&self) -> bool {
        self.background == Color::Reset
    }

    /// The screen: the theme's text on its background.
    pub fn base(&self) -> Style {
        Style::new().fg(self.text).bg(self.background)
    }

    pub fn text(&self) -> Style {
        Style::new().fg(self.text)
    }

    pub fn dim(&self) -> Style {
        if self.faint {
            Style::new().fg(self.dim).add_modifier(Modifier::DIM)
        } else {
            Style::new().fg(self.dim)
        }
    }

    pub fn accent(&self) -> Style {
        Style::new().fg(self.accent)
    }

    pub fn warning(&self) -> Style {
        Style::new().fg(self.warning)
    }

    pub fn error(&self) -> Style {
        Style::new().fg(self.error)
    }

    pub fn success(&self) -> Style {
        Style::new().fg(self.success)
    }

    /// A menu's selected row, or a picker's active choice.
    pub fn chosen(&self) -> Style {
        if self.is_terminal() {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().fg(self.on_fill).bg(self.fill)
        }
    }

    /// A key, as a keycap: the whole shortcut in one cap (` Ctrl+O `).
    pub fn key(&self) -> Style {
        if self.is_terminal() {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().fg(self.text).bg(self.surface)
        }
    }

    /// The lines and corners of a bordered element (an overlay, a panel):
    /// rounded or square as the theme's radius says, in the symbols the
    /// terminal shows.
    pub fn borders(&self) -> border::Set<'static> {
        match glyphs::symbols() {
            Symbols::Ascii => border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            },
            // (The classic console's fonts have no rounded corners.)
            Symbols::Basic => border::PLAIN,
            Symbols::Full => match self.corners {
                Corners::Square => border::PLAIN,
                Corners::Rounded => border::ROUNDED,
            },
        }
    }

    /// Code, in a line of text.
    pub fn code(&self) -> Style {
        if self.is_terminal() {
            Style::new().fg(self.accent)
        } else {
            Style::new().fg(self.text).bg(self.surface)
        }
    }
}

/// A variant's roles, as colours.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Roles {
    background: Rgb,
    text: Rgb,
    dim: Rgb,
    accent: Rgb,
    fill: Rgb,
    on_fill: Rgb,
    surface: Rgb,
    raised: Rgb,
    border: Rgb,
    selection: Rgb,
    user: Rgb,
    warning: Rgb,
    error: Rgb,
    success: Rgb,
}

/// The contrast text needs on its background (WCAG AA).
const READABLE: f64 = 4.5;

impl Roles {
    fn of(variant: &Variant) -> Self {
        let raw = |key: &str| variant.colors.get(key).map(String::as_str);
        let background = raw("background")
            .and_then(|value| Rgb::parse(value, None))
            .unwrap_or(Rgb(0, 0, 0));
        let color = |key: &str| raw(key).and_then(|value| Rgb::parse(value, Some(background)));
        let text = color("foreground").unwrap_or(Rgb(255, 255, 255));
        // `color`, darker on a light background (lighter on a dark one)
        // until it reads there: its hue stays, so red still says red.
        let readable = |color: Rgb, on: Rgb| {
            let toward = if on.luminance() > 0.18 {
                Rgb(0, 0, 0)
            } else {
                Rgb(255, 255, 255)
            };
            let mut shade = color;
            let mut step = 0.0;
            while shade.contrast(on) < READABLE && step < 1.0 {
                step += 0.05;
                shade = color.mix(toward, step);
            }
            shade
        };
        let first = |keys: &[&str]| keys.iter().find_map(|key| color(key));
        let accent = ["link", "base.blue", "ring", "primary.background"]
            .iter()
            .filter_map(|key| color(key))
            .find(|accent| accent.contrast(background) >= READABLE)
            .unwrap_or_else(|| {
                readable(
                    first(&["link", "base.blue", "ring", "primary.background"]).unwrap_or(text),
                    background,
                )
            });
        let fill = first(&["primary.background"]).unwrap_or(accent);
        let on_fill = match color("primary.foreground") {
            Some(on) if on.contrast(fill) >= READABLE => on,
            _ => {
                let (white, black) = (Rgb(255, 255, 255), Rgb(0, 0, 0));
                if white.contrast(fill) >= black.contrast(fill) {
                    white
                } else {
                    black
                }
            }
        };
        let apart = |color: Rgb| color != background;
        let surface = [
            "list.even.background",
            "secondary.background",
            "muted.background",
        ]
        .iter()
        .filter_map(|key| color(key))
        .find(|surface| apart(*surface))
        .unwrap_or_else(|| background.mix(text, 0.08));
        let raised = first(&["popover.background"])
            .filter(|raised| apart(*raised))
            .unwrap_or(surface);
        let border = first(&["border"])
            .filter(|border| apart(*border))
            .unwrap_or_else(|| background.mix(text, 0.2));
        // The theme's own, else its blue faintly over the background: the
        // first that the text reads on.
        let selection = [
            color("selection.background"),
            color("list.active.background"),
        ]
        .into_iter()
        .flatten()
        .chain([0.25, 0.18, 0.12].map(|amount| background.mix(fill, amount)))
        .chain([surface])
        .find(|selection| apart(*selection) && text.contrast(*selection) >= READABLE)
        .unwrap_or(surface);
        // The theme's muted surface: every theme sets it a little apart from
        // its background, and never far enough to glare.
        let user = first(&["muted.background"])
            .filter(|user| apart(*user))
            .unwrap_or_else(|| background.mix(text, 0.06));
        Self {
            background,
            text,
            dim: {
                // (Dim text has no hue to keep: toward the text.)
                let muted = first(&["muted.foreground"]).unwrap_or(background.mix(text, 0.6));
                let mut shade = muted;
                let mut step = 0.0;
                while shade.contrast(background) < READABLE && step < 1.0 {
                    step += 0.1;
                    shade = muted.mix(text, step);
                }
                shade
            },
            accent,
            fill,
            on_fill,
            surface,
            raised,
            border,
            selection,
            user,
            warning: readable(first(&["base.yellow"]).unwrap_or(text), background),
            error: readable(first(&["base.red"]).unwrap_or(text), background),
            success: readable(first(&["base.green"]).unwrap_or(text), background),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_with_alpha_blended_onto_the_background() {
        let white = Rgb(255, 255, 255);
        assert_eq!(Rgb::parse("#131313", None), Some(Rgb(0x13, 0x13, 0x13)));
        assert_eq!(
            Rgb::parse("#0060de20", Some(white)),
            Some(Rgb(223, 235, 251))
        );
        assert_eq!(Rgb::parse("linear-gradient(#fff, #000)", None), None);
        assert!((Rgb(0, 0, 0).contrast(white) - 21.0).abs() < 0.01);
    }

    /// The owner's examples: the background, the text and the blue.
    #[test]
    fn colours_map_to_the_nearest_of_xterms_256() {
        assert_eq!(Rgb(0x13, 0x13, 0x13).xterm256(), 233);
        assert_eq!(Rgb(0xDE, 0xDE, 0xDE).xterm256(), 253);
        assert_eq!(Rgb(0x07, 0x7C, 0xFD).xterm256(), 33);
        assert_eq!(Rgb(0xF9, 0xF9, 0xF9).xterm256(), 231);
    }

    /// Every bundled variant reads: its text, dim text, accent and
    /// warnings on its background, and the text on its filled accent.
    #[test]
    fn every_bundled_theme_gets_readable_roles() {
        let catalog = Catalog::load(None);
        assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);
        let variants: Vec<&Variant> = catalog
            .families
            .iter()
            .flat_map(|family| &family.variants)
            .collect();
        assert_eq!(variants.len(), 71);
        for variant in variants {
            let roles = Roles::of(variant);
            let on = |color: Rgb, background: Rgb, what: &str| {
                let ratio = color.contrast(background);
                assert!(
                    ratio >= READABLE - 0.01,
                    "{}: {what} {color:?} on {background:?} is {ratio:.2}",
                    variant.name
                );
            };
            on(roles.text, roles.background, "text");
            on(roles.dim, roles.background, "dim");
            on(roles.accent, roles.background, "accent");
            on(roles.warning, roles.background, "warning");
            on(roles.error, roles.background, "error");
            on(roles.success, roles.background, "success");
            on(roles.on_fill, roles.fill, "text on the accent");
            on(roles.text, roles.selection, "text on a selection");
        }
    }

    /// A colour too faint for text keeps its hue as it is made readable:
    /// Adventure Time's red (3.0:1 on its background), lightened, is still
    /// red.
    #[test]
    fn faint_colours_keep_their_hue() {
        let catalog = Catalog::load(None);
        let roles = Roles::of(catalog.find("Adventure Time").unwrap());
        let Rgb(r, g, b) = roles.error;
        assert!(r > g && r > b, "{:?}", roles.error);
        assert!(roles.error.contrast(roles.background) >= READABLE);
    }

    /// macOS Classic Dark takes its link blue as the accent, and its
    /// primary blue with white text as the fill.
    #[test]
    fn the_default_theme_maps_as_the_owner_chose() {
        let catalog = Catalog::load(None);
        let roles = Roles::of(catalog.find("macos classic dark").unwrap());
        assert_eq!(roles.background, Rgb(0x13, 0x13, 0x13));
        assert_eq!(roles.text, Rgb(0xDE, 0xDE, 0xDE));
        assert_eq!(roles.accent, Rgb(0x41, 0x9C, 0xFF));
        assert_eq!(roles.fill, Rgb(0x00, 0x6E, 0xE5));
        assert_eq!(roles.selection, Rgb(0x3F, 0x63, 0x8B));
        let light = Roles::of(catalog.find("macOS Classic Light").unwrap());
        assert_eq!(light.background, Rgb(0xF9, 0xF9, 0xF9));
        assert_eq!(light.text, Rgb(0, 0, 0));
        assert_eq!(light.accent, Rgb(0x00, 0x60, 0xDE));
        // The theme's selection is its blue at low alpha, blended.
        assert!(light.selection.contrast(light.background) > 1.1);
        assert_eq!(
            catalog.theme("nonsense", Depth::Rgb).name,
            "macOS Classic Dark"
        );
        assert_eq!(catalog.theme(DEFAULT, Depth::Ansi16), Theme::terminal());
        assert_eq!(
            catalog.theme(DEFAULT, Depth::Ansi256).background,
            Color::Indexed(233)
        );
    }

    #[test]
    fn the_users_themes_load_beside_the_bundled_and_bad_ones_are_reported() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("mine.json"),
            r##"{"name": "Mine", "themes": [{"name": "Mine Dark", "mode": "dark",
                "colors": {"background": "#000000", "foreground": "#ffffff"}}]}"##,
        )
        .unwrap();
        std::fs::write(temp.path().join("broken.json"), "{").unwrap();
        std::fs::write(
            temp.path().join("bare.json"),
            r#"{"name": "Bare", "themes": [{"name": "Bare", "colors": {}}]}"#,
        )
        .unwrap();
        let catalog = Catalog::load(Some(temp.path()));
        assert!(catalog.find("mine dark").is_some());
        assert_eq!(catalog.warnings.len(), 2, "{:?}", catalog.warnings);
        let theme = catalog.theme("Mine Dark", Depth::Rgb);
        assert_eq!(theme.background, Color::Rgb(0, 0, 0));
        assert_eq!(catalog.names().last().unwrap().1, TERMINAL);
    }
}
