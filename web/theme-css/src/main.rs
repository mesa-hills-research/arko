//! Writes the page's colors as CSS variables from GPUI Kit's theme files
//! (the ones Arko Studio and the terminal use), and the list of themes the
//! page offers.
//!
//! `theme-css THEMES_FOLDER STYLE_FOLDER LIST.rs`
//!
//! - `STYLE_FOLDER/themes.css` has the default pair (macOS Classic Light
//!   and Dark, as in Studio), which follows the system's setting, and the
//!   rule of each highlight name a code block's spans name.
//! - `STYLE_FOLDER/all-themes.css` has every theme, each under
//!   `:root[data-theme="its-slug"]`. The page loads it once a theme is
//!   chosen.
//! - `LIST.rs` lists the themes (name, slug, dark), sorted by name within
//!   light and dark, as Studio lists them.
//!
//! Each variable takes the first of its tokens the theme sets, as the kit
//! falls back from one token to another, then the default theme's of the
//! same mode. Files are written only when they change, so a build that runs
//! this each time doesn't rebuild the page for nothing.

use std::path::Path;

use serde_json::Value;

// The highlight names, as Arko's server names a code block's spans.
#[allow(dead_code)]
#[path = "../../../src/remote/protocol.rs"]
mod protocol;

/// The page's variables, each with the tokens it takes, in order. A token
/// under `highlight.` is read from the theme's highlight colors.
const VARIABLES: &[(&str, &[&str])] = &[
    ("bg", &["background"]),
    ("bar", &["title_bar.background", "background"]),
    ("bar-line", &["title_bar.border", "border"]),
    ("surface", &["highlight.editor.background", "background"]),
    ("code", &["list.even.background", "muted.background"]),
    ("secondary", &["secondary.background", "muted.background"]),
    (
        "secondary-hover",
        &["secondary.hover.background", "secondary.background"],
    ),
    ("muted-bg", &["muted.background", "secondary.background"]),
    ("text", &["foreground"]),
    ("muted", &["muted.foreground"]),
    ("line", &["border"]),
    ("input-line", &["input.border", "border"]),
    ("accent", &["primary.background", "base.blue"]),
    ("on-accent", &["primary.foreground"]),
    ("link", &["link", "base.blue"]),
    ("tint", &["list.active.background"]),
    ("ring", &["ring", "base.blue"]),
    ("warn", &["warning.background", "base.yellow"]),
    ("warn-mark", &["base.yellow", "warning.background"]),
    ("error", &["base.red", "danger.background"]),
    ("success", &["success.background", "base.green"]),
    (
        "thumb",
        &["scrollbar.thumb.background", "accent.background"],
    ),
    (
        "thumb-hover",
        &[
            "scrollbar.thumb.hover.background",
            "scrollbar.thumb.background",
        ],
    ),
    ("switch-off", &["switch.background", "secondary.background"]),
    ("switch-thumb", &["switch.thumb.background", "background"]),
    ("popover", &["popover.background", "background"]),
    ("popover-text", &["popover.foreground", "foreground"]),
];

const DEFAULT_FILE: &str = "macos-classic.json";
const DEFAULT_LIGHT: &str = "macOS Classic Light";
const DEFAULT_DARK: &str = "macOS Classic Dark";

/// The kit's palette colors that theme files name (`red-600`).
const PALETTE: &[(&str, &str)] = &[
    ("red-400", "#F87171"),
    ("red-600", "#DC2626"),
    ("green-400", "#4ADE80"),
    ("green-600", "#16A34A"),
    ("blue-400", "#60A5FA"),
    ("blue-600", "#2563EB"),
    ("yellow-400", "#FACC15"),
    ("yellow-600", "#CA8A04"),
    ("purple-400", "#C084FC"),
    ("purple-600", "#9333EA"),
    ("cyan-400", "#22D3EE"),
    ("cyan-600", "#0891B2"),
];

struct Theme {
    name: String,
    slug: String,
    dark: bool,
    value: Value,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [folder, style, list] = args.as_slice() else {
        eprintln!("usage: theme-css THEMES_FOLDER STYLE_FOLDER LIST.rs");
        std::process::exit(2);
    };
    let mut themes = read_themes(Path::new(folder));
    themes.sort_by_key(|theme| (theme.dark, theme.name.to_lowercase()));
    let find = |name: &str| {
        themes
            .iter()
            .find(|theme| theme.name == name)
            .unwrap_or_else(|| fail(&format!("no theme is named {name}")))
    };
    let (light, dark) = (find(DEFAULT_LIGHT), find(DEFAULT_DARK));
    let fallback = |theme: &Theme| if theme.dark { dark } else { light };

