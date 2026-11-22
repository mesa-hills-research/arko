//! Gives each build an ID from what it was built from (`ARKO_BUILD_ID`), and
//! writes the build's details to `build.json` in `OUT_DIR` for
//! `src/build_info.rs`.
//!
//! The ID is the package version with the first 12 hex digits of a SHA-256
//! over the version, profile, target and features, every file in `src/` and
//! `themes/`, `Cargo.toml`, `build.rs`, the `Cargo.lock` the build uses, and
//! the sources of each dependency built from a local path. Two builds share
//! an ID only when they were built from the same files, committed or not. The
//! version gets `-dev` when Git shows uncommitted changes to those files or a
//! dependency comes from a local path, so `0.24.0-dev+7b2e109f4c3a` is a
//! build from a work in progress and `0.24.0+a4f8c91d2e0b` one from a commit.

// What clippy.toml disallows is denied only in the full screen (src/tui).
#![allow(clippy::disallowed_methods, clippy::disallowed_macros)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// The dependencies whose versions every build records, overridden or not.
const TRACKED: [&str; 5] = [
    "arko_ai",
    "arko_agents",
    "arko_workflows",
    "arko_index",
    "termelon",
];

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let package = env::var("CARGO_PKG_NAME").unwrap();
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let profile = env::var("PROFILE").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    let mut features: Vec<String> = env::vars()
        .filter_map(|(key, _)| key.strip_prefix("CARGO_FEATURE_").map(feature_name))
        .collect();
    features.sort();

    let mut watched = BTreeSet::new();
    let mut hasher = Sha256::new();
    for field in [&version, &profile, &target, &features.join(",")] {
        hash_field(&mut hasher, field.as_bytes());
    }

    // The package's own files.
    let mut own = Vec::new();
    for name in ["src", "themes", "Cargo.toml", "build.rs"] {
        collect(&manifest_dir, &manifest_dir.join(name), &mut own);
        watched.insert(manifest_dir.join(name));
    }
    hash_files(&mut hasher, &own);

    // The lock the build uses. A build in a workspace (also one of an app
    // that depends on this package) uses the workspace's, beside its
    // `target` folder, which holds `OUT_DIR`. A packaged crate built
    // elsewhere has its own.
    let (lock_path, lock_text, lock) = out_dir
        .ancestors()
        .chain(manifest_dir.ancestors())
        .map(|dir| dir.join("Cargo.lock"))
        .filter(|path| path.is_file())
        .find_map(|path| {
            let text = fs::read_to_string(&path).ok()?;
            let lock = Lock::parse(&text);
            lock.find(&package, None, None)?;
            Some((Some(path), text, lock))
        })
        .unwrap_or_else(|| {
            (
                None,
                String::new(),
                Lock {
                    packages: Vec::new(),
                },
            )
        });
    if let Some(path) = &lock_path {
        watched.insert(path.clone());
    }
    hash_field(&mut hasher, lock_text.as_bytes());
    let used = lock.closure(&package);

    // Where each dependency built from a local path comes from, as Cargo's
    // configuration, a `[patch]`, or a dependency in a manifest says.
    let workspace = lock_path
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or(&manifest_dir)
        .to_path_buf();
    let mut declared = HashMap::new();
    let mut scanned = BTreeSet::new();
    for (file, base) in declaring_files(&workspace, &manifest_dir) {
        scan_declarations(&file, &base, &mut scanned, &mut watched, &mut declared);
    }

    // A local path dependency can itself pull in siblings from its own
    // manifest or workspace manifest, so keep scanning newly resolved crates
    // until every reachable local path dependency is found.
    let local_names: Vec<String> = used
        .iter()
        .map(|&index| &lock.packages[index])
        .filter(|entry| entry.name != package && entry.source.is_none())
        .map(|entry| entry.name.clone())
        .collect();
    loop {
        let before = declared.len();
        let dirs: Vec<PathBuf> = local_names
            .iter()
            .filter_map(|name| declared.get(name))
            .filter_map(|path| fs::canonicalize(path).ok())
            .collect();
        for dir in dirs {
            scan_declarations(
                &dir.join("Cargo.toml"),
                &dir,
                &mut scanned,
                &mut watched,
                &mut declared,
            );
            for ancestor in dir.ancestors().skip(1) {
                let candidate = ancestor.join("Cargo.toml");
                if candidate.is_file() {
                    if fs::read_to_string(&candidate).is_ok_and(|text| text.contains("[workspace]"))
                    {
                        scan_declarations(
                            &candidate,
                            ancestor,
                            &mut scanned,
                            &mut watched,
                            &mut declared,
                        );
                    }
                    break;
                }
            }
        }
        if declared.len() == before {
            break;
        }
    }

    let mut versions = BTreeMap::new();
    let mut overrides = Vec::new();
    let mut local = false;
    for entry in used.iter().map(|&index| &lock.packages[index]) {
        if entry.name == package {
            continue;
        }
        if TRACKED.contains(&entry.name.as_str()) {
            versions.insert(entry.name.clone(), entry.version.clone());
        }
        match &entry.source {
            Some(source) if source.starts_with("registry+") => {}
            Some(source) => overrides.push(json_object(&[
                ("name", json_string(&entry.name)),
                ("version", json_string(&entry.version)),
                ("source", json_string(source)),
            ])),
            None => {
                local = true;
                let dir = declared
                    .get(&entry.name)
                    .and_then(|path| fs::canonicalize(path).ok());
                let mut fields = vec![
                    ("name", json_string(&entry.name)),
                    ("version", json_string(&entry.version)),
                    ("source", json_string("path")),
                ];
                match &dir {
                    Some(dir) => {
                        let mut files = Vec::new();
                        for name in ["src", "Cargo.toml", "build.rs"] {
                            collect(dir, &dir.join(name), &mut files);
                            watched.insert(dir.join(name));
                        }
                        let mut crate_hasher = Sha256::new();
                        hash_files(&mut crate_hasher, &files);
                        let crate_hash = hex(&crate_hasher.finalize());
                        hash_field(&mut hasher, entry.name.as_bytes());
                        hash_field(&mut hasher, crate_hash.as_bytes());
                        let git = git_state(dir, &["src", "Cargo.toml", "build.rs"], &mut watched);
                        fields.push(("path", json_string(&dir.display().to_string())));
                        fields.push(("hash", json_string(&crate_hash)));
                        fields.push(("git", git_json(git.as_ref())));
                    }
                    // Not found where the manifests and configuration say,
                    // so only the lock's entry is known.
                    None => {
                        hash_field(&mut hasher, entry.name.as_bytes());
                        fields.push(("path", "null".into()));
                    }
                }
                overrides.push(json_object(&fields));
            }
        }
    }

    let git = git_state(
        &manifest_dir,
        &["src", "themes", "Cargo.toml", "Cargo.lock", "build.rs"],
        &mut watched,
    );
    let dirty = git.as_ref().is_some_and(|git| git.dirty);
    let source_hash = hex(&hasher.finalize());
    let dev = if dirty || local { "-dev" } else { "" };
    let id = format!("{version}{dev}+{}", &source_hash[..12]);

    let versions_json = json_object(
        &versions
            .iter()
            .map(|(name, version)| (name.as_str(), json_string(version)))
            .collect::<Vec<_>>(),
    );
    let features_json = format!(
        "[{}]",
        features
            .iter()
            .map(|feature| json_string(feature))
            .collect::<Vec<_>>()
            .join(",")
    );
    let details = json_object(&[
        ("id", json_string(&id)),
        ("version", json_string(&version)),
        ("source_hash", json_string(&source_hash)),
        ("profile", json_string(&profile)),
        ("target", json_string(&target)),
        ("features", features_json),
        (
            "rustc",
            rustc_version().map_or("null".into(), |text| json_string(&text)),
        ),
        ("git", git_json(git.as_ref())),
        ("dependencies", versions_json),
        ("overrides", format!("[{}]", overrides.join(","))),
    ]);
    fs::write(out_dir.join("build.json"), details).unwrap();

    println!("cargo:rustc-env=ARKO_BUILD_ID={id}");
    // A folder is watched with every file in it at any depth.
    for path in watched.iter().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// `CARGO_FEATURE_CODE_MODE` names `code-mode`.
