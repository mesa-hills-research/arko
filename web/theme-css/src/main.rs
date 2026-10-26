//! Writes the page's colors as CSS variables from a theme file of GPUI Kit's
//! (the format Arko Studio reads), with a light and a dark theme from it.
//!
//! `theme-css THEMES_FOLDER OUT.css [FILE [LIGHT DARK]]`
//!
//! Each variable takes the first of its tokens the theme sets, as the kit
//! falls back from one token to another when a theme leaves one out. The
//! file is written only when it changes, so a build that runs this each
//! time doesn't rebuild the page for nothing.

use std::path::Path;

use serde_json::Value;

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
    ("popover", &["popover.background", "background"]),
    ("popover-text", &["popover.foreground", "foreground"]),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [folder, out, rest @ ..] = args.as_slice() else {
        eprintln!("usage: theme-css THEMES_FOLDER OUT.css [FILE [LIGHT DARK]]");
        std::process::exit(2);
    };
    let file = rest.first().map_or("macos-classic.json", String::as_str);
    let light = rest.get(1).map_or("macOS Classic Light", String::as_str);
    let dark = rest.get(2).map_or("macOS Classic Dark", String::as_str);
    let path = Path::new(folder).join(file);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| fail(&format!("can't read {} ({error})", path.display())));
    let family: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| fail(&format!("{} isn't JSON ({error})", path.display())));
    let theme = |name: &str| {
        family["themes"]
            .as_array()
            .and_then(|themes| themes.iter().find(|theme| theme["name"] == name))
            .unwrap_or_else(|| fail(&format!("{} has no theme named {name}", path.display())))
    };
    let css = format!(
        "/* Written by web/theme-css from themes/{file} ({light}, {dark}). Don't edit. */\n\
         :root {{\n{}}}\n\
         @media (prefers-color-scheme: dark) {{\n  :root:not([data-theme=\"light\"]) {{\n{}  }}\n}}\n\
         :root[data-theme=\"dark\"] {{\n{}}}\n",
        block(theme(light), "  "),
        block(theme(dark), "    "),
        block(theme(dark), "  "),
    );
    if std::fs::read_to_string(out).ok().as_deref() != Some(css.as_str()) {
        std::fs::write(out, css)
            .unwrap_or_else(|error| fail(&format!("can't write {out} ({error})")));
    }
}

fn block(theme: &Value, indent: &str) -> String {
    let mut css = String::new();
    for (name, tokens) in VARIABLES {
        let color = tokens
            .iter()
            .find_map(|token| color(theme, token))
            .unwrap_or_else(|| fail(&format!("the theme sets none of {tokens:?}")));
        css.push_str(&format!("{indent}--{name}: {color};\n"));
    }
    let scheme = if theme["mode"] == "dark" {
        "dark"
    } else {
        "light"
    };
    css.push_str(&format!("{indent}color-scheme: {scheme};\n"));
    css
}

fn color<'a>(theme: &'a Value, token: &str) -> Option<&'a str> {
    let value = match token.strip_prefix("highlight.") {
        Some(token) => &theme["highlight"][token],
        None => &theme["colors"][token],
    };
    value.as_str().filter(|value| value.starts_with('#'))
}

fn fail(message: &str) -> ! {
    eprintln!("theme-css: {message}");
    std::process::exit(1);
}