    // A theme's syntax colors are its own: a chosen theme without one shows
    // the code's color, as in the apps. The dark default's rule overrides
    // the light one's, so it unsets what it doesn't style.
    let defaults = format!(
        "/* Written by web/theme-css from themes/{DEFAULT_FILE}. Don't edit. */\n\
         :root {{\n{}}}\n\
         :root:not([data-theme]) {{\n{}}}\n\
         @media (prefers-color-scheme: dark) {{\n  :root:not([data-theme]) {{\n{}{}  }}\n}}\n{}",
        block(light, light, "  "),
        syntax(light, "  ", false),
        block(dark, dark, "    "),
        syntax(dark, "    ", true),
        highlight_rules(),
    );
    write(&Path::new(style).join("themes.css"), &defaults);

    let mut all = String::from("/* Written by web/theme-css from themes/. Don't edit. */\n");
    for theme in &themes {
        all.push_str(&format!(
            ":root[data-theme=\"{}\"] {{\n{}{}}}\n",
            theme.slug,
            block(theme, fallback(theme), "  "),
            syntax(theme, "  ", false)
        ));
    }
    write(&Path::new(style).join("all-themes.css"), &all);

    let mut rust = String::from(
        "// Written by web/theme-css from themes/. Don't edit.\n\n\
         /// Every theme: its name, its slug (`data-theme`) and whether it's dark.\n\
         pub const THEMES: &[(&str, &str, bool)] = &[\n",
    );
    for theme in &themes {
        rust.push_str(&format!(
            "    ({:?}, {:?}, {}),\n",
            theme.name, theme.slug, theme.dark
        ));
    }
    rust.push_str("];\n");
    write(Path::new(list), &rust);
}

fn read_themes(folder: &Path) -> Vec<Theme> {
    let mut files: Vec<_> = std::fs::read_dir(folder)
        .unwrap_or_else(|error| fail(&format!("can't read {} ({error})", folder.display())))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    let mut themes = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| fail(&format!("can't read {} ({error})", path.display())));
        let family: Value = serde_json::from_str(&text)
            .unwrap_or_else(|error| fail(&format!("{} isn't JSON ({error})", path.display())));
        for value in family["themes"].as_array().into_iter().flatten() {
            let Some(name) = value["name"].as_str() else {
                continue;
            };
            themes.push(Theme {
                name: name.to_string(),
                slug: slug(name),
                dark: value["mode"] == "dark",
                value: value.clone(),
            });
        }
    }
    themes
}

/// `macOS Classic Dark` as `macos-classic-dark`.
fn slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

fn block(theme: &Theme, fallback: &Theme, indent: &str) -> String {
    let mut css = String::new();
    for (name, tokens) in VARIABLES {
        let color = if *name == "surface" {
            editor_background(theme)
        } else {
            [theme, fallback]
                .iter()
                .find_map(|theme| tokens.iter().find_map(|token| color(&theme.value, token)))
                .unwrap_or_else(|| fail(&format!("{} sets none of {tokens:?}", theme.name)))
        };
        css.push_str(&format!("{indent}--{name}: {color};\n"));
    }
    // A chosen segment's outline is the field border where a theme sets one
    // apart from its hairline. Elsewhere the page mixes one, as `initial`
    // leaves the variable unset (not the default theme's).
    let border = color(&theme.value, "border").unwrap_or_default();
    let outline = color(&theme.value, "input.border")
        .filter(|outline| !outline.eq_ignore_ascii_case(&border))
        .unwrap_or_else(|| "initial".into());
    css.push_str(&format!("{indent}--outline: {outline};\n"));
    // The kit's radii (6 and 8 unless the theme says, 0 squares them).
    for (name, key, default) in [("radius", "radius", 6.0), ("radius-lg", "radius.lg", 8.0)] {
        let radius = theme.value[key].as_f64().unwrap_or(default);
        css.push_str(&format!("{indent}--{name}: {radius}px;\n"));
    }
    let scheme = if theme.dark { "dark" } else { "light" };
    css.push_str(&format!("{indent}color-scheme: {scheme};\n"));
    css
}