fn feature_name(key: &str) -> String {
    key.to_ascii_lowercase().replace('_', "-")
}

/// Adds the files at `path` to `files` with their paths relative to `base`,
/// meaning the file itself, or each file in the folder at any depth. Names
/// that start with a dot (editors' swap files, `.DS_Store`) are left out.
fn collect(base: &Path, path: &Path, files: &mut Vec<(String, PathBuf)>) {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
    {
        return;
    }
    if path.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect(base, &entry.path(), files);
        }
    } else if path.is_file() {
        let relative = path.strip_prefix(base).unwrap_or(path);
        let name = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        files.push((name, path.to_path_buf()));
    }
}

/// Hashes each file's relative path and contents, in the order of the
/// paths, so the hash is the same on every machine and file system.
fn hash_files(hasher: &mut Sha256, files: &[(String, PathBuf)]) {
    let mut sorted: Vec<_> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, path) in sorted {
        hash_field(hasher, name.as_bytes());
        hash_field(hasher, &fs::read(path).unwrap_or_default());
    }
}

/// Hashes `bytes` with their length first, so no two sequences of fields
/// hash the same input.
fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The packages of a `Cargo.lock`.
struct Lock {
    packages: Vec<Package>,
}

struct Package {
    name: String,
    version: String,
    /// `None` for a package built from a local path.
    source: Option<String>,
    /// Each as the lock gives it, `name`, `name version` or
    /// `name version (source)`.
    dependencies: Vec<String>,
}

