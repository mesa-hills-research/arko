//! `kind: auto`: a census of the file types under the index's rules (ignore
//! files honoured as arko_index honours them) decides between code and docs.

use std::path::Path;

use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::store::{Kind, Rules};

/// Files counted at most: enough to tell a tree's character.
pub const CENSUS_LIMIT: usize = 20_000;

/// What a census counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
    /// Prose files (Markdown, MDX, reStructuredText, AsciiDoc, text, Org).
    pub prose: u64,
    /// Source files.
    pub code: u64,
    /// Everything else (configuration, data, assets).
    pub other: u64,
    /// The walk stopped at [`CENSUS_LIMIT`].
    pub truncated: bool,
}

impl Census {
    /// Docs when at least two thirds of the prose and source files are prose;
    /// otherwise (an empty tree included) code.
    pub fn kind(&self) -> Kind {
        if self.prose > 0 && self.prose * 3 >= (self.prose + self.code) * 2 {
            Kind::Docs
        } else {
            Kind::Code
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} prose, {} source, {} other files{}",
            self.prose,
            self.code,
            self.other,
            if self.truncated { " (first files)" } else { "" }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Prose,
    Code,
    Other,
}

const PROSE: &[&str] = &[
    "md", "mdx", "markdown", "mdown", "mkd", "rst", "txt", "text", "adoc", "asciidoc", "asc",
    "org", "textile", "rdoc", "pod", "tex", "wiki",
];

const CODE: &[&str] = &[
    "rs", "py", "pyi", "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "go", "java", "kt",
    "kts", "scala", "sc", "groovy", "gradle", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx",
    "ipp", "cu", "cuh", "m", "mm", "cs", "fs", "fsi", "fsx", "vb", "swift", "rb", "rake", "php",
    "pl", "pm", "lua", "r", "jl", "dart", "ex", "exs", "erl", "hrl", "hs", "lhs", "ml", "mli",
    "elm", "clj", "cljs", "cljc", "edn", "zig", "nim", "odin", "v", "sv", "svh", "vhd", "vhdl",
    "sol", "sh", "bash", "zsh", "fish", "ps1", "psm1", "bat", "cmd", "sql", "proto", "graphql",
    "gql", "vue", "svelte", "astro", "css", "scss", "sass", "less", "html", "htm", "erb", "ejs",
    "hbs", "glsl", "vert", "frag", "hlsl", "wgsl", "tf", "hcl", "nix", "cmake", "mk", "f", "f90",
    "f95", "f03", "for", "pas", "ada", "adb", "ads", "cr", "d", "re", "rei", "purs", "tcl", "awk",
    "asm", "s",
];

const CODE_NAMES: &[&str] = &[
    "makefile",
    "gnumakefile",
    "dockerfile",
    "cmakelists.txt",
    "pkgbuild",
    "rakefile",
    "gemfile",
    "justfile",
    "build",
    "workspace",
    "vagrantfile",
];

/// A file's class by its name.
pub fn classify(path: &Path) -> Class {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if CODE_NAMES.contains(&name.as_str()) {
        return Class::Code;
    }
    let extension = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase());
    match extension.as_deref() {
        Some(ext) if PROSE.contains(&ext) => Class::Prose,
        Some(ext) if CODE.contains(&ext) => Class::Code,
        _ => Class::Other,
    }
}

fn matcher(root: &Path, patterns: &[String]) -> Result<Option<Gitignore>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        builder
            .add_line(None, pattern)
            .map_err(|error| format!("invalid pattern {pattern:?}: {error}"))?;
    }
    builder.build().map(Some).map_err(|error| error.to_string())
}

/// Counts the files under `root` that `rules` select, up to `limit`: `.git`
/// folders and folders with a `CACHEDIR.TAG` are skipped, ignore files are
/// read when the rules honour them (not those above the root, nor the global
/// one), and symbolic links aren't followed.
pub fn census(root: &Path, rules: &Rules, limit: usize) -> Result<Census, String> {
    let exclude = matcher(root, &rules.exclude)?;
    let include = matcher(root, &rules.include)?;
    let honour = rules.respect_ignore;
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .hidden(false)
        .parents(false)
        .ignore(honour)
        .git_ignore(honour)
        .git_exclude(honour)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| {
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            if !is_dir {
                return true;
            }
            if entry.file_name() == ".git" {
                return false;
            }
            // The root is counted even when it is tagged, as arko_index indexes it.
            entry.depth() == 0 || !entry.path().join("CACHEDIR.TAG").is_file()
        });
    let mut census = Census::default();
    let mut counted = 0usize;
    for entry in builder.build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(exclude) = &exclude
            && exclude.matched_path_or_any_parents(path, false).is_ignore()
        {
            continue;
        }
        if let Some(include) = &include
            && !include.matched_path_or_any_parents(path, false).is_ignore()
        {
            continue;
        }
        match classify(path) {
            Class::Prose => census.prose += 1,
            Class::Code => census.code += 1,
            Class::Other => census.other += 1,
        }
        counted += 1;
        if counted >= limit {
            census.truncated = true;
            break;
        }
    }
    Ok(census)
}