/// The editor's surface, as GPUI Kit's `editor_background` gives it: the
/// theme's `highlight.editor.background`, else that of the kit's built-in
/// highlight colors when the theme has none at all, else the input
/// background (the page's in a light theme, the field border at 30 % over
/// it in a dark one).
fn editor_background(theme: &Theme) -> String {
    if let Some(color) = color(&theme.value, "highlight.editor.background") {
        return color;
    }
    if theme.value["highlight"].is_null() {
        // The editor background of the kit's default-theme.json.
        return if theme.dark { "#0a0a0a" } else { "#ffffff" }.into();
    }
    let background = color(&theme.value, "background")
        .unwrap_or_else(|| fail(&format!("{} has no background", theme.name)));
    if !theme.dark {
        return background;
    }
    let input = color(&theme.value, "input.border").or_else(|| color(&theme.value, "border"));
    match (input.as_deref().and_then(rgba), rgba(&background)) {
        (Some(input), Some(under)) => over(input, 0.3, under),
        _ => background,
    }
}

/// A `#rgb`, `#rrggbb` or `#rrggbbaa` color's channels, alpha last (0–1).
fn rgba(hex: &str) -> Option<[f64; 4]> {
    let hex = hex.strip_prefix('#')?;
    let digits: Vec<u8> = match hex.len() {
        3 | 4 => hex
            .chars()
            .flat_map(|c| [c, c])
            .collect::<String>()
            .into_bytes(),
        6 | 8 => hex.as_bytes().to_vec(),
        _ => return None,
    };
    let channel = |at: usize| -> Option<f64> {
        let pair = std::str::from_utf8(digits.get(at..at + 2)?).ok()?;
        Some(u8::from_str_radix(pair, 16).ok()? as f64 / 255.0)
    };
    Some([
        channel(0)?,
        channel(2)?,
        channel(4)?,
        if digits.len() == 8 { channel(6)? } else { 1.0 },
    ])
}

/// `color` at `opacity` (times its own alpha) painted over `under`, as `#rrggbb`.
fn over(color: [f64; 4], opacity: f64, under: [f64; 4]) -> String {
    let alpha = color[3] * opacity;
    let channel = |at: usize| {
        let value = color[at] * alpha + under[at] * (1.0 - alpha);
        (value * 255.0).round().clamp(0.0, 255.0) as u8
    };
    format!("#{:02X}{:02X}{:02X}", channel(0), channel(1), channel(2))
}

/// A highlight name as CSS names it (`string-escape`).
fn css_name(name: &str) -> String {
    name.replace('.', "-")
}

/// The theme's style of each highlight name, as variables: its own, else
/// its first word's (`string.escape` takes `string`'s), as the kit's themes
/// do. A name the theme gives no style has none. With `unset`, each
/// variable the theme leaves out is written as `initial`, so a rule it
/// overrides (the light default's, under the dark one) gives the code's
/// own color and not the other theme's.
fn syntax(theme: &Theme, indent: &str, unset: bool) -> String {
    let styles = &theme.value["highlight"]["syntax"];
    let style = |name: &str| {
        let own = [name.to_string(), name.replace('.', "_")]
            .into_iter()
            .find_map(|key| styles.get(&key).filter(|style| style.is_object()).cloned());
        own.or_else(|| styles.get(name.split('.').next()?).cloned())
    };
    let mut css = String::new();
    for name in protocol::HIGHLIGHT_NAMES {
        let style = style(name).unwrap_or_default();
        let name = css_name(name);
        let color = style["color"].as_str().and_then(palette_color);
        let font_style = style["font_style"]
            .as_str()
            .filter(|font_style| ["normal", "italic", "oblique"].contains(font_style))
            .map(str::to_string);
        let weight = style["font_weight"]
            .as_f64()
            .map(|weight| weight.to_string());
        for (suffix, value) in [("", color), ("-style", font_style), ("-weight", weight)] {
            if let Some(value) = value.or_else(|| unset.then(|| "initial".to_string())) {
                css.push_str(&format!("{indent}--syn-{name}{suffix}: {value};\n"));
            }
        }
    }
    css
}

/// The rule of each highlight name: the theme's style for it, else the
/// code's own.
fn highlight_rules() -> String {
    protocol::HIGHLIGHT_NAMES
        .iter()
        .map(|name| {
            let name = css_name(name);
            format!(
                ".hl-{name} {{ color: var(--syn-{name}); font-style: var(--syn-{name}-style); font-weight: var(--syn-{name}-weight); }}\n"
            )
        })
        .collect()
}