impl Lock {
    fn parse(text: &str) -> Lock {
        let mut packages = Vec::new();
        for entry in text.split("[[package]]").skip(1) {
            let mut package = Package {
                name: String::new(),
                version: String::new(),
                source: None,
                dependencies: Vec::new(),
            };
            let mut in_dependencies = false;
            for line in entry.lines().map(str::trim) {
                if in_dependencies {
                    if line.starts_with(']') {
                        in_dependencies = false;
                    } else if let Some(value) = quoted(line) {
                        package.dependencies.push(value);
                    }
                } else if let Some(value) = line.strip_prefix("name = ") {
                    package.name = quoted(value).unwrap_or_default();
                } else if let Some(value) = line.strip_prefix("version = ") {
                    package.version = quoted(value).unwrap_or_default();
                } else if let Some(value) = line.strip_prefix("source = ") {
                    package.source = quoted(value);
                } else if let Some(rest) = line.strip_prefix("dependencies = [") {
                    if let Some(inline) = rest.strip_suffix(']') {
                        for item in inline.split(',') {
                            if let Some(value) = quoted(item) {
                                package.dependencies.push(value);
                            }
                        }
                    } else {
                        in_dependencies = true;
                    }
                } else if line.starts_with('[') {
                    break;
                }
            }
            packages.push(package);
        }
        Lock { packages }
    }

    /// The indexes of `root` and every package it depends on at any depth.
    fn closure(&self, root: &str) -> BTreeSet<usize> {
        let mut seen = BTreeSet::new();
        let mut queue: Vec<usize> = self.find(root, None, None).into_iter().collect();
        while let Some(index) = queue.pop() {
            if !seen.insert(index) {
                continue;
            }
            for dependency in &self.packages[index].dependencies {
                let mut parts = dependency.splitn(3, ' ');
                let name = parts.next().unwrap_or_default();
                let version = parts.next();
                let source = parts
                    .next()
                    .map(|text| text.trim_matches(|c| c == '(' || c == ')'));
                if let Some(found) = self.find(name, version, source) {
                    queue.push(found);
                }
            }
        }
        seen
    }

    /// The package `name`, of `version` and `source` when the lock holds
    /// several.
    fn find(&self, name: &str, version: Option<&str>, source: Option<&str>) -> Option<usize> {
        let mut matches = self.packages.iter().enumerate().filter(|(_, package)| {
            package.name == name
                && version.is_none_or(|version| package.version == version)
                && source.is_none_or(|source| package.source.as_deref() == Some(source))
        });
        let first = matches.next()?;
        if source.is_none() {
            // When two packages share a name and version, Cargo omits the
            // source only for the local path one.
            if let Some(second) = matches.next() {
                return std::iter::once(first)
                    .chain(std::iter::once(second))
                    .chain(matches)
                    .find(|(_, package)| package.source.is_none())
                    .map(|(index, _)| index)
                    .or(Some(first.0));
            }
        }
        Some(first.0)
    }
}

