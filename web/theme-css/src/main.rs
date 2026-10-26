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
    // the code's color, as in the apps.
    let defaults = format!(
        "/* Written by web/theme-css from themes/{DEFAULT_FILE}. Don't edit. */\n\
         :root {{\n{}}}\n\
         :root:not([data-theme]) {{\n{}}}\n\
         @media (prefers-color-scheme: dark) {{\n  :root:not([data-theme]) {{\n{}{}  }}\n}}\n{}",
        block(light, light, "  "),
        syntax(light, "  "),
        block(dark, dark, "    "),
        syntax(dark, "    "),
        highlight_rules(),
    );
    write(&Path::new(style).join("themes.css"), &defaults);

    let mut all = String::from("/* Written by web/theme-css from themes/. Don't edit. */\n");
    for theme in &themes {
        all.push_str(&format!(
            ":root[data-theme=\"{}\"] {{\n{}{}}}\n",
            theme.slug,
            block(theme, fallback(theme), "  "),
            syntax(theme, "  ")
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
        let color = [theme, fallback]
            .iter()
            .find_map(|theme| tokens.iter().find_map(|token| color(&theme.value, token)))
            .unwrap_or_else(|| fail(&format!("{} sets none of {tokens:?}", theme.name)));
        css.push_str(&format!("{indent}--{name}: {color};\n"));
    }
    // The kit's radii (6 and 8 unless the theme says, 0 squares them).
    for (name, key, default) in [("radius", "radius", 6.0), ("radius-lg", "radius.lg", 8.0)] {
        let radius = theme.value[key].as_f64().unwrap_or(default);
        css.push_str(&format!("{indent}--{name}: {radius}px;\n"));
    }
    // A shimmer's highlight leans toward the background in a light theme
    // and toward the text in a dark one, as the kit's does.
    let (shine, scheme) = if theme.dark {
        ("var(--text)", "dark")
    } else {
        ("var(--bg)", "light")
    };
    css.push_str(&format!("{indent}--shine-toward: {shine};\n"));
    css.push_str(&format!("{indent}color-scheme: {scheme};\n"));
    css
}

/// A highlight name as CSS names it (`string-escape`).
fn css_name(name: &str) -> String {
    name.replace('.', "-")
}

/// The theme's style of each highlight name, as variables: its own, else
/// its first word's (`string.escape` takes `string`'s), as the kit's themes
/// do. A name the theme gives no style has none.
fn syntax(theme: &Theme, indent: &str) -> String {
    let styles = &theme.value["highlight"]["syntax"];
    let style = |name: &str| {
        let own = [name.to_string(), name.replace('.', "_")]
            .into_iter()
            .find_map(|key| styles.get(&key).filter(|style| style.is_object()).cloned());
        own.or_else(|| styles.get(name.split('.').next()?).cloned())
    };
    let mut css = String::new();
    for name in protocol::HIGHLIGHT_NAMES {
        let Some(style) = style(name) else { continue };
        let name = css_name(name);
        if let Some(color) = style["color"].as_str().and_then(palette_color) {
            css.push_str(&format!("{indent}--syn-{name}: {color};\n"));
        }
        if let Some(font_style) = style["font_style"]
            .as_str()
            .filter(|font_style| ["normal", "italic", "oblique"].contains(font_style))
        {
            css.push_str(&format!("{indent}--syn-{name}-style: {font_style};\n"));
        }
        if let Some(weight) = style["font_weight"].as_f64() {
            css.push_str(&format!("{indent}--syn-{name}-weight: {weight};\n"));
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