/// A token's color, as CSS. A gradient gives its first color, and a
/// palette name (`red-600`) the kit's color.
fn color(theme: &Value, token: &str) -> Option<String> {
    let value = match token.strip_prefix("highlight.") {
        Some(token) => &theme["highlight"][token],
        None => &theme["colors"][token],
    }
    .as_str()?;
    if value.starts_with('#') {
        return Some(value.to_string());
    }
    if value.starts_with("linear-gradient") {
        let at = value.find('#')?;
        let end = value[at..]
            .find(|c: char| !c.is_ascii_hexdigit() && c != '#')
            .map_or(value.len(), |end| at + end);
        return Some(value[at..end].to_string());
    }
    PALETTE
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, color)| color.to_string())
}

/// A color as a theme file writes it (`#0433ff`, a gradient, a palette
/// name), as CSS.
fn palette_color(value: &str) -> Option<String> {
    color(&serde_json::json!({ "colors": { "it": value } }), "it")
}

fn write(path: &Path, text: &str) {
    if std::fs::read_to_string(path).ok().as_deref() != Some(text) {
        std::fs::write(path, text)
            .unwrap_or_else(|error| fail(&format!("can't write {} ({error})", path.display())));
    }
}

fn fail(message: &str) -> ! {
    eprintln!("theme-css: {message}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(dark: bool, value: Value) -> Theme {
        Theme {
            name: "Test".into(),
            slug: "test".into(),
            dark,
            value,
        }
    }

    #[test]
    fn the_editor_surface_falls_back_as_the_kits_does() {
        let colors = serde_json::json!({ "background": "#101010", "input.border": "#707070" });
        let own = serde_json::json!({ "colors": colors, "highlight": { "editor.background": "#202020" } });
        assert_eq!(editor_background(&theme(true, own)), "#202020");
        // The input background: the page's in a light theme, the field
        // border at 30 % over it in a dark one.
        let unset = serde_json::json!({ "colors": colors, "highlight": {} });
        assert_eq!(editor_background(&theme(false, unset.clone())), "#101010");
        assert_eq!(editor_background(&theme(true, unset)), "#2D2D2D");
        // No highlight colors at all: the kit's built-in ones.
        let none = serde_json::json!({ "colors": colors });
        assert_eq!(editor_background(&theme(true, none.clone())), "#0a0a0a");
        assert_eq!(editor_background(&theme(false, none)), "#ffffff");
        // A translucent border counts its own alpha too.
        assert_eq!(
            over([1.0, 1.0, 1.0, 0.5], 0.3, [0.0, 0.0, 0.0, 1.0]),
            "#262626"
        );
    }

    /// The names of the custom properties that `css` sets (`--name:`) and
    /// reads (`var(--name`).
    fn properties(css: &str) -> (Vec<String>, Vec<String>) {
        let (mut set, mut read) = (Vec::new(), Vec::new());
        for (at, _) in css.match_indices("--") {
            let name: String = css[at + 2..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if name.is_empty() {
                continue;
            }
            let after = css[at + 2 + name.len()..].trim_start();
            if css[..at].ends_with("var(") {
                read.push(name);
            } else if after.starts_with(':') {
                set.push(name);
            }
        }
        (set, read)
    }

    /// Every variable the page's styles read is set by its own rules or by
    /// every theme, such as the muted and text colors a shimmer sweeps
    /// between, so a leftover name can't leave a color unset.
    #[test]
    fn the_page_reads_only_variables_that_something_sets() {
        let themes = read_themes(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes"));
        let find = |dark: bool| {
            themes
                .iter()
                .find(|theme| theme.name == if dark { DEFAULT_DARK } else { DEFAULT_LIGHT })
                .unwrap()
        };
        let (app, read) = properties(include_str!("../../style/app.css"));
        for theme in &themes {
            let block = block(theme, find(theme.dark), "");
            let (set, _) = properties(&block);
            let mode = if theme.dark { "dark" } else { "light" };
            assert!(
                block.contains(&format!("color-scheme: {mode};")),
                "{}",
                theme.name
            );
            for name in &read {
                // The syntax colors come from the highlight rules, not the block.
                assert!(
                    app.contains(name) || set.contains(name) || name.starts_with("syn-"),
                    "--{name} is set by neither the page's styles nor {}",
                    theme.name
                );
            }
        }
    }
}