/// The text of the first quoted TOML string (`"..."` or `'...'`) in `text`.
fn quoted(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let quote = chars.find(|c| matches!(c, '"' | '\''))?;
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c == quote {
            return Some(out);
        }
        if quote == '"' && c == '\\' {
            let escaped = chars.next()?;
            match escaped {
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else {
            out.push(c);
        }
    }
    None
}

fn scan_declarations(
    file: &Path,
    base: &Path,
    scanned: &mut BTreeSet<PathBuf>,
    watched: &mut BTreeSet<PathBuf>,
    declared: &mut HashMap<String, PathBuf>,
) {
    if !scanned.insert(file.to_path_buf()) {
        return;
    }
    if let Ok(text) = fs::read_to_string(file) {
        watched.insert(file.to_path_buf());
        for (name, path) in path_declarations(&text) {
            declared.entry(name).or_insert_with(|| base.join(path));
        }
    }
}

/// The files that may say where a dependency built from a local path is,
/// each with the folder its relative paths start from. A manifest's paths
/// start from its own folder, and those in `.cargo/config.toml` from the
/// folder that holds `.cargo`. Cargo reads the configuration in the
/// folder it runs in and every parent, then in `CARGO_HOME`.
fn declaring_files(workspace: &Path, manifest_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut files = Vec::new();
    for folder in manifest_dir.ancestors().chain(workspace.ancestors()) {
        for name in ["config.toml", "config"] {
            files.push((folder.join(".cargo").join(name), folder.to_path_buf()));
        }
    }
    let cargo_home = env::var_os("CARGO_HOME").map(PathBuf::from).or_else(|| {
        env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .map(|home| PathBuf::from(home).join(".cargo"))
    });
    if let Some(home) = cargo_home {
        for name in ["config.toml", "config"] {
            files.push((home.join(name), home.clone()));
        }
    }
    files.push((workspace.join("Cargo.toml"), workspace.to_path_buf()));
    files.push((manifest_dir.join("Cargo.toml"), manifest_dir.to_path_buf()));
    let mut seen = BTreeSet::new();
    files.retain(|(file, _)| seen.insert(file.clone()));
    files
}

/// Each dependency path declared in a manifest or configuration file, whether
/// as an inline table (`name = { ..., path = "..." }`) or as a dependency or
/// patch section (`[patch.crates-io.name]` with `path = "..."`).
fn path_declarations(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut section_name: Option<String> = None;
    let mut section_package: Option<String> = None;
    let mut section_path: Option<String> = None;
    let flush = |found: &mut Vec<(String, String)>,
                 name: &mut Option<String>,
                 package: &mut Option<String>,
                 path: &mut Option<String>| {
        if let (Some(name), Some(path)) = (package.take().or_else(|| name.take()), path.take()) {
            found.push((name, path));
        }
        *name = None;
        *package = None;
        *path = None;
    };

    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            flush(
                &mut found,
                &mut section_name,
                &mut section_package,
                &mut section_path,
            );
            if !line.starts_with("[[") {
                let header = strip_comment(line)
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .trim();
                section_name = dependency_section_name(header);
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches(|c| matches!(c, '"' | '\''));
        let value = value.trim();
        if value.starts_with('{') {
            let Some(at) = find_key(value, "path") else {
                continue;
            };
            let Some(path) = quoted(&value[at..]) else {
                continue;
            };
            let name = find_key(value, "package")
                .and_then(|at| quoted(&value[at..]))
                .unwrap_or_else(|| key.to_string());
            found.push((name, path));
        } else if section_name.is_some() {
            if key == "path" {
                section_path = quoted(value);
            } else if key == "package" {
                section_package = quoted(value);
            }
        }
    }
    flush(
        &mut found,
        &mut section_name,
        &mut section_package,
        &mut section_path,
    );
    found
}

/// Drops a trailing `#` comment outside quoted strings.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escape = false;
    for (index, c) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if let Some(q) = quote {
            if q == '"' && c == '\\' {
                escape = true;
            } else if c == q {
                quote = None;
            }
        } else if matches!(c, '"' | '\'') {
            quote = Some(c);
        } else if c == '#' {
            return &line[..index];
        }
    }
    line
}

/// Splits a dotted TOML header into key segments, keeping dots inside quotes.
fn split_header_keys(header: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escape = false;
    for c in header.chars() {
        if escape {
            current.push(c);
            escape = false;
            continue;
        }
        if let Some(q) = quote {
            if q == '"' && c == '\\' {
                escape = true;
            } else if c == q {
                quote = None;
            } else {
                current.push(c);
            }
        } else if matches!(c, '"' | '\'') {
            quote = Some(c);
        } else if c == '.' {
            parts.push(current.trim().to_string());
            current.clear();
        } else {
            current.push(c);
        }
    }
    let last = current.trim();
    if !last.is_empty() {
        parts.push(last.to_string());
    }
    parts
}

/// The dependency name from a `[patch.<source>.<name>]` or
/// `[...dependencies.<name>]` table header, or `None` for other sections.
fn dependency_section_name(header: &str) -> Option<String> {
    let parts = split_header_keys(header);
    if parts.len() >= 3 && parts[0] == "patch" {
        return parts.last().cloned();
    }
    if parts.len() >= 2
        && matches!(
            parts[parts.len() - 2].as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        )
    {
        return parts.last().cloned();
    }
    None
}

/// Where the value of `key` starts in an inline table (`{ path = "../x" }`).
fn find_key(table: &str, key: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = table[from..].find(key) {
        let at = from + offset;
        let before = table[..at].chars().next_back();
        let rest = table[at + key.len()..].trim_start();
        if matches!(before, Some('{' | ',' | ' ')) && rest.starts_with('=') {
            return Some(at + key.len());
        }
        from = at + key.len();
    }
    None
}

/// A Git checkout's commit, and whether the files a build reads have
/// uncommitted changes.
struct GitState {
    commit: String,
    dirty: bool,
}

fn git_json(git: Option<&GitState>) -> String {
    match git {
        Some(git) => json_object(&[
            ("commit", json_string(&git.commit)),
            ("dirty", git.dirty.to_string()),
        ]),
        None => "null".into(),
    }
}

/// The commit `dir` is checked out at and whether `paths` (relative to it)
/// have uncommitted changes, untracked files included, or `None` outside a
/// Git checkout that tracks `dir`'s `Cargo.toml` (a packaged crate, a copy
/// without `.git`). Watches the files that change on a commit, checkout or
/// index update, so the next build sees it.
fn git_state(dir: &Path, paths: &[&str], watched: &mut BTreeSet<PathBuf>) -> Option<GitState> {
    let git = |args: &[&str]| -> Option<String> {
        let output = Command::new("git")
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    git(&["ls-files", "--error-unmatch", "Cargo.toml"])?;
    let commit = git(&["rev-parse", "HEAD"])?;
    let mut status = vec!["status", "--porcelain", "--"];
    status.extend_from_slice(paths);
    let dirty = git(&status).is_none_or(|text| !text.is_empty());
    let mut refs = vec![
        "HEAD".to_string(),
        "index".to_string(),
        "packed-refs".to_string(),
        "logs/HEAD".to_string(),
    ];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        refs.push(branch);
    }
    for name in refs {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", &name]) {
            watched.insert(PathBuf::from(path));
        }
    }
    Some(GitState { commit, dirty })
}

fn rustc_version() -> Option<String> {
    let rustc = env::var_os("RUSTC")?;
    let output = Command::new(rustc).arg("-V").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON object of fields whose values are JSON already.
fn json_object(fields: &[(&str, String)]) -> String {
    let body: Vec<String> = fields
        .iter()
        .map(|(key, value)| format!("{}:{value}", json_string(key)))
        .collect();
    format!("{{{}}}", body.join(","))
}
