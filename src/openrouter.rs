//! OpenRouter's routing settings and its list of models, for every front end.
//!
//! The settings are one file in Arko's folder, `openrouter.json`, which
//! [`Settings`] reads and writes. They say which providers may serve a
//! request (zero data retention, no data collection, a region, the providers
//! to use or to leave out, how to order them, how precise they are). Every
//! OpenRouter client Arko builds carries them, and a change saved here
//! reaches the sessions open in this process from their next request. Another
//! Arko process reads them when its next session starts.
//!
//! The list of models is OpenRouter's own (`GET /models`, which needs no
//! key), filtered by what the settings rule out. It is kept in memory for the
//! process and fetched again when it is older than ten minutes. Reading it
//! with [`catalog`] never waits for the network. [`refresh`] fetches it when
//! it is due, and a request to a model never waits on either.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use arko_ai::providers::openrouter::Quantization;
use arko_ai::{
    DataCollection, Model, OpenRouterCatalog, OpenRouterClient, OpenRouterOptions,
    ProviderPreferences, SortBy,
};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
use serde_json::{Map, Value};

/// The file in Arko's folder that holds the [`Settings`].
pub const FILE: &str = "openrouter.json";

/// How many models [`Settings::recent`] keeps.
pub const RECENT: usize = 8;

/// How long the list of models is good for.
const FRESH: Duration = Duration::from_secs(10 * 60);

/// How long Arko waits after a failed fetch before it tries again by itself.
const RETRY: Duration = Duration::from_secs(30);

/// How long Arko waits for OpenRouter's list.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The most a list may hold once unpacked, in bytes.
const MOST: u64 = 64 * 1024 * 1024;

/// OpenRouter's address, and its regional ones.
const HOST: &str = arko_ai::providers::openrouter::OPENROUTER_BASE_URL;
const US_HOST: &str = "https://us.openrouter.ai/api/v1";
const EU_HOST: &str = "https://eu.openrouter.ai/api/v1";

// ----- the choices -----

/// Where OpenRouter may serve requests from. A region is a different address
/// (`us.openrouter.ai`), which OpenRouter offers on its Business and
/// Enterprise plans, and fewer models are available in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Region {
    #[default]
    Any,
    Us,
    Eu,
    /// Each model is served from the US when OpenRouter offers it there, else
    /// from the EU, and from nowhere else.
    UsOrEu,
}

impl Region {
    pub const ALL: [Region; 4] = [Region::Any, Region::Us, Region::Eu, Region::UsOrEu];

    pub fn label(self) -> &'static str {
        match self {
            Region::Any => "Any",
            Region::Us => "US",
            Region::Eu => "EU",
            Region::UsOrEu => "US or EU",
        }
    }

    /// The word `/openrouter region` takes.
    pub fn word(self) -> &'static str {
        match self {
            Region::Any => "any",
            Region::Us => "us",
            Region::Eu => "eu",
            Region::UsOrEu => "us-or-eu",
        }
    }

    fn is_any(&self) -> bool {
        *self == Region::Any
    }
}

/// A regional address a request goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Zone {
    Us,
    Eu,
}

impl Zone {
    pub fn label(self) -> &'static str {
        match self {
            Zone::Us => "US",
            Zone::Eu => "EU",
        }
    }

    /// The address that serves the models in this zone.
    pub fn base_url(self) -> &'static str {
        match self {
            Zone::Us => US_HOST,
            Zone::Eu => EU_HOST,
        }
    }
}

/// How OpenRouter orders the providers that can serve a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// OpenRouter chooses.
    #[default]
    Default,
    Price,
    Throughput,
    Latency,
}

impl Sort {
    pub const ALL: [Sort; 4] = [Sort::Default, Sort::Price, Sort::Throughput, Sort::Latency];

    pub fn label(self) -> &'static str {
        match self {
            Sort::Default => "Default",
            Sort::Price => "Price",
            Sort::Throughput => "Throughput",
            Sort::Latency => "Latency",
        }
    }

    /// The word `/openrouter sort` takes.
    pub fn word(self) -> &'static str {
        match self {
            Sort::Default => "default",
            Sort::Price => "price",
            Sort::Throughput => "throughput",
            Sort::Latency => "latency",
        }
    }

    fn by(self) -> Option<SortBy> {
        match self {
            Sort::Default => None,
            Sort::Price => Some(SortBy::Price),
            Sort::Throughput => Some(SortBy::Throughput),
            Sort::Latency => Some(SortBy::Latency),
        }
    }
}

/// How precise the weights a provider serves must be at least. `unknown`
/// stays allowed in both, because the providers of closed models report no
/// precision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Precision {
    #[default]
    Any,
    /// 8-bit floating point or more.
    Fp8,
    /// 16-bit floating point or more.
    Bf16,
    /// A list written by hand in the file, which Arko keeps as it is. It
    /// can't be chosen.
    Custom,
}

impl Precision {
    /// The choices.
    pub const ALL: [Precision; 3] = [Precision::Any, Precision::Fp8, Precision::Bf16];

    pub fn label(self) -> &'static str {
        match self {
            Precision::Any => "Any",
            Precision::Fp8 => "8-bit or more",
            Precision::Bf16 => "16-bit or more",
            Precision::Custom => "Custom",
        }
    }

    /// The word `/openrouter precision` takes.
    pub fn word(self) -> &'static str {
        match self {
            Precision::Any => "any",
            Precision::Fp8 => "fp8",
            Precision::Bf16 => "bf16",
            Precision::Custom => "custom",
        }
    }

    /// The `quantizations` the choice sends.
    fn quantizations(self) -> Option<Vec<Quantization>> {
        use Quantization::*;
        match self {
            Precision::Any | Precision::Custom => None,
            Precision::Fp8 => Some(vec![Fp8, Mxfp8, Fp16, Bf16, Fp32, Unknown]),
            Precision::Bf16 => Some(vec![Fp16, Bf16, Fp32, Unknown]),
        }
    }

    fn of(quantizations: Option<&[Quantization]>) -> Self {
        let Some(list) = quantizations else {
            return Precision::Any;
        };
        let same = |choice: Precision| {
            choice.quantizations().is_some_and(|wanted| {
                wanted.len() == list.len() && wanted.iter().all(|one| list.contains(one))
            })
        };
        [Precision::Fp8, Precision::Bf16]
            .into_iter()
            .find(|choice| same(*choice))
            .unwrap_or(Precision::Custom)
    }
}

/// One of the seven choices the settings show, for a front end that lists
/// them all alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Zdr,
    DataCollection,
    Region,
    Sort,
    Precision,
    Only,
    Ignore,
}

impl Control {
    pub const ALL: [Control; 7] = [
        Control::Zdr,
        Control::DataCollection,
        Control::Region,
        Control::Sort,
        Control::Precision,
        Control::Only,
        Control::Ignore,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Control::Zdr => "Zero data retention",
            Control::DataCollection => "No data collection",
            Control::Region => "Region",
            Control::Sort => "Providers by",
            Control::Precision => "Precision",
            Control::Only => "Only these providers",
            Control::Ignore => "Never these providers",
        }
    }

    /// What the choice does, in a sentence or two.
    pub fn about(self) -> &'static str {
        match self {
            Control::Zdr => {
                "Only providers that keep no data. The model list shows only the models that offer it."
            }
            Control::DataCollection => {
                "Skips providers that store your prompts or may train on them. Zero data retention is stricter."
            }
            Control::Region => {
                "Keeps requests in a region. It needs OpenRouter's Business or Enterprise plan, and the model list shows only the models offered there. US or EU uses the US for the models offered there, and the EU for the rest."
            }
            Control::Sort => "How OpenRouter orders the providers. Default lets it choose.",
            Control::Precision => "The least precise weights a provider may serve.",
            Control::Only => {
                "Provider slugs separated by commas, such as anthropic, google-vertex. No other provider serves a request."
            }
            Control::Ignore => "Provider slugs separated by commas. These never serve a request.",
        }
    }

    /// The word `/openrouter` takes for it.
    pub fn word(self) -> &'static str {
        match self {
            Control::Zdr => "zdr",
            Control::DataCollection => "data-collection",
            Control::Region => "region",
            Control::Sort => "sort",
            Control::Precision => "precision",
            Control::Only => "only",
            Control::Ignore => "ignore",
        }
    }

    /// The choice as `settings` has it, for a row.
    pub fn value(self, settings: &Settings) -> String {
        let on = |on: bool| if on { "On" } else { "Off" }.to_string();
        let list = |slugs: &[String], none: &str| match slugs {
            [] => none.to_string(),
            slugs => slugs.join(", "),
        };
        match self {
            Control::Zdr => on(settings.zdr()),
            Control::DataCollection => {
                on(settings.deny_data_collection() || settings.data_collection_implied())
            }
            Control::Region => settings.region().label().to_string(),
            Control::Sort => settings.sort().label().to_string(),
            Control::Precision => settings.precision().label().to_string(),
            Control::Only => list(settings.only(), "Any"),
            Control::Ignore => list(settings.ignore(), "None"),
        }
    }
}

// ----- the settings -----

/// The routing settings, as `openrouter.json` holds them. A file written by
/// hand may hold more than the seven choices (`max_price`, `allow_fallbacks`
/// and the rest of OpenRouter's `provider` object), and Arko keeps it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Region::is_any")]
    region: Region,
    #[serde(default, skip_serializing_if = "ProviderPreferences::is_empty")]
    provider: ProviderPreferences,
    /// The models picked, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recent: Vec<Recent>,
    /// Whatever else the file holds.
    #[serde(flatten)]
    other: Map<String, Value>,
}

/// A model picked, with what OpenRouter said of it then, so that the models
/// picked show their details and size a handover without the list of models.
/// The file holds a slug alone or an object with `slug`, `label`, `context`,
/// `input_price` and `output_price`.
#[derive(Debug, Clone, PartialEq)]
struct Recent {
    slug: String,
    label: Option<String>,
    context: Option<u64>,
    input_price: Option<f64>,
    output_price: Option<f64>,
}

/// How a [`Recent`] is written, as a slug alone or as an object.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RecentFile {
    Slug(String),
    Details {
        slug: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_price: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_price: Option<f64>,
    },
}

impl From<RecentFile> for Recent {
    fn from(file: RecentFile) -> Self {
        match file {
            RecentFile::Slug(slug) => Recent::new(slug),
            RecentFile::Details {
                slug,
                label,
                context,
                input_price,
                output_price,
            } => Recent {
                slug,
                label,
                context,
                input_price,
                output_price,
            },
        }
    }
}

impl From<&Recent> for RecentFile {
    fn from(recent: &Recent) -> Self {
        if recent.label.is_none()
            && recent.context.is_none()
            && recent.input_price.is_none()
            && recent.output_price.is_none()
        {
            return RecentFile::Slug(recent.slug.clone());
        }
        RecentFile::Details {
            slug: recent.slug.clone(),
            label: recent.label.clone(),
            context: recent.context,
            input_price: recent.input_price,
            output_price: recent.output_price,
        }
    }
}

impl Serialize for Recent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        RecentFile::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Recent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        RecentFile::deserialize(deserializer).map(Recent::from)
    }
}

impl Recent {
    fn new(slug: String) -> Self {
        Self {
            slug,
            label: None,
            context: None,
            input_price: None,
            output_price: None,
        }
    }

    /// The details `listed` gives (those it has).
    fn of(listed: &Listed) -> Self {
        Self {
            slug: listed.slug().to_string(),
            label: Some(listed.label.clone()),
            context: listed.context,
            input_price: listed.input_price,
            output_price: listed.output_price,
        }
    }

    fn listed(&self) -> Listed {
        Listed {
            name: format!("{}{}", crate::providers::OPENROUTER, self.slug),
            label: self.label.clone().unwrap_or_else(|| {
                crate::models::display_name(&format!("openrouter:{}", self.slug))
            }),
            context: self.context,
            input_price: self.input_price,
            output_price: self.output_price,
            zone: None,
        }
    }
}

/// What a front end passes to [`Settings::set_only`] and
/// [`Settings::set_ignore`], read from text (`anthropic, google-vertex`).
pub fn slugs(text: &str) -> Vec<String> {
    let mut slugs: Vec<String> = Vec::new();
    for slug in text
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|slug| slug.trim().to_lowercase())
        .filter(|slug| !slug.is_empty() && slug != "none")
    {
        if !slugs.contains(&slug) {
            slugs.push(slug);
        }
    }
    slugs
}

impl Settings {
    /// The settings in Arko's folder `root`, or none set when the file is
    /// missing or can't be read ([`read`](Self::read) says why).
    pub fn load(root: &Path) -> Self {
        Self::read(root).unwrap_or_else(|error| {
            log::warn!("openrouter: {error}");
            // (Known, for what only needs to know which models to list, and
            // not read again at every draw.)
            let settings = Self::default();
            crate::lock(&CACHE).insert(folder(root), settings.clone());
            settings
        })
    }

    /// The settings in Arko's folder `root`. A missing file means none set.
    /// Any file there that isn't settings, an empty one too, is an error that
    /// says so.
    pub fn read(root: &Path) -> Result<Self, String> {
        #[cfg(test)]
        {
            *crate::lock(&READS).entry(folder(root)).or_default() += 1;
        }
        let path = root.join(FILE);
        let settings = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| format!("{} can't be read ({error})", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(format!("{} can't be read ({error})", path.display())),
        };
        crate::lock(&CACHE).insert(folder(root), settings.clone());
        Ok(settings)
    }

    /// The settings as this process last read or saved them, with no file
    /// access, or `None` before it has. Nothing that draws reads a file, so
    /// it asks this (see [`settings_in_memory`]).
    pub(crate) fn cached(root: &Path) -> Option<Self> {
        crate::lock(&CACHE).get(&folder(root)).cloned()
    }

    /// [`read`](Self::read), for what must not go by a guess, with what to do
    /// when the file can't be read.
    pub(crate) fn read_to_use(root: &Path) -> Result<Self, String> {
        Self::read(root).map_err(|error| {
            format!("{error}. Fix the file or remove it, since it holds the OpenRouter settings.")
        })
    }

    /// Changes the settings in Arko's folder `root`, as the file has them
    /// now, and returns what is saved. `change` edits the settings read
    /// under a lock that every change to the file takes, the models picked
    /// and the other Arko processes' too, so two changes made at once both
    /// stay, and what `change` doesn't touch stays as it is. The sessions
    /// open in this process use the new settings from their next request,
    /// and have them installed when this returns. A file that can't be read
    /// is an error, and nothing is written.
    pub fn update(root: &Path, change: impl FnOnce(&mut Settings)) -> Result<Settings, String> {
        Self::edit(root, 0, change).map(|(settings, ())| settings)
    }

    /// [`update`](Self::update) for the session numbered `origin` (see
    /// `Keys::id`), which isn't told of its own change in a notice, and
    /// with what `change` returns.
    pub(crate) fn edit<R>(
        root: &Path,
        origin: u64,
        change: impl FnOnce(&mut Settings) -> R,
    ) -> Result<(Settings, R), String> {
        // (A wait for the folder's lock is bounded, the other processes'
        // too, so a change that can't be made says so.)
        let give_up = Instant::now() + lock_wait();
        let _here = lock_root_by(root, give_up)?;
        std::fs::create_dir_all(root)
            .map_err(|error| format!("Couldn't save the settings ({error})."))?;
        let _elsewhere = FileLock::acquire(root, give_up)?;
        let mut settings = Self::read_to_use(root)?;
        #[cfg(test)]
        pause(root);
        let before = settings.clone();
        let result = change(&mut settings);
        if settings != before {
            let text = serde_json::to_string_pretty(&settings)
                .map_err(|error| format!("Couldn't save the settings ({error})."))?;
            crate::session::write_whole(&root.join(FILE), (text + "\n").as_bytes())
                .map_err(|error| format!("Couldn't save the settings ({error})."))?;
            crate::lock(&CACHE).insert(folder(root), settings.clone());
        }
        // The models picked change nothing a request carries.
        if settings.region != before.region || settings.provider != before.provider {
            notify(root, &settings, origin);
        }
        Ok((settings, result))
    }

    /// Saves the region and the `provider` these settings hold in Arko's
    /// folder `root`, over the file as it is now, so the models picked
    /// since they were read stay. Fields of the settings another change made
    /// meanwhile are written back as these have them, so a front end that
    /// changes one choice at a time uses [`update`](Self::update).
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        Self::update(root, |file| {
            file.region = self.region;
            file.provider = self.provider.clone();
        })
        .map(|_| ())
        .map_err(std::io::Error::other)
    }

    /// Zero data retention: only providers that keep no data serve a
    /// request, and the list of models shows only the models offered so.
    pub fn zdr(&self) -> bool {
        self.provider.zdr == Some(true)
    }

    pub fn set_zdr(&mut self, on: bool) {
        self.provider.zdr = on.then_some(true);
    }

    /// No data collection: only providers that don't collect prompts serve a
    /// request.
    pub fn deny_data_collection(&self) -> bool {
        self.provider.data_collection == Some(DataCollection::Deny)
    }

    /// Whether no data collection holds because zero data retention is on,
    /// which covers it. The file keeps the two apart, so a front end shows
    /// the choice on, and not to be changed, while this is so.
    pub fn data_collection_implied(&self) -> bool {
        self.zdr()
    }

    pub fn set_deny_data_collection(&mut self, on: bool) {
        self.provider.data_collection = on.then_some(DataCollection::Deny);
    }

    pub fn region(&self) -> Region {
        self.region
    }

    pub fn set_region(&mut self, region: Region) {
        self.region = region;
    }

    pub fn sort(&self) -> Sort {
        match self.provider.sort.and_then(|sort| sort.by()) {
            Some(SortBy::Price) => Sort::Price,
            Some(SortBy::Throughput) => Sort::Throughput,
            Some(SortBy::Latency) => Sort::Latency,
            _ => Sort::Default,
        }
    }

    pub fn set_sort(&mut self, sort: Sort) {
        self.provider.sort = sort.by().map(Into::into);
    }

    pub fn precision(&self) -> Precision {
        Precision::of(self.provider.quantizations.as_deref())
    }

    /// Sets the precision. `Custom` can't be chosen, so it changes nothing.
    pub fn set_precision(&mut self, precision: Precision) {
        if precision != Precision::Custom {
            self.provider.quantizations = precision.quantizations();
        }
    }

    /// The only providers that may serve a request, none meaning any.
    pub fn only(&self) -> &[String] {
        self.provider.only.as_deref().unwrap_or_default()
    }

    pub fn set_only(&mut self, providers: Vec<String>) {
        self.provider.only = (!providers.is_empty()).then_some(providers);
    }

    /// The providers that never serve a request.
    pub fn ignore(&self) -> &[String] {
        self.provider.ignore.as_deref().unwrap_or_default()
    }

    pub fn set_ignore(&mut self, providers: Vec<String>) {
        self.provider.ignore = (!providers.is_empty()).then_some(providers);
    }

    /// The OpenRouter models picked, newest first, at most [`RECENT`], with
    /// what OpenRouter said of each when it was picked.
    pub fn recent(&self) -> Vec<Listed> {
        self.recent.iter().map(Recent::listed).collect()
    }

    /// The models picked as `/model` takes them (`openrouter:slug`).
    pub fn recent_names(&self) -> Vec<String> {
        self.recent
            .iter()
            .map(|recent| format!("{}{}", crate::providers::OPENROUTER, recent.slug))
            .collect()
    }

    /// Puts `slug` first among the models picked, with `listed`'s details
    /// (else those it had). Returns whether that changed the list.
    fn remember(&mut self, slug: &str, listed: Option<&Listed>) -> bool {
        let before = self.recent.clone();
        let earlier = self.recent.iter().find(|one| one.slug == slug).cloned();
        self.recent.retain(|one| one.slug != slug);
        let recent = match (listed, earlier) {
            (Some(listed), _) => Recent::of(listed),
            (None, Some(earlier)) => earlier,
            (None, None) => Recent::new(slug.to_string()),
        };
        self.recent.insert(0, recent);
        self.recent.truncate(RECENT);
        self.recent != before
    }

    /// Brings the details of the models picked up to date with `listed`.
    /// Returns whether any changed.
    fn refresh_recent(&mut self, listed: &[Listed]) -> bool {
        let mut changed = false;
        for recent in &mut self.recent {
            let Some(found) = listed.iter().find(|one| one.slug() == recent.slug) else {
                continue;
            };
            let now = Recent::of(found);
            if *recent != now {
                *recent = now;
                changed = true;
            }
        }
        changed
    }

    /// What is set, in a few words (`Zero data retention, US only`), or
    /// `None` when nothing is.
    pub fn summary(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.zdr() {
            parts.push("zero data retention".into());
        }
        if self.deny_data_collection() && !self.data_collection_implied() {
            parts.push("no data collection".into());
        }
        match self.region {
            Region::Any => {}
            Region::Us => parts.push("US only".into()),
            Region::Eu => parts.push("EU only".into()),
            Region::UsOrEu => parts.push("US or EU only".into()),
        }
        match self.sort() {
            Sort::Default => {}
            sort => parts.push(format!("{} first", sort.label().to_lowercase())),
        }
        match self.precision() {
            Precision::Any => {}
            Precision::Custom => parts.push("custom precision".into()),
            precision => parts.push(precision.label().to_lowercase()),
        }
        let few = |slugs: &[String]| match slugs.len() {
            0..=3 => crate::and_list(slugs),
            count => format!("{count} providers"),
        };
        if !self.only().is_empty() {
            parts.push(format!("only {}", few(self.only())));
        }
        if !self.ignore().is_empty() {
            parts.push(format!("never {}", few(self.ignore())));
        }
        let text = parts.join(", ");
        let mut chars = text.chars();
        let first = chars.next()?;
        Some(first.to_uppercase().chain(chars).collect())
    }

    /// What every OpenRouter request carries.
    pub fn options(&self) -> OpenRouterOptions {
        OpenRouterOptions {
            provider: (!self.provider.is_empty()).then(|| self.provider.clone()),
            ..OpenRouterOptions::default()
        }
    }

    /// The address that serves every model, when one does (not US or EU,
    /// where each model has its own).
    pub fn base_url(&self) -> &'static str {
        match self.region {
            Region::Any => HOST,
            Region::Us | Region::UsOrEu => US_HOST,
            Region::Eu => EU_HOST,
        }
    }

    /// The settings as lines, a choice to a line, for a front end without a
    /// menu. `models` is the line's value for the models, when it has one.
    pub fn lines(&self, models: Option<&str>) -> Vec<String> {
        let rows = Control::ALL
            .iter()
            .map(|control| (control.label(), control.value(self)))
            .chain(models.map(|models| ("Models", models.to_string())));
        let width = Control::ALL
            .iter()
            .map(|control| control.label().len())
            .max()
            .unwrap_or(0);
        rows.map(|(label, value)| format!("  {label:<width$}  {value}"))
            .collect()
    }

    /// What the settings rule out of the list of models (zero data retention
    /// and a region), in a few words, or `None` when nothing is.
    pub fn filters_summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.zdr() {
            parts.push("zero data retention");
        }
        match self.region {
            Region::Any => {}
            Region::Us => parts.push("US only"),
            Region::Eu => parts.push("EU only"),
            Region::UsOrEu => parts.push("US or EU only"),
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }

    /// What decides the list of models: the choices that rule models out.
    fn filters(&self, local: Option<&str>) -> Key {
        Key {
            local: local.map(str::to_string),
            region: self.region,
            zdr: self.zdr(),
        }
    }
}

// ----- the clients -----

/// A client for OpenRouter with `settings` (read from `root` when `None`).
/// Every client Arko builds comes from here, so that the settings reach
/// each request. A region sends the requests to its address. US or EU sends
/// each model to the address that offers it, as the list of models says.
pub(crate) fn client_with(key: &str, settings: &Settings, local: Option<&str>) -> OpenRouterClient {
    let client = OpenRouterClient::new(key).with_default_options(settings.options());
    match local {
        // (Tests: a local server serves everything.)
        Some(url) => client.with_base_url(url),
        None if settings.region == Region::UsOrEu => {
            // The US address, unless the models in memory say the EU's.
            client
                .with_base_url(US_HOST)
                .with_base_url_for(eu_only(settings.filters(None)))
        }
        None => client.with_base_url(settings.base_url()),
    }
}

/// Sends a model to the EU address when the models in memory for `key` put it
/// there, which is when the EU offers it and the US doesn't. It reads memory
/// and never waits, so a request before the models are loaded goes to the
/// US, which is the safe side.
fn eu_only(key: Key) -> impl Fn(&str) -> Option<String> + Send + Sync + 'static {
    move |slug| (zone_of(&key, slug) == Some(Zone::Eu)).then(|| EU_HOST.to_string())
}

/// The zone the models in memory put `slug` in, when they hold it.
fn zone_of(key: &Key, slug: &str) -> Option<Zone> {
    let memory = memory();
    let zones = &memory.get(key)?.loaded.as_ref()?.zones;
    zones
        .get(slug)
        .or_else(|| zones.get(slug.split_once(':')?.0))
        .copied()
}

// ----- one change at a time -----

/// The settings last read or saved for each Arko folder.
static CACHE: LazyLock<Mutex<HashMap<PathBuf, Settings>>> = LazyLock::new(Mutex::default);

/// The folder as the locks, the settings kept and the sessions know it (a
/// trailing slash or a `.` is no other folder).
fn folder(root: &Path) -> PathBuf {
    root.components().collect()
}

/// The lock for changes to the file of Arko's folder `root` in this process.
/// A session also takes it as a key is replaced, so that the key and the
/// settings it installs are the latest of both.
pub(crate) fn lock_root(root: &Path) -> std::sync::MutexGuard<'static, ()> {
    crate::lock(root_lock(root))
}

fn root_lock(root: &Path) -> &'static Mutex<()> {
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, &'static Mutex<()>>>> =
        LazyLock::new(Mutex::default);
    crate::lock(&LOCKS)
        .entry(folder(root))
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
}

/// How long a change waits for the folder's lock, in this process and in
/// others, before it says the settings are in use.
fn lock_wait() -> Duration {
    #[cfg(test)]
    if let Some(wait) = LOCK_WAIT.with(std::cell::Cell::get) {
        return wait;
    }
    Duration::from_secs(5)
}

/// [`lock_root`], waiting until `give_up` at most.
fn lock_root_by(
    root: &Path,
    give_up: Instant,
) -> Result<std::sync::MutexGuard<'static, ()>, String> {
    let lock = root_lock(root);
    loop {
        match lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() < give_up => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(std::sync::TryLockError::WouldBlock) => return Err(in_use(root)),
        }
    }
}

fn in_use(root: &Path) -> String {
    format!(
        "{} is in use by another change. Try again.",
        root.join(FILE).display()
    )
}

/// A lock on a file beside the settings, for the other Arko processes: a
/// change waits while one holds it. The operating system holds the lock for
/// the open file, and lets it go when this ends or the process does. The
/// file stays.
struct FileLock(std::fs::File);

impl FileLock {
    fn acquire(root: &Path, give_up: Instant) -> Result<Self, String> {
        let path = root.join(format!("{FILE}.lock"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("Couldn't lock {} ({error}).", path.display()))?;
        loop {
            let attempt = Self::try_lock(&file);
            match attempt {
                Ok(()) => return Ok(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < give_up => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::WouldBlock) => return Err(in_use(root)),
                // Without locks, another Arko process could change the file
                // under this one, and the privacy settings may be in it.
                Err(std::fs::TryLockError::Error(error))
                    if error.kind() == std::io::ErrorKind::Unsupported =>
                {
                    return Err(format!(
                        "The file system of {} can't lock files, so the OpenRouter settings can't be changed there.",
                        root.display()
                    ));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!("Couldn't lock {} ({error}).", path.display()));
                }
            }
        }
    }
}

impl FileLock {
    /// [`File::try_lock`], or (tests) the error the test chose.
    fn try_lock(file: &std::fs::File) -> Result<(), std::fs::TryLockError> {
        #[cfg(test)]
        if let Some(kind) = LOCK_FAILS.with(std::cell::Cell::get) {
            return Err(std::fs::TryLockError::Error(std::io::Error::from(kind)));
        }
        file.try_lock()
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// The sessions open in this process, by Arko's folder.
type Sessions = Vec<(PathBuf, Weak<crate::keys::Keys>)>;
static SESSIONS: LazyLock<Mutex<Sessions>> = LazyLock::new(Mutex::default);

/// Has the sessions of `keys`' kind hear of changes to the settings in
/// `root` (see `Keys::share`).
pub(crate) fn register(root: &Path, keys: &Arc<crate::keys::Keys>) {
    let mut sessions = crate::lock(&SESSIONS);
    sessions.retain(|(_, keys)| keys.strong_count() > 0);
    sessions.push((folder(root), Arc::downgrade(keys)));
}

/// Installs `settings` in each session open on `root`, which says so unless
/// it is the one numbered `origin`. It runs under the change's lock, so that
/// the installs come in the order of the changes.
fn notify(root: &Path, settings: &Settings, origin: u64) {
    let sessions: Vec<Arc<crate::keys::Keys>> = crate::lock(&SESSIONS)
        .iter()
        .filter(|(at, _)| *at == folder(root))
        .filter_map(|(_, keys)| keys.upgrade())
        .collect();
    for keys in sessions {
        keys.use_openrouter_settings(settings, keys.id() != origin);
    }
}

/// How many times each folder's settings file was read (tests).
#[cfg(test)]
static READS: LazyLock<Mutex<HashMap<PathBuf, usize>>> = LazyLock::new(Mutex::default);

/// How many times the settings file of `root` was read (tests).
#[cfg(test)]
fn reads_of(root: &Path) -> usize {
    crate::lock(&READS).get(&folder(root)).copied().unwrap_or(0)
}

// For tests, on the thread: how long a change waits for the lock instead of
// the usual, and what the lock on a file fails with.
#[cfg(test)]
thread_local! {
    static LOCK_WAIT: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
    /// The error the lock on a file fails with on the thread, instead.
    static LOCK_FAILS: std::cell::Cell<Option<std::io::ErrorKind>> =
        const { std::cell::Cell::new(None) };
}

/// Stops the change in Arko's folder `root` that reads next, once it has
/// read, until the test lets it go (tests).
#[cfg(test)]
type Pause = (
    PathBuf,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[cfg(test)]
static PAUSE: Mutex<Option<Pause>> = Mutex::new(None);

/// Holds the next change in Arko's folder `root` after it has read the file.
/// The first receiver says it did, and the sender lets it go (tests).
#[cfg(test)]
pub(crate) fn pause_next_change(
    root: &Path,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (reached, wait_for_it) = std::sync::mpsc::channel();
    let (go, hold) = std::sync::mpsc::channel();
    *crate::lock(&PAUSE) = Some((root.to_path_buf(), reached, hold));
    (wait_for_it, go)
}

#[cfg(test)]
fn pause(root: &Path) {
    let pause = {
        let mut held = crate::lock(&PAUSE);
        match held.as_ref() {
            Some((folder, ..)) if folder == root => held.take(),
            _ => None,
        }
    };
    if let Some((_, reached, go)) = pause {
        let _ = reached.send(());
        let _ = go.recv();
    }
}

/// Keeps `model` (as `/model` takes it) among the models picked, when it is
/// an OpenRouter model, with what the models in memory say of it. It saves
/// without telling the open sessions, since nothing is routed differently.
pub fn remember(root: &Path, model: &str) {
    remember_in(root, model, None);
}

fn remember_in(root: &Path, model: &str, local: Option<&str>) {
    let Some(slug) = crate::providers::strip_prefix(model, crate::providers::OPENROUTER) else {
        return;
    };
    // (As the file has the settings now, so that the models picked don't
    // write old choices back.)
    let kept = Settings::edit(root, 0, |settings| {
        let listed = listed_in_memory(&settings.filters(local), slug);
        settings.remember(slug, listed.as_ref());
    });
    if let Err(error) = kept {
        log::warn!("openrouter: couldn't keep the model picked ({error})");
    }
}

/// The models picked in Arko's folder `root`, newest first, with their
/// details.
pub fn recent(root: &Path) -> Vec<Listed> {
    Settings::load(root).recent()
}

/// The models picked are brought up to date with a list just fetched.
fn update_recent(root: &Path, listed: &[Listed]) {
    if let Err(error) = Settings::edit(root, 0, |settings| {
        settings.refresh_recent(listed);
    }) {
        log::warn!("openrouter: couldn't update the models picked ({error})");
    }
}

// ----- the words /openrouter takes -----

/// How `/openrouter` is used, one form to a line.
pub const FORMS: [&str; 8] = [
    "/openrouter zdr on|off",
    "/openrouter data-collection allow|deny",
    "/openrouter region any|us|eu|us-or-eu",
    "/openrouter sort default|price|throughput|latency",
    "/openrouter precision any|fp8|bf16",
    "/openrouter only SLUGS|none",
    "/openrouter ignore SLUGS|none",
    "/openrouter refresh",
];

/// What `/openrouter WORDS` changes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Change {
    Zdr(bool),
    DataCollection(bool),
    Region(Region),
    Sort(Sort),
    Precision(Precision),
    Only(Vec<String>),
    Ignore(Vec<String>),
    /// Fetch the list of models again.
    Refresh,
}

/// The change `/openrouter`'s `argument` asks for, or what is wrong with it.
pub(crate) fn parse(argument: &str) -> Result<Change, String> {
    let words: Vec<&str> = argument.split_whitespace().collect();
    let usage = || format!("/openrouter takes these forms.\n{}", FORMS.join("\n"));
    let word = |wanted: &[(&str, Change)], text: &str| {
        wanted
            .iter()
            .find(|(word, _)| word.eq_ignore_ascii_case(text))
            .map(|(_, change)| change.clone())
    };
    let choices = |name: &str, list: &[&str]| {
        let (last, rest) = list.split_last().expect("choices");
        let words = match rest {
            [] => last.to_string(),
            rest => format!("{} or {last}", rest.join(", ")),
        };
        format!("/openrouter {name} takes {words}.")
    };
    match words.as_slice() {
        ["refresh"] => Ok(Change::Refresh),
        ["zdr", value] => word(
            &[("on", Change::Zdr(true)), ("off", Change::Zdr(false))],
            value,
        )
        .ok_or_else(|| choices("zdr", &["on", "off"])),
        ["data-collection", value] => word(
            &[
                ("deny", Change::DataCollection(true)),
                ("allow", Change::DataCollection(false)),
            ],
            value,
        )
        .ok_or_else(|| choices("data-collection", &["allow", "deny"])),
        ["region", value] => Region::ALL
            .into_iter()
            .find(|region| region.word().eq_ignore_ascii_case(value))
            .map(Change::Region)
            .ok_or_else(|| choices("region", &["any", "us", "eu", "us-or-eu"])),
        ["sort", value] => Sort::ALL
            .into_iter()
            .find(|sort| sort.word().eq_ignore_ascii_case(value))
            .map(Change::Sort)
            .ok_or_else(|| choices("sort", &["default", "price", "throughput", "latency"])),
        ["precision", value] => Precision::ALL
            .into_iter()
            .find(|precision| precision.word().eq_ignore_ascii_case(value))
            .map(Change::Precision)
            .ok_or_else(|| choices("precision", &["any", "fp8", "bf16"])),
        ["only", rest @ ..] if !rest.is_empty() => Ok(Change::Only(slugs(&rest.join(" ")))),
        ["ignore", rest @ ..] if !rest.is_empty() => Ok(Change::Ignore(slugs(&rest.join(" ")))),
        _ => Err(usage()),
    }
}

impl Change {
    /// Makes the change in `settings`, and says what is so now. `None` for
    /// a change that isn't to the settings.
    pub(crate) fn apply(&self, settings: &mut Settings) -> Option<String> {
        let list = |slugs: &[String]| crate::and_list(slugs);
        Some(match self {
            Change::Refresh => return None,
            Change::Zdr(on) => {
                settings.set_zdr(*on);
                match on {
                    true => "Zero data retention is on. Only providers that keep no data serve requests, and the list of models shows the models offered that way.".to_string(),
                    false => "Zero data retention is off.".to_string(),
                }
            }
            Change::DataCollection(deny) => {
                settings.set_deny_data_collection(*deny);
                match (deny, settings.zdr()) {
                    (true, true) => "No data collection is on, and zero data retention covers it already.".to_string(),
                    (true, false) => "No data collection is on. Only providers that don't collect your prompts serve requests.".to_string(),
                    (false, true) => "No data collection is off, and zero data retention still covers it.".to_string(),
                    (false, false) => "No data collection is off.".to_string(),
                }
            }
            Change::Region(region) => {
                settings.set_region(*region);
                match region {
                    Region::Any => "Region is any.".to_string(),
                    region => format!(
                        "Region is {}. It needs OpenRouter's Business or Enterprise plan, and fewer models are offered there.",
                        region.label()
                    ),
                }
            }
            Change::Sort(sort) => {
                settings.set_sort(*sort);
                match sort {
                    Sort::Default => "OpenRouter chooses how to order providers.".to_string(),
                    sort => format!(
                        "OpenRouter orders providers by {}.",
                        sort.label().to_lowercase()
                    ),
                }
            }
            Change::Precision(precision) => {
                settings.set_precision(*precision);
                match precision {
                    Precision::Any => "Any precision is allowed.".to_string(),
                    precision => format!(
                        "Providers must serve weights of {} precision.",
                        precision.label().to_lowercase()
                    ),
                }
            }
            Change::Only(slugs) => {
                settings.set_only(slugs.clone());
                match slugs.as_slice() {
                    [] => "Any provider may serve requests.".to_string(),
                    slugs => format!("Only {} serve requests.", list(slugs)),
                }
            }
            Change::Ignore(slugs) => {
                settings.set_ignore(slugs.clone());
                match slugs.as_slice() {
                    [] => "No provider is left out.".to_string(),
                    slugs => format!("{} never serve requests.", list(slugs)),
                }
            }
        })
    }
}

/// What `/openrouter ARGUMENT` does and says, as session number `origin` (see
/// `Keys::id`) of Arko's folder `root`, whose `feed` hears of a fetch. It
/// may wait for another change to the settings, so it runs where waiting is
/// all right (see `Harness::openrouter_live`).
pub(crate) fn run_command(
    root: &Path,
    origin: u64,
    feed: &crate::remote::Feed,
    argument: &str,
) -> Result<String, String> {
    if argument.is_empty() {
        return show(root);
    }
    let change = parse(argument)?;
    if change == Change::Refresh {
        // The models come in the background.
        let (root, feed) = (root.to_path_buf(), feed.clone());
        std::thread::spawn(move || {
            let catalog = refresh_blocking(&root, true);
            let (level, text) = match catalog.problem {
                Some(problem) => (crate::remote::Level::Warn, format!("{problem}.")),
                None => (
                    crate::remote::Level::Info,
                    format!(
                        "OpenRouter lists {} models that take tool calls.",
                        catalog.models.len()
                    ),
                ),
            };
            feed.notice(level, &text);
        });
        return Ok("Fetching OpenRouter's models.".to_string());
    }
    // Saved, and installed in every session open here, this one too, before
    // it says so.
    let (_, said) = Settings::edit(root, origin, |settings| change.apply(settings))?;
    let said = said.unwrap_or_default();
    // (The list of models follows the filters.)
    refresh_soon(root);
    Ok(format!(
        "{said} Open sessions use it from their next request."
    ))
}

/// The settings and the forms that change them, as lines.
fn show(root: &Path) -> Result<String, String> {
    let settings = Settings::read(root)?;
    let mut lines = vec!["OpenRouter".to_string()];
    lines.extend(settings.lines(Some(&catalog(root).summary())));
    lines.push(String::new());
    lines.extend(FORMS.iter().map(|form| form.to_string()));
    lines.push(String::new());
    lines.push(
        "Your OpenRouter account's own privacy settings at openrouter.ai apply too. Open sessions use a change from their next request.".to_string(),
    );
    Ok(lines.join("\n"))
}

impl crate::Harness {
    /// `/openrouter`, typed at the line prompt or sent from the page or an
    /// app: says the settings and the forms that change them, or changes one
    /// (the full-screen view has its own menu, opened by `/openrouter` alone).
    /// The open sessions of this process use a change from their next
    /// request. This one waits for the change on this thread, where a session
    /// that runs tasks uses [`openrouter_live`](Self::openrouter_live).
    pub(crate) fn openrouter_command(&mut self, argument: &str, source: crate::remote::Source) {
        let result = run_command(&self.root, self.keys.id(), &self.feed, argument);
        self.setting_reply(result, source, false);
    }

    /// [`openrouter_command`](Self::openrouter_command) for a line the session
    /// reads: the change waits for its lock on a thread made for waiting, so
    /// the session's tasks go on, and it is said once it is made and installed.
    pub(crate) async fn openrouter_live(
        &mut self,
        command: &str,
        argument: &str,
        source: crate::remote::Source,
    ) {
        self.feed
            .user(command, source, crate::remote::Delivery::Command);
        let (root, origin, feed, argument) = (
            self.root.clone(),
            self.keys.id(),
            self.feed.clone(),
            argument.to_string(),
        );
        let result =
            tokio::task::spawn_blocking(move || run_command(&root, origin, &feed, &argument))
                .await
                .unwrap_or_else(|_| Err("The change stopped.".to_string()));
        self.setting_reply(result, source, false);
        // (A setting it changed shows in the status and the details.)
        self.gauges.touch();
        self.publish_details();
    }
}

// ----- the models -----

/// A model OpenRouter lists, for a picker.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    /// As `/model` takes it: `openrouter:moonshotai/kimi-k3`.
    pub name: String,
    /// What people call it (`Kimi K3`), without its maker's name before it.
    pub label: String,
    /// The most input it takes, in tokens.
    pub context: Option<u64>,
    /// What a million input and output tokens cost, in US dollars, when
    /// OpenRouter says.
    pub input_price: Option<f64>,
    pub output_price: Option<f64>,
    /// The regional address its requests go to (with a US, EU or US or EU
    /// region), none with any region.
    pub zone: Option<Zone>,
}

impl Listed {
    /// The slug (`moonshotai/kimi-k3`).
    pub fn slug(&self) -> &str {
        self.name
            .strip_prefix(crate::providers::OPENROUTER)
            .unwrap_or(&self.name)
    }

    /// What to know before choosing it, as a line (`262k context, $0.60 in
    /// and $2.50 out per million tokens`), empty when OpenRouter says
    /// nothing.
    pub fn details(&self) -> String {
        let mut parts = Vec::new();
        if let Some(context) = self.context {
            parts.push(format!("{} context", tokens(context)));
        }
        match (self.input_price, self.output_price) {
            (Some(input), Some(output)) if input == 0.0 && output == 0.0 => {
                parts.push("free".into());
            }
            (Some(input), Some(output)) => parts.push(format!(
                "{} in and {} out per million tokens",
                price(input),
                price(output)
            )),
            _ => {}
        }
        parts.join(", ")
    }

    /// The context and prices in few characters (`262k · $0.60/$2.50`), for
    /// a column. Empty when OpenRouter says nothing.
    pub fn short(&self) -> String {
        let mut parts = Vec::new();
        if let Some(context) = self.context {
            parts.push(tokens(context));
        }
        match (self.input_price, self.output_price) {
            (Some(input), Some(output)) if input == 0.0 && output == 0.0 => {
                parts.push("free".into());
            }
            (Some(input), Some(output)) => {
                parts.push(format!("{}/{}", price(input), price(output)));
            }
            _ => {}
        }
        parts.join(" · ")
    }

    /// Whether every word of `query` is in its name or slug, in any case.
    pub fn matches(&self, query: &str) -> bool {
        let label = self.label.to_lowercase();
        let slug = self.slug().to_lowercase();
        query
            .split_whitespace()
            .map(str::to_lowercase)
            .all(|word| label.contains(&word) || slug.contains(&word))
    }
}

/// `262k`, `1M`: a number of tokens in few characters.
fn tokens(count: u64) -> String {
    if count >= 1_000_000 {
        let millions = format!("{:.1}", count as f64 / 1_000_000.0);
        format!("{}M", millions.trim_end_matches('0').trim_end_matches('.'))
    } else {
        format!("{}k", (count + 500) / 1000)
    }
}

/// `$0.60`, `$15.00`, `$0.075`: a price in dollars with at least cents.
fn price(dollars: f64) -> String {
    let mut text = format!("{dollars:.4}");
    while text.ends_with('0')
        && text
            .split_once('.')
            .is_some_and(|(_, cents)| cents.len() > 2)
    {
        text.pop();
    }
    format!("${text}")
}

/// OpenRouter's models that take tool calls (which Arko's agents work
/// through) and that the settings allow, as they are in memory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    /// The models, in OpenRouter's order (the newest first).
    pub models: Vec<Listed>,
    /// When they were fetched, once they have been.
    pub fetched: Option<SystemTime>,
    /// Why the list can't be had or is out of date, when it is so.
    pub problem: Option<String>,
}

impl Catalog {
    /// The model `name` (`openrouter:slug`, or just the slug).
    pub fn get(&self, name: &str) -> Option<&Listed> {
        let slug = name
            .strip_prefix(crate::providers::OPENROUTER)
            .unwrap_or(name);
        self.models.iter().find(|model| model.slug() == slug)
    }

    /// The models whose name or slug holds every word of `query`, those that
    /// start with it first. Every model for a query without words.
    pub fn search(&self, query: &str) -> Vec<&Listed> {
        let query = query.trim().to_lowercase();
        let mut found: Vec<&Listed> = self
            .models
            .iter()
            .filter(|model| model.matches(&query))
            .collect();
        if !query.is_empty() {
            // (A stable sort keeps OpenRouter's order within each part.)
            found.sort_by_key(|model| {
                !(model.slug().to_lowercase().starts_with(&query)
                    || model.label.to_lowercase().starts_with(&query))
            });
        }
        found
    }

    /// How long ago it was fetched (`updated 2 hours ago`), once it was.
    pub fn updated(&self) -> Option<String> {
        let seconds = SystemTime::now()
            .duration_since(self.fetched?)
            .ok()?
            .as_secs();
        let ago = |count: u64, unit: &str| {
            format!(
                "updated {count} {unit}{} ago",
                if count == 1 { "" } else { "s" }
            )
        };
        Some(match seconds {
            0..=59 => "updated just now".to_string(),
            60..=3599 => ago(seconds / 60, "minute"),
            3600..=86399 => ago(seconds / 3600, "hour"),
            _ => ago(seconds / 86400, "day"),
        })
    }

    /// What the models row says (`301, updated 2 hours ago`).
    pub fn summary(&self) -> String {
        match (self.models.len(), self.updated()) {
            (0, _) => "not loaded".to_string(),
            (count, Some(updated)) => format!("{count}, {updated}"),
            (count, None) => count.to_string(),
        }
    }
}

/// What decides the models listed: the filters, and (tests) the server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    local: Option<String>,
    region: Region,
    zdr: bool,
}

impl Key {
    /// The address of each list to fetch, with the zone its models are in.
    fn lists(&self) -> Vec<(Option<Zone>, String)> {
        let at = |zone: Option<Zone>| match (&self.local, zone) {
            (Some(url), None) => url.clone(),
            (Some(url), Some(Zone::Us)) => format!("{url}/us"),
            (Some(url), Some(Zone::Eu)) => format!("{url}/eu"),
            (None, None) => HOST.to_string(),
            (None, Some(zone)) => zone.base_url().to_string(),
        };
        match self.region {
            Region::Any => vec![(None, at(None))],
            Region::Us => vec![(Some(Zone::Us), at(Some(Zone::Us)))],
            Region::Eu => vec![(Some(Zone::Eu), at(Some(Zone::Eu)))],
            Region::UsOrEu => vec![
                (Some(Zone::Us), at(Some(Zone::Us))),
                (Some(Zone::Eu), at(Some(Zone::Eu))),
            ],
        }
    }
}

/// A list fetched.
#[derive(Clone)]
struct Loaded {
    at: Instant,
    fetched: SystemTime,
    listed: Arc<Vec<Listed>>,
    catalog: Arc<OpenRouterCatalog>,
    /// The zone of each model by its ID, for the models with one.
    zones: HashMap<String, Zone>,
}

#[derive(Default)]
struct Entry {
    loaded: Option<Loaded>,
    /// When the last fetch failed, and why.
    failed: Option<(Instant, String)>,
}

static MEMORY: LazyLock<Mutex<HashMap<Key, Entry>>> = LazyLock::new(Mutex::default);
/// How many fetches are going on or waiting.
static FETCHING: AtomicUsize = AtomicUsize::new(0);
/// How many are going on or waiting for each list.
static IN_FLIGHT: LazyLock<Mutex<HashMap<Key, usize>>> = LazyLock::new(Mutex::default);
/// Counts what changed in memory, for a view to draw again.
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn memory() -> std::sync::MutexGuard<'static, HashMap<Key, Entry>> {
    crate::lock(&MEMORY)
}

/// Counts the changes to the models in memory, so that a view knows when
/// to draw them again.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Whether a fetch of the models is going on in this process, for whichever
/// settings ([`loading_for`] says for the settings of a folder).
pub fn loading() -> bool {
    FETCHING.load(Ordering::Relaxed) > 0
}

/// Whether the models the settings in Arko's folder `root` allow are being
/// fetched. It needs no file access, so a view can ask as it draws.
pub fn loading_for(root: &Path) -> bool {
    settings_in_memory(root, false).is_some_and(|settings| fetching(&settings.filters(None)))
}

/// The settings as this process has them, for what draws: no file is read
/// here. Before they have been read it asks for them in the background (and
/// for the models once they are, with `models`) and says nothing yet, and a
/// view finds out when [`generation`] moves.
fn settings_in_memory(root: &Path, models: bool) -> Option<Settings> {
    if let Some(settings) = Settings::cached(root) {
        return Some(settings);
    }
    static READING: LazyLock<Mutex<std::collections::HashSet<PathBuf>>> =
        LazyLock::new(Mutex::default);
    if crate::lock(&READING).insert(folder(root)) {
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            let settings = Settings::load(&root);
            crate::lock(&READING).remove(&folder(&root));
            GENERATION.fetch_add(1, Ordering::Relaxed);
            if models {
                refresh_key_soon(settings.filters(None), &root);
            }
        });
    }
    None
}

fn fetching(key: &Key) -> bool {
    crate::lock(&IN_FLIGHT)
        .get(key)
        .is_some_and(|count| *count > 0)
}

fn snapshot(key: &Key) -> Catalog {
    let memory = memory();
    let Some(entry) = memory.get(key) else {
        return Catalog::default();
    };
    Catalog {
        models: entry
            .loaded
            .as_ref()
            .map(|loaded| loaded.listed.to_vec())
            .unwrap_or_default(),
        fetched: entry.loaded.as_ref().map(|loaded| loaded.fetched),
        problem: entry.failed.as_ref().map(|(_, why)| problem(why, entry)),
    }
}

/// What a failed fetch says, as a sentence.
fn problem(why: &str, entry: &Entry) -> String {
    match entry.loaded {
        Some(_) => format!("Couldn't update the models ({why})"),
        None => format!("Couldn't load the models ({why})"),
    }
}

fn is_fresh(key: &Key) -> bool {
    memory()
        .get(key)
        .and_then(|entry| entry.loaded.as_ref())
        .is_some_and(|loaded| loaded.at.elapsed() < FRESH)
}

fn failed_lately(key: &Key) -> bool {
    memory()
        .get(key)
        .and_then(|entry| entry.failed.as_ref())
        .is_some_and(|(at, _)| at.elapsed() < RETRY)
}

/// The models OpenRouter lists that take tool calls and that the settings in
/// Arko's folder `root` allow, as they are in memory. It never waits for the
/// network. The list is empty until [`refresh`] has fetched it.
pub fn catalog(root: &Path) -> Catalog {
    snapshot(&Settings::load(root).filters(None))
}

/// [`catalog`], fetching the models first when they are older than ten
/// minutes (or `force`). The list the settings rule out isn't kept, so a
/// change of region or zero data retention fetches again. A failure leaves
/// the list as it was, with [`Catalog::problem`] saying why.
pub async fn refresh(root: &Path, force: bool) -> Catalog {
    refresh_at(&Settings::load(root).filters(None), force, Some(root)).await
}

/// [`refresh`], for an app's background thread: it waits for the fetch
/// (ten seconds at most) on a runtime of its own.
pub fn refresh_blocking(root: &Path, force: bool) -> Catalog {
    let key = Settings::load(root).filters(None);
    blocking(key, force, root.to_path_buf())
}

fn blocking(key: Key, force: bool, root: std::path::PathBuf) -> Catalog {
    // (On a thread of its own, so that this works inside a runtime too.)
    let fetched = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|runtime| runtime.block_on(refresh_at(&key, force, Some(&root))))
    })
    .join();
    match fetched {
        Ok(Ok(catalog)) => catalog,
        _ => Catalog {
            problem: Some("Couldn't load the models (the fetch stopped)".into()),
            ..Catalog::default()
        },
    }
}

/// Puts the models `slugs` in memory for `settings`, as if fetched (tests,
/// which don't reach the network). Tests that read the models by
/// [`catalog`] or [`completions`] use settings no other test does.
#[cfg(test)]
pub(crate) fn seed_for_tests(settings: &Settings, slugs: &[&str]) {
    let entries: Vec<Value> = slugs.iter().map(|slug| json!({"id": slug})).collect();
    seed_entries_for_tests(settings, entries);
}

/// [`seed_for_tests`] with the entries as OpenRouter lists them.
#[cfg(test)]
pub(crate) fn seed_entries_for_tests(settings: &Settings, entries: Vec<Value>) {
    let catalog = OpenRouterCatalog::from_json(&json!({ "data": entries }).to_string()).unwrap();
    let listed = catalog
        .models()
        .iter()
        .map(|model| listed(model, None))
        .collect();
    memory().entry(settings.filters(None)).or_default().loaded = Some(Loaded {
        at: Instant::now(),
        fetched: SystemTime::now(),
        listed: Arc::new(listed),
        catalog: Arc::new(catalog),
        zones: HashMap::new(),
    });
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Makes the models for `settings` a failed fetch, as if it had just
/// happened (tests).
#[cfg(test)]
pub(crate) fn seed_failure_for_tests(settings: &Settings, why: &str) {
    memory().entry(settings.filters(None)).or_default().failed =
        Some((Instant::now(), why.to_string()));
}

/// The most completions of a prefix [`completions`] gives.
const MOST_COMPLETIONS: usize = 40;

/// The OpenRouter models (as `/model` takes them, `openrouter:slug`) whose
/// name starts with `typed`, from the models in memory for the settings in
/// Arko's folder `root`, at most forty. It never waits and needs no file
/// access, so the input box can ask as it draws. It starts a fetch in the
/// background when the models are due, so the first call may give none, and
/// later calls give them.
pub fn completions(root: &Path, typed: &str) -> Vec<String> {
    let prefix = typed.to_lowercase();
    if !prefix.starts_with(crate::providers::OPENROUTER) {
        return Vec::new();
    }
    // (Before the settings are known, nothing yet.)
    let Some(settings) = settings_in_memory(root, true) else {
        return Vec::new();
    };
    let key = settings.filters(None);
    refresh_key_soon(key.clone(), root);
    snapshot(&key)
        .models
        .into_iter()
        .filter(|model| model.name.to_lowercase().starts_with(&prefix))
        .map(|model| model.name)
        .take(MOST_COMPLETIONS)
        .collect()
}

/// Starts fetching the models in the background when they are due, and
/// returns at once. A view draws what is in memory ([`catalog`]) and
/// again when [`generation`] moves.
pub fn refresh_soon(root: &Path) {
    if let Some(settings) = settings_in_memory(root, true) {
        refresh_key_soon(settings.filters(None), root);
    }
}

/// [`refresh_soon`] for the models of `key`. A fetch of other models going
/// on doesn't hold it back.
fn refresh_key_soon(key: Key, root: &Path) {
    if is_fresh(&key) || failed_lately(&key) || fetching(&key) {
        return;
    }
    // (A runtime of its own: the caller may have none.)
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        blocking(key, false, root);
    });
}

async fn refresh_at(key: &Key, force: bool, root: Option<&Path>) -> Catalog {
    if !force && (is_fresh(key) || failed_lately(key)) {
        return snapshot(key);
    }
    /// A fetch for `key`, going on or waiting.
    struct Counted(Key);
    impl Counted {
        fn start(key: &Key) -> Self {
            FETCHING.fetch_add(1, Ordering::Relaxed);
            *crate::lock(&IN_FLIGHT).entry(key.clone()).or_default() += 1;
            Counted(key.clone())
        }
    }
    impl Drop for Counted {
        fn drop(&mut self) {
            FETCHING.fetch_sub(1, Ordering::Relaxed);
            let mut in_flight = crate::lock(&IN_FLIGHT);
            if let Some(count) = in_flight.get_mut(&self.0) {
                *count -= 1;
                if *count == 0 {
                    in_flight.remove(&self.0);
                }
            }
            GENERATION.fetch_add(1, Ordering::Relaxed);
        }
    }
    // One fetch at a time for each list, and another list's doesn't wait.
    static TURNS: LazyLock<Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(Mutex::default);
    let turn = crate::lock(&TURNS).entry(key.clone()).or_default().clone();
    let _counted = Counted::start(key);
    let _turn = turn.lock().await;
    // Another call may have fetched while this one waited.
    if !force && (is_fresh(key) || failed_lately(key)) {
        return snapshot(key);
    }
    match fetch(key).await {
        Ok((listed, catalog, zones)) => {
            if let Some(root) = root {
                // The models picked take what OpenRouter says now (a change
                // to the file, away from this task).
                let (root, listed) = (root.to_path_buf(), listed.clone());
                let _ = tokio::task::spawn_blocking(move || update_recent(&root, &listed)).await;
            }
            let mut memory = memory();
            let entry = memory.entry(key.clone()).or_default();
            entry.loaded = Some(Loaded {
                at: Instant::now(),
                fetched: SystemTime::now(),
                listed: Arc::new(listed),
                catalog: Arc::new(catalog),
                zones,
            });
            entry.failed = None;
        }
        Err(why) => {
            log::warn!("openrouter: the models weren't fetched ({why})");
            memory().entry(key.clone()).or_default().failed = Some((Instant::now(), why));
        }
    }
    snapshot(key)
}

/// The lists for `key` fetched, and read.
async fn fetch(
    key: &Key,
) -> Result<(Vec<Listed>, OpenRouterCatalog, HashMap<String, Zone>), String> {
    let lists = key.lists();
    let fetches = lists.iter().map(|(_, url)| fetch_list(url, key.zdr));
    let results = futures_util::future::join_all(fetches).await;
    let mut entries: Vec<Value> = Vec::new();
    let mut zones: HashMap<String, Zone> = HashMap::new();
    for ((zone, _), result) in lists.iter().zip(results) {
        // (All of them, or none: a half is a different list.)
        for entry in result? {
            let Some(id) = entry.get("id").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            // The first zone to list a model keeps it (the US before the EU).
            if let Some(zone) = zone
                && !zones.contains_key(&id)
            {
                zones.insert(id, *zone);
                entries.push(entry);
            } else if zone.is_none() {
                entries.push(entry);
            }
        }
    }
    let body = serde_json::json!({ "data": entries }).to_string();
    let catalog = OpenRouterCatalog::from_json(&body)
        .map_err(|error| format!("its list couldn't be read, {error}"))?;
    let listed = catalog
        .models()
        .iter()
        .map(|model| listed(model, zones.get(&model.id).copied()))
        .collect();
    Ok((listed, catalog, zones))
}

/// One list from `base`, without the entries that aren't models of their own
/// (`:batch` runs the same model at a delay).
async fn fetch_list(base: &str, zdr: bool) -> Result<Vec<Value>, String> {
    // (Tests reach a local server only.)
    if cfg!(test) && !base.starts_with("http://127.0.0.1") {
        return Err("tests don't reach the network".to_string());
    }
    let url = format!(
        "{}/models?supported_parameters=tools{}",
        base.trim_end_matches('/'),
        if zdr { "&zdr=true" } else { "" }
    );
    let client = arko_ai::http_client_builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    // (Without reqwest's decompression, so that the body comes as sent.)
    let response = client
        .get(&url)
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .map_err(reached)?;
    if !response.status().is_success() {
        return Err(format!(
            "OpenRouter answered {}",
            response.status().as_u16()
        ));
    }
    let gzip = response
        .headers()
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("gzip"));
    let body = response.bytes().await.map_err(reached)?;
    let text = if gzip {
        use std::io::Read;
        let mut text = Vec::new();
        flate2::read::GzDecoder::new(&body[..])
            .take(MOST)
            .read_to_end(&mut text)
            .map_err(|error| format!("its list wasn't readable, {error}"))?;
        text
    } else {
        body.to_vec()
    };
    let mut value: Value = serde_json::from_slice(&text)
        .map_err(|error| format!("its list wasn't readable, {error}"))?;
    let Some(Value::Array(entries)) = value.get_mut("data").map(Value::take) else {
        return Err("its list held no models".to_string());
    };
    Ok(entries
        .into_iter()
        .filter(|entry| {
            entry
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.ends_with(":batch"))
        })
        .map(slim)
        .collect())
}

/// Why OpenRouter wasn't reached, in a few words.
fn reached(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "it didn't answer in 10 s".to_string()
    } else if error.is_connect() {
        "it couldn't be reached".to_string()
    } else {
        error.without_url().to_string()
    }
}

/// An entry without what Arko doesn't read (the descriptions and
/// benchmarks make most of a list).
fn slim(entry: Value) -> Value {
    const KEEP: [&str; 10] = [
        "id",
        "canonical_slug",
        "name",
        "context_length",
        "architecture",
        "pricing",
        "top_provider",
        "supported_parameters",
        "reasoning",
        "expiration_date",
    ];
    match entry {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .filter(|(name, _)| KEEP.contains(&name.as_str()) || name == "alias_target")
                .collect(),
        ),
        other => other,
    }
}

fn listed(model: &Model, zone: Option<Zone>) -> Listed {
    let info = model.openrouter.as_ref();
    let label = info
        .and_then(|info| info.name.as_deref())
        .map(|name| name.split_once(": ").map_or(name, |(_, label)| label))
        .map(str::to_string)
        .unwrap_or_else(|| crate::models::display_name(&format!("openrouter:{}", model.id)));
    let known = info.is_some_and(|info| info.pricing_known);
    Listed {
        name: format!("{}{}", crate::providers::OPENROUTER, model.id),
        label,
        context: (model.max_input_tokens > 0).then_some(model.max_input_tokens as u64),
        input_price: known.then_some(model.pricing.input_cost_per_million),
        output_price: known.then_some(model.pricing.output_cost_per_million),
        zone,
    }
}

/// What the models in memory say of `slug`, however old they are.
fn listed_in_memory(key: &Key, slug: &str) -> Option<Listed> {
    let memory = memory();
    let loaded = memory.get(key)?.loaded.as_ref()?;
    loaded.listed.iter().find(|one| one.slug() == slug).cloned()
}

/// The model `slug` as the models in memory for the settings in Arko's
/// folder `root` hold it (however old they are), else with what was kept
/// when it was picked, for its context window. `None` when neither knows it.
pub(crate) fn known(root: &Path, slug: &str) -> Option<Model> {
    known_in(root, slug, None)
}

fn known_in(root: &Path, slug: &str, local: Option<&str>) -> Option<Model> {
    let settings = Settings::load(root);
    let live = memory()
        .get(&settings.filters(local))
        .and_then(|entry| entry.loaded.as_ref())
        .and_then(|loaded| loaded.catalog.get(slug));
    live.or_else(|| {
        let recent = settings.recent.iter().find(|one| one.slug == slug)?;
        let mut model = Model::openrouter(slug);
        model.max_input_tokens = recent.context? as usize;
        Some(model)
    })
}

/// The catalog for a session start's check: refreshed when due. `local` is
/// a server standing in for OpenRouter (tests).
pub(crate) async fn checked(
    root: &Path,
    settings: &Settings,
    local: Option<&str>,
) -> Result<Arc<OpenRouterCatalog>, String> {
    let key = settings.filters(local);
    refresh_at(&key, false, Some(root)).await;
    let memory = memory();
    let entry = memory.get(&key);
    match entry.and_then(|entry| entry.loaded.as_ref()) {
        Some(loaded) => Ok(loaded.catalog.clone()),
        None => Err(entry
            .and_then(|entry| entry.failed.as_ref())
            .map_or_else(|| "it gave no models".to_string(), |(_, why)| why.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A model as OpenRouter lists it, with fields Arko doesn't read.
    fn entry(id: &str, name: &str, context: u64, prompt: &str, completion: &str) -> Value {
        json!({
            "id": id,
            "canonical_slug": format!("{id}-20261001"),
            "name": name,
            "description": "A long description.",
            "context_length": context,
            "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]},
            "pricing": {"prompt": prompt, "completion": completion},
            "top_provider": {"context_length": context, "max_completion_tokens": 8000},
            "supported_parameters": ["tools", "reasoning"],
            "benchmarks": {"design_arena": [1, 2, 3]},
        })
    }

    fn list(entries: Vec<Value>) -> Vec<u8> {
        json!({ "data": entries }).to_string().into_bytes()
    }

    fn gzipped(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(bytes).unwrap();
        gzip.finish().unwrap()
    }

    /// A local server standing in for OpenRouter. A route is a path prefix,
    /// the status and the body (gzipped when it says so).
    struct Server {
        url: String,
        heads: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Server {
        async fn start(routes: Vec<(&'static str, u16, Vec<u8>, bool)>) -> Self {
            Self::start_slow(routes, None).await
        }

        /// A server that answers the path starting with `slow.0` after
        /// `slow.1`.
        async fn start_slow(
            routes: Vec<(&'static str, u16, Vec<u8>, bool)>,
            slow: Option<(&'static str, Duration)>,
        ) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let heads = Arc::new(Mutex::new(Vec::new()));
            let seen = heads.clone();
            let task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let (routes, seen) = (routes.clone(), seen.clone());
                    tokio::spawn(async move {
                        let mut request = vec![0; 8192];
                        let n = socket.read(&mut request).await.unwrap_or(0);
                        let head = String::from_utf8_lossy(&request[..n]).to_string();
                        let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                        crate::lock(&seen).push(head);
                        if let Some((prefix, delay)) = slow
                            && path.starts_with(prefix)
                        {
                            tokio::time::sleep(delay).await;
                        }
                        let (status, body, gzip) = routes
                            .iter()
                            .find(|(prefix, ..)| path.starts_with(prefix))
                            .map(|(_, status, body, gzip)| (*status, body.clone(), *gzip))
                            .unwrap_or((404, b"{}".to_vec(), false));
                        let encoding = if gzip {
                            "Content-Encoding: gzip\r\n"
                        } else {
                            ""
                        };
                        let head = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{encoding}Content-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                    });
                }
            });
            Self { url, heads, task }
        }

        fn requests(&self) -> Vec<String> {
            crate::lock(&self.heads).clone()
        }

        /// The paths asked for, as `/models?...`.
        fn paths(&self) -> Vec<String> {
            self.requests()
                .iter()
                .map(|head| head.split_whitespace().nth(1).unwrap().to_string())
                .collect()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn key(settings: &Settings, server: &Server) -> Key {
        settings.filters(Some(&server.url))
    }

    /// The seven choices are saved as OpenRouter's `provider` object and
    /// read back, and what the file holds besides stays as it was.
    #[test]
    fn settings_are_kept_and_a_field_written_by_hand_stays() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert_eq!(Settings::load(root), Settings::default());
        assert_eq!(Settings::default().summary(), None);
        std::fs::write(
            root.join(FILE),
            r#"{"provider": {"max_price": {"prompt": 3.0}, "allow_fallbacks": false, "order": ["azure"]},
                "recent": ["z-ai/glm-5", {"slug": "moonshotai/kimi-k3", "label": "Kimi K3", "context": 262144, "input_price": 0.6, "output_price": 2.5}],
                "something_new": {"a": 1}}"#,
        )
        .unwrap();
        let mut settings = Settings::read(root).unwrap();
        settings.set_zdr(true);
        settings.set_deny_data_collection(true);
        settings.set_region(Region::UsOrEu);
        settings.set_sort(Sort::Throughput);
        settings.set_precision(Precision::Fp8);
        settings.set_only(slugs("Anthropic, google-vertex"));
        settings.set_ignore(slugs("some-provider"));
        settings.save(root).unwrap();

        let text = std::fs::read_to_string(root.join(FILE)).unwrap();
        let file: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(file["region"], "us-or-eu");
        assert_eq!(file["provider"]["zdr"], true);
        assert_eq!(file["provider"]["data_collection"], "deny");
        assert_eq!(file["provider"]["sort"], "throughput");
        assert_eq!(
            file["provider"]["quantizations"],
            json!(["fp8", "mxfp8", "fp16", "bf16", "fp32", "unknown"])
        );
        assert_eq!(
            file["provider"]["only"],
            json!(["anthropic", "google-vertex"])
        );
        assert_eq!(file["provider"]["ignore"], json!(["some-provider"]));
        // What the controls don't show is kept.
        assert_eq!(file["provider"]["max_price"], json!({"prompt": 3.0}));
        assert_eq!(file["provider"]["allow_fallbacks"], false);
        assert_eq!(file["provider"]["order"], json!(["azure"]));
        assert_eq!(file["something_new"], json!({"a": 1}));
        // A slug alone stays a slug, and a snapshot stays as written.
        assert_eq!(
            file["recent"],
            json!([
                "z-ai/glm-5",
                {"slug": "moonshotai/kimi-k3", "label": "Kimi K3", "context": 262144,
                 "input_price": 0.6, "output_price": 2.5}
            ])
        );

        let again = Settings::read(root).unwrap();
        assert_eq!(again, settings);
        assert!(again.zdr() && again.deny_data_collection());
        assert_eq!(again.region(), Region::UsOrEu);
        assert_eq!(again.sort(), Sort::Throughput);
        assert_eq!(again.precision(), Precision::Fp8);

        // Turned off, a choice leaves the file.
        let mut off = again;
        off.set_zdr(false);
        off.set_deny_data_collection(false);
        off.set_region(Region::Any);
        off.set_sort(Sort::Default);
        off.set_precision(Precision::Any);
        off.set_only(Vec::new());
        off.set_ignore(Vec::new());
        off.save(root).unwrap();
        let file: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(FILE)).unwrap()).unwrap();
        assert_eq!(
            file["provider"],
            json!({"max_price": {"prompt": 3.0}, "allow_fallbacks": false, "order": ["azure"]})
        );
        assert!(file.get("region").is_none());
    }

    /// A file that can't be read is an error for whoever must not guess
    /// (a session start), and none set for a view.
    #[test]
    fn a_file_that_cant_be_read_is_an_error_and_not_a_guess() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(FILE), "{ not json").unwrap();
        let error = Settings::read(temp.path()).unwrap_err();
        assert!(error.contains("openrouter.json can't be read"), "{error}");
        assert_eq!(Settings::load(temp.path()), Settings::default());
        std::fs::write(temp.path().join(FILE), r#"{"region": "mars"}"#).unwrap();
        assert!(Settings::read(temp.path()).is_err());
    }

    #[test]
    fn a_precision_written_by_hand_is_custom_and_kept() {
        let mut settings: Settings =
            serde_json::from_str(r#"{"provider": {"quantizations": ["int8"]}}"#).unwrap();
        assert_eq!(settings.precision(), Precision::Custom);
        settings.set_precision(Precision::Custom);
        assert_eq!(settings.precision(), Precision::Custom);
        settings.set_precision(Precision::Bf16);
        assert_eq!(settings.precision(), Precision::Bf16);
        // The order of the list doesn't matter.
        let shuffled: Settings = serde_json::from_str(
            r#"{"provider": {"quantizations": ["unknown", "fp32", "bf16", "fp16"]}}"#,
        )
        .unwrap();
        assert_eq!(shuffled.precision(), Precision::Bf16);
    }

    #[test]
    fn what_is_set_is_said_in_a_few_words() {
        let mut settings = Settings::default();
        settings.set_zdr(true);
        settings.set_region(Region::Us);
        assert_eq!(
            settings.summary().as_deref(),
            Some("Zero data retention, US only")
        );
        settings.set_region(Region::UsOrEu);
        settings.set_deny_data_collection(true);
        settings.set_sort(Sort::Price);
        settings.set_precision(Precision::Bf16);
        settings.set_only(slugs("anthropic google-vertex"));
        settings.set_ignore(slugs("a b c d"));
        assert_eq!(
            settings.summary().as_deref(),
            Some(
                "Zero data retention, US or EU only, price first, 16-bit or more, only anthropic and google-vertex, never 4 providers"
            )
        );
        assert_eq!(slugs("none"), Vec::<String>::new());
        assert_eq!(slugs("a, a,b"), ["a", "b"]);
        let lines = settings.lines(None);
        assert_eq!(lines.len(), 7);
        assert!(
            lines[2].contains("Region") && lines[2].ends_with("US or EU"),
            "{lines:?}"
        );
    }

    /// The models picked come first, newest first, once each, eight at most.
    #[test]
    fn the_models_picked_are_remembered_newest_first_eight_at_most() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for n in 0..10 {
            remember(root, &format!("openrouter:acme/model-{n}"));
        }
        remember(root, "claude-opus-5-5");
        remember(root, "openrouter:acme/model-5");
        remember(root, "OpenRouter:acme/model-5");
        let settings = Settings::load(root);
        assert_eq!(settings.recent().len(), RECENT);
        assert_eq!(
            settings.recent_names()[..3],
            [
                "openrouter:acme/model-5",
                "openrouter:acme/model-9",
                "openrouter:acme/model-8"
            ]
        );
        assert!(
            !settings
                .recent_names()
                .contains(&"openrouter:acme/model-1".to_string())
        );
        assert_eq!(recent(root).len(), RECENT);
    }

    /// A session open on a folder, with a feed, as a front end opens one:
    /// it has the key it started with.
    fn session(root: &Path) -> (Arc<crate::keys::Keys>, crate::remote::Feed) {
        let feed = crate::remote::Feed::new("main");
        let keys = crate::keys::Keys::new(
            Arc::new(arko_ai::LLMClient::new()),
            None,
            None,
            Some(feed.clone()),
        );
        keys.use_openrouter_key("sk-or-first");
        keys.share(root.to_path_buf());
        // (Reached, as it is once the key is used.)
        keys.apply(&crate::credentials::OPENROUTER, Some("sk-or-first"));
        (keys, feed)
    }

    fn notices(feed: &crate::remote::Feed) -> usize {
        feed.subscribe()
            .0
            .items
            .iter()
            .filter(|item| format!("{item:?}").contains("OpenRouter settings changed"))
            .count()
    }

    fn zdr_installed(keys: &crate::keys::Keys) -> bool {
        keys.installed()
            .and_then(|(_, options)| options.provider)
            .is_some_and(|provider| provider.zdr == Some(true))
    }

    /// A saved change is installed in the sessions open in this process
    /// before the save returns, and they say so, except one that made the
    /// change itself. A model picked changes nothing a request carries, so
    /// it says nothing.
    #[tokio::test]
    async fn a_saved_change_is_installed_in_the_open_sessions_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (keys, feed) = session(root);
        let (other, other_feed) = session(root);
        assert!(!zdr_installed(&keys));
        remember(root, "openrouter:acme/model");
        assert!(!zdr_installed(&keys));
        assert_eq!(notices(&feed), 0);

        // Saved by an app, nobody's own session.
        Settings::update(root, |settings| settings.set_zdr(true)).unwrap();
        assert!(zdr_installed(&keys) && zdr_installed(&other));
        assert_eq!((notices(&feed), notices(&other_feed)), (1, 1));
        assert_eq!(keys.installed().unwrap().0, "sk-or-first");

        // Saved by a session: it has it, and its own words say so.
        Settings::edit(root, keys.id(), |settings| settings.set_zdr(false)).unwrap();
        assert!(!zdr_installed(&keys) && !zdr_installed(&other));
        assert_eq!((notices(&feed), notices(&other_feed)), (1, 2));
        // The models picked later don't say anything.
        remember(root, "openrouter:acme/another");
        assert_eq!((notices(&feed), notices(&other_feed)), (1, 2));
        // Another folder's sessions aren't told.
        let elsewhere = tempfile::tempdir().unwrap();
        Settings::update(elsewhere.path(), |settings| settings.set_zdr(true)).unwrap();
        assert!(!zdr_installed(&keys));
    }

    /// A change to the models picked and a change to the settings, made at
    /// once, both stay in the file, and the sessions have the settings. (The
    /// models picked once wrote the file as they had read it, over a change
    /// made meanwhile.)
    #[tokio::test]
    async fn a_model_picked_and_a_change_made_at_once_both_stay() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (keys, _feed) = session(&root);
        let (wait_for_it, go) = pause_next_change(&root);
        // The model picked has read the file, with zero data retention off.
        let picked = {
            let root = root.clone();
            std::thread::spawn(move || remember(&root, "openrouter:acme/picked"))
        };
        wait_for_it.recv().unwrap();
        let changed = {
            let root = root.clone();
            std::thread::spawn(move || Settings::update(&root, |settings| settings.set_zdr(true)))
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(!changed.is_finished(), "the change waits its turn");
        go.send(()).unwrap();
        picked.join().unwrap();
        changed.join().unwrap().unwrap();
        let file: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(FILE)).unwrap()).unwrap();
        assert_eq!(file["provider"]["zdr"], true, "{file}");
        assert_eq!(file["recent"], json!(["acme/picked"]), "{file}");
        assert!(zdr_installed(&keys));
        // And the other way: the change has read, and the model waits.
        let (wait_for_it, go) = pause_next_change(&root);
        let changed = {
            let root = root.clone();
            std::thread::spawn(move || Settings::update(&root, |s| s.set_region(Region::Eu)))
        };
        wait_for_it.recv().unwrap();
        let picked = {
            let root = root.clone();
            std::thread::spawn(move || remember(&root, "openrouter:acme/later"))
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(!picked.is_finished());
        go.send(()).unwrap();
        changed.join().unwrap().unwrap();
        picked.join().unwrap();
        let now = Settings::read(&root).unwrap();
        assert_eq!(now.region(), Region::Eu);
        assert!(now.zdr());
        assert_eq!(now.recent_names()[0], "openrouter:acme/later");
    }

    /// Settings saved whole don't write the models picked back as they were
    /// read, and an unreadable file is left alone.
    #[test]
    fn saving_whole_settings_keeps_the_models_picked_since() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut settings = Settings::load(root);
        remember(root, "openrouter:acme/picked");
        settings.set_zdr(true);
        settings.save(root).unwrap();
        let now = Settings::read(root).unwrap();
        assert!(now.zdr());
        assert_eq!(now.recent_names(), ["openrouter:acme/picked"]);
        std::fs::write(root.join(FILE), "{ oops").unwrap();
        assert!(settings.save(root).is_err());
        assert_eq!(std::fs::read_to_string(root.join(FILE)).unwrap(), "{ oops");
        let error = Settings::update(root, |s| s.set_zdr(false)).unwrap_err();
        assert!(error.contains("openrouter.json can't be read"), "{error}");
    }

    /// Another Arko process holds the file's lock: a change waits for it, and
    /// goes on once it lets go. A holder that has held it longer than the old
    /// stale age, as the file's age says, is still the holder: nothing takes
    /// the lock over, the wait ends with a clear error, and the lock file is
    /// never removed.
    #[test]
    fn a_live_holder_keeps_the_lock_and_a_waiter_gives_up_cleanly() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("{FILE}.lock"));
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        held.lock().unwrap();
        // Long held, as far as the file's age says.
        held.set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
        LOCK_WAIT.with(|wait| wait.set(Some(Duration::from_millis(400))));
        let started = Instant::now();
        let error = Settings::update(&root, |s| s.set_zdr(true)).unwrap_err();
        assert!(error.contains("is in use by another change"), "{error}");
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(350) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        assert!(path.exists(), "the lock file stays");
        assert!(!Settings::read(&root).unwrap().zdr(), "nothing was changed");

        // A change made while the holder lets go goes on, and so does the
        // one after it.
        LOCK_WAIT.with(|wait| wait.set(Some(Duration::from_secs(5))));
        let waiting = {
            let root = root.clone();
            std::thread::spawn(move || Settings::update(&root, |s| s.set_zdr(true)))
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(!waiting.is_finished(), "the holder is still there");
        drop(held);
        waiting.join().unwrap().unwrap();
        assert!(Settings::read(&root).unwrap().zdr());
        Settings::update(&root, |s| s.set_zdr(false)).unwrap();
        assert!(path.exists(), "the lock file is never removed");
    }

    /// A file system that can't lock files can't have the settings changed:
    /// another Arko process could change the file meanwhile, and the privacy
    /// settings may be in it. Nothing is written, no client is installed, and
    /// the message says why. Any other lock error fails the same way.
    #[tokio::test]
    async fn a_file_system_without_locks_changes_nothing_and_installs_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        Settings::update(&root, |s| s.set_zdr(true)).unwrap();
        let (keys, _feed) = session(&root);
        let before = std::fs::read_to_string(root.join(FILE)).unwrap();
        let installed = keys.installed().unwrap();
        for (kind, said) in [
            (
                std::io::ErrorKind::Unsupported,
                "can't lock files, so the OpenRouter settings can't be changed there.",
            ),
            (std::io::ErrorKind::PermissionDenied, "Couldn't lock"),
        ] {
            LOCK_FAILS.with(|fails| fails.set(Some(kind)));
            let error = Settings::update(&root, |s| s.set_zdr(false)).unwrap_err();
            assert!(error.contains(said), "{kind:?}: {error}");
            // The models picked are kept the same way, so nothing is written
            // behind its back either.
            remember(&root, "openrouter:acme/model");
            LOCK_FAILS.with(|fails| fails.set(None));
            assert_eq!(std::fs::read_to_string(root.join(FILE)).unwrap(), before);
            assert_eq!(
                keys.installed().unwrap(),
                installed,
                "no client was installed"
            );
        }
        // And on a folder with no file yet: none is made.
        let empty = temp.path().join("empty");
        LOCK_FAILS.with(|fails| fails.set(Some(std::io::ErrorKind::Unsupported)));
        assert!(Settings::update(&empty, |s| s.set_zdr(true)).is_err());
        LOCK_FAILS.with(|fails| fails.set(None));
        assert!(!empty.join(FILE).exists());
        // With locks again, it goes on.
        Settings::update(&root, |s| s.set_zdr(false)).unwrap();
        assert!(!zdr_installed(&keys));
    }

    /// A change waits for a change going on in this process too, for a
    /// bounded time.
    #[test]
    fn a_change_waits_for_one_in_this_process_a_bounded_time() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (reached, go) = pause_next_change(&root);
        let first = {
            let root = root.clone();
            std::thread::spawn(move || Settings::update(&root, |s| s.set_zdr(true)))
        };
        reached.recv().unwrap();
        LOCK_WAIT.with(|wait| wait.set(Some(Duration::from_millis(300))));
        let error = Settings::update(&root, |s| s.set_deny_data_collection(true)).unwrap_err();
        assert!(error.contains("is in use by another change"), "{error}");
        go.send(()).unwrap();
        first.join().unwrap().unwrap();
        let now = Settings::read(&root).unwrap();
        assert!(now.zdr() && !now.deny_data_collection());
    }

    /// A lock that can't be made is an error at once, not a wait.
    #[test]
    fn a_lock_that_cant_be_made_is_an_error_and_not_a_wait() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(format!("{FILE}.lock"))).unwrap();
        let started = Instant::now();
        let error = Settings::update(temp.path(), |s| s.set_zdr(true)).unwrap_err();
        assert!(error.contains("Couldn't lock"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// An empty file, or one of blanks, isn't settings: only a missing file
    /// is none set. (A truncated file must not read as unrestricted.)
    #[test]
    fn an_empty_file_isnt_settings_and_only_a_missing_one_is_none_set() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert_eq!(Settings::read(root).unwrap(), Settings::default());
        for text in ["", "  \n\t ", "\n"] {
            std::fs::write(root.join(FILE), text).unwrap();
            let error = Settings::read(root).unwrap_err();
            assert!(
                error.contains("openrouter.json can't be read"),
                "{text:?}: {error}"
            );
            assert!(
                Settings::read_to_use(root)
                    .unwrap_err()
                    .contains("Fix the file")
            );
            let error = Settings::update(root, |s| s.set_zdr(true)).unwrap_err();
            assert!(error.contains("can't be read"), "{error}");
            assert_eq!(std::fs::read_to_string(root.join(FILE)).unwrap(), text);
        }
        std::fs::remove_file(root.join(FILE)).unwrap();
        assert_eq!(Settings::read(root).unwrap(), Settings::default());
    }

    /// What draws asks the settings as this process has them, and reads no
    /// file when it has none yet: it asks for them in the background, says
    /// nothing meanwhile, and asks only once, an unreadable file too.
    #[test]
    fn what_draws_reads_no_file_before_the_settings_are_known() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("never-read");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(FILE), "{ oops").unwrap();
        assert_eq!(reads_of(&root), 0);
        for _ in 0..5 {
            assert!(completions(&root, "openrouter:moon").is_empty());
            assert!(!loading_for(&root));
        }
        // The background read, once.
        let started = Instant::now();
        while Settings::cached(&root).is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the settings are read"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let reads = reads_of(&root);
        assert!(reads >= 1);
        for _ in 0..5 {
            assert!(completions(&root, "openrouter:moon").is_empty());
            assert!(!loading_for(&root));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(reads_of(&root), reads, "no further read as it draws");
    }

    #[test]
    fn requests_carry_the_settings_and_the_region_sets_the_address() {
        let mut settings = Settings::default();
        assert_eq!(settings.base_url(), "https://openrouter.ai/api/v1");
        assert_eq!(settings.options().provider, None);
        settings.set_zdr(true);
        settings.set_sort(Sort::Latency);
        let provider = settings.options().provider.unwrap();
        assert_eq!(provider.zdr, Some(true));
        assert_eq!(
            provider.sort.and_then(|sort| sort.by()),
            Some(SortBy::Latency)
        );
        settings.set_region(Region::Us);
        assert_eq!(settings.base_url(), "https://us.openrouter.ai/api/v1");
        settings.set_region(Region::Eu);
        assert_eq!(settings.base_url(), "https://eu.openrouter.ai/api/v1");
        // US or EU starts at the US, and the models only the EU offers go
        // to the EU (see `a_list_for_us_or_eu_is_both_regions_lists`).
        settings.set_region(Region::UsOrEu);
        assert_eq!(settings.base_url(), "https://us.openrouter.ai/api/v1");
    }

    #[test]
    fn a_model_is_described_for_choosing() {
        let model = |context, input, output| Listed {
            name: "openrouter:moonshotai/kimi-k3".into(),
            label: "Kimi K3".into(),
            context,
            input_price: input,
            output_price: output,
            zone: None,
        };
        let kimi = model(Some(262_144), Some(0.6), Some(2.5));
        assert_eq!(kimi.slug(), "moonshotai/kimi-k3");
        assert_eq!(
            kimi.details(),
            "262k context, $0.60 in and $2.50 out per million tokens"
        );
        assert_eq!(kimi.short(), "262k · $0.60/$2.50");
        let big = model(Some(1_048_576), Some(15.0), Some(0.075));
        assert_eq!(big.short(), "1M · $15.00/$0.075");
        assert_eq!(model(Some(2_000_000), None, None).short(), "2M");
        assert_eq!(model(None, None, None).details(), "");
        assert_eq!(
            model(Some(131_072), Some(0.0), Some(0.0)).details(),
            "131k context, free"
        );
        assert!(kimi.matches("kimi"));
        assert!(kimi.matches("KIMI moonshot"));
        assert!(kimi.matches("k3"));
        assert!(!kimi.matches("glm"));
    }

    fn catalog_of(names: &[(&str, &str)]) -> Catalog {
        Catalog {
            models: names
                .iter()
                .map(|(slug, label)| Listed {
                    name: format!("openrouter:{slug}"),
                    label: label.to_string(),
                    context: None,
                    input_price: None,
                    output_price: None,
                    zone: None,
                })
                .collect(),
            ..Catalog::default()
        }
    }

    #[test]
    fn the_list_is_searched_by_name_and_slug_with_the_best_matches_first() {
        let catalog = catalog_of(&[
            ("acme/big-kimi-clone", "Big Kimi Clone"),
            ("moonshotai/kimi-k3", "Kimi K3"),
            ("z-ai/glm-5", "GLM 5"),
        ]);
        let slugs = |query| {
            catalog
                .search(query)
                .iter()
                .map(|model| model.slug().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            slugs(""),
            ["acme/big-kimi-clone", "moonshotai/kimi-k3", "z-ai/glm-5"]
        );
        assert_eq!(slugs("kimi"), ["moonshotai/kimi-k3", "acme/big-kimi-clone"]);
        assert_eq!(slugs("z-ai glm"), ["z-ai/glm-5"]);
        assert!(slugs("nothing").is_empty());
        assert_eq!(
            catalog
                .get("openrouter:z-ai/glm-5")
                .map(|m| m.label.as_str()),
            Some("GLM 5")
        );
        assert!(catalog.get("z-ai/glm-5").is_some());
        assert!(catalog.get("z-ai/glm-6").is_none());
    }

    #[test]
    fn a_catalog_says_when_it_was_fetched() {
        let mut catalog = catalog_of(&[("a/b", "B")]);
        assert_eq!(catalog.updated(), None);
        assert_eq!(catalog.summary(), "1");
        let ago = |seconds| Some(SystemTime::now() - Duration::from_secs(seconds));
        for (seconds, said) in [
            (5, "updated just now"),
            (60, "updated 1 minute ago"),
            (7300, "updated 2 hours ago"),
            (3 * 86400, "updated 3 days ago"),
        ] {
            catalog.fetched = ago(seconds);
            assert_eq!(catalog.updated().as_deref(), Some(said));
        }
        assert_eq!(catalog.summary(), "1, updated 3 days ago");
        assert_eq!(Catalog::default().summary(), "not loaded");
    }

    /// The list is fetched with the filters, without a key and gzipped, the
    /// `:batch` rows are left out, and it is kept in memory for ten minutes.
    #[tokio::test]
    async fn the_list_is_fetched_filtered_and_kept_in_memory() {
        let body = list(vec![
            entry(
                "moonshotai/kimi-k3",
                "MoonshotAI: Kimi K3",
                262_144,
                "0.0000006",
                "0.0000025",
            ),
            entry(
                "anthropic/claude-opus-5.5:batch",
                "Batch",
                1_000_000,
                "0.000001",
                "0.000005",
            ),
            entry(
                "acme/free-one:free",
                "Acme: Free One (free)",
                32_768,
                "0",
                "0",
            ),
        ]);
        let server = Server::start(vec![("/models", 200, gzipped(&body), true)]).await;
        let mut settings = Settings::default();
        let key = key(&settings, &server);
        assert_eq!(snapshot(&key), Catalog::default());

        let catalog = refresh_at(&key, false, None).await;
        assert_eq!(catalog.problem, None);
        assert!(catalog.fetched.is_some());
        let names: Vec<&str> = catalog.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "openrouter:moonshotai/kimi-k3",
                "openrouter:acme/free-one:free"
            ],
            "the :batch row is left out"
        );
        let kimi = &catalog.models[0];
        assert_eq!(kimi.label, "Kimi K3");
        assert_eq!(kimi.context, Some(262_144));
        assert_eq!(kimi.input_price, Some(0.6));
        assert_eq!(kimi.output_price, Some(2.5));
        assert_eq!(kimi.zone, None);
        assert_eq!(catalog.models[1].details(), "33k context, free");

        let requests = server.requests();
        assert_eq!(server.paths(), ["/models?supported_parameters=tools"]);
        let head = requests[0].to_ascii_lowercase();
        assert!(!head.contains("authorization"), "no key is sent: {head}");
        assert!(head.contains("accept-encoding: gzip"), "{head}");

        // Fresh: no second fetch, but a forced one fetches.
        assert_eq!(refresh_at(&key, false, None).await, catalog);
        assert_eq!(server.requests().len(), 1);
        refresh_at(&key, true, None).await;
        assert_eq!(server.requests().len(), 2);

        // Zero data retention is another list, fetched with `zdr=true`.
        settings.set_zdr(true);
        let zdr = self::key(&settings, &server);
        assert_eq!(snapshot(&zdr).models.len(), 0);
        refresh_at(&zdr, false, None).await;
        assert_eq!(
            server.paths().last().unwrap(),
            "/models?supported_parameters=tools&zdr=true"
        );
        assert_eq!(snapshot(&zdr).models.len(), 2);
    }

    /// The list is plain JSON too, and a list that is old is fetched again.
    #[tokio::test]
    async fn an_old_list_is_fetched_again_and_a_plain_body_is_read() {
        let body = list(vec![entry("a/b", "A: B", 8_000, "0.000001", "0.000002")]);
        let server = Server::start(vec![("/models", 200, body, false)]).await;
        let key = key(&Settings::default(), &server);
        refresh_at(&key, false, None).await;
        assert_eq!(server.requests().len(), 1);
        // Ten minutes on.
        memory().get_mut(&key).unwrap().loaded.as_mut().unwrap().at -=
            FRESH + Duration::from_secs(1);
        assert!(!is_fresh(&key));
        refresh_at(&key, false, None).await;
        assert_eq!(server.requests().len(), 2);
        assert!(is_fresh(&key));
    }

    /// A fetch that fails leaves the list as it was and says why, and isn't
    /// tried again for a while unless asked.
    #[tokio::test]
    async fn a_failed_fetch_keeps_the_list_and_says_why() {
        let body = list(vec![entry("a/b", "A: B", 8_000, "0.000001", "0.000002")]);
        let server = Server::start(vec![("/models", 200, body, false)]).await;
        let key = key(&Settings::default(), &server);
        refresh_at(&key, false, None).await;
        drop(server);

        let broken = Server::start(vec![("/models", 503, b"{}".to_vec(), false)]).await;
        let broken_key = Key {
            local: Some(broken.url.clone()),
            ..key.clone()
        };
        let catalog = refresh_at(&broken_key, false, None).await;
        assert!(catalog.models.is_empty());
        assert_eq!(
            catalog.problem.as_deref(),
            Some("Couldn't load the models (OpenRouter answered 503)")
        );
        assert_eq!(broken.requests().len(), 1);
        // Not again at once.
        refresh_at(&broken_key, false, None).await;
        assert_eq!(broken.requests().len(), 1);
        refresh_at(&broken_key, true, None).await;
        assert_eq!(broken.requests().len(), 2);

        // With a list in memory, the failure says the update failed.
        memory().get_mut(&key).unwrap().loaded.as_mut().unwrap().at -=
            FRESH + Duration::from_secs(1);
        let catalog = refresh_at(&key, false, None).await;
        assert_eq!(catalog.models.len(), 1, "the list stays");
        let problem = catalog.problem.unwrap();
        assert!(
            problem.starts_with("Couldn't update the models ("),
            "{problem}"
        );
    }

    #[tokio::test]
    async fn a_list_that_isnt_a_list_is_a_problem() {
        let server = Server::start(vec![("/models", 200, b"[1]".to_vec(), false)]).await;
        let key = key(&Settings::default(), &server);
        let catalog = refresh_at(&key, false, None).await;
        assert!(catalog.models.is_empty());
        assert_eq!(
            catalog.problem.as_deref(),
            Some("Couldn't load the models (its list held no models)")
        );
    }

    /// US or EU lists the models of both regions, each with the zone it is
    /// served from. A model both offer goes to the US.
    #[tokio::test]
    async fn a_list_for_us_or_eu_is_both_regions_lists() {
        let us = list(vec![
            entry(
                "anthropic/claude-opus-5.5",
                "Anthropic: Claude Opus 5.5",
                1_000_000,
                "0.000005",
                "0.000025",
            ),
            entry("acme/both", "Acme: Both", 64_000, "0.000001", "0.000002"),
        ]);
        let eu = list(vec![
            entry("acme/both", "Acme: Both", 64_000, "0.000001", "0.000002"),
            entry(
                "mistralai/eu-only",
                "Mistral: EU Only",
                128_000,
                "0.000001",
                "0.000002",
            ),
        ]);
        let server = Server::start(vec![
            ("/us/models", 200, gzipped(&us), true),
            ("/eu/models", 200, gzipped(&eu), true),
        ])
        .await;
        let mut settings = Settings::default();
        settings.set_region(Region::UsOrEu);
        settings.set_zdr(true);
        let key = key(&settings, &server);
        let catalog = refresh_at(&key, false, None).await;
        assert_eq!(catalog.problem, None);
        let zones: Vec<(&str, Option<Zone>)> = catalog
            .models
            .iter()
            .map(|model| (model.slug(), model.zone))
            .collect();
        assert_eq!(
            zones,
            [
                ("anthropic/claude-opus-5.5", Some(Zone::Us)),
                ("acme/both", Some(Zone::Us)),
                ("mistralai/eu-only", Some(Zone::Eu)),
            ]
        );
        let mut paths = server.paths();
        paths.sort();
        assert_eq!(
            paths,
            [
                "/eu/models?supported_parameters=tools&zdr=true",
                "/us/models?supported_parameters=tools&zdr=true"
            ]
        );

        // The models only the EU offers are sent there, the rest to the US,
        // and a model not listed (or no list yet) to the US.
        let eu_only = eu_only(key.clone());
        assert_eq!(eu_only("mistralai/eu-only"), Some(EU_HOST.to_string()));
        assert_eq!(
            eu_only("mistralai/eu-only:thinking"),
            Some(EU_HOST.to_string())
        );
        assert_eq!(eu_only("acme/both"), None);
        assert_eq!(eu_only("anthropic/claude-opus-5.5:floor"), None);
        assert_eq!(eu_only("nobody/unlisted"), None);
        let nothing = Key {
            local: Some("http://127.0.0.1:1".into()),
            ..key
        };
        assert_eq!(self::eu_only(nothing)("mistralai/eu-only"), None);
    }

    /// One region's list failing leaves the whole list as it was, since half
    /// of it would send a model to the wrong place.
    #[tokio::test]
    async fn a_list_for_us_or_eu_needs_both_regions() {
        let us = list(vec![entry("a/b", "A: B", 8_000, "0.000001", "0.000002")]);
        let server = Server::start(vec![
            ("/us/models", 200, us, false),
            ("/eu/models", 500, b"{}".to_vec(), false),
        ])
        .await;
        let mut settings = Settings::default();
        settings.set_region(Region::UsOrEu);
        let catalog = refresh_at(&key(&settings, &server), false, None).await;
        assert!(catalog.models.is_empty());
        assert_eq!(
            catalog.problem.as_deref(),
            Some("Couldn't load the models (OpenRouter answered 500)")
        );
    }

    #[tokio::test]
    async fn the_models_in_memory_size_a_models_context() {
        let temp = tempfile::tempdir().unwrap();
        let body = list(vec![entry("a/b", "A: B", 128_000, "0.000001", "0.000002")]);
        let server = Server::start(vec![("/models", 200, body, false)]).await;
        let settings = Settings::default();
        let catalog = checked(temp.path(), &settings, Some(&server.url))
            .await
            .unwrap();
        assert_eq!(catalog.get("a/b").unwrap().max_input_tokens, 128_000);
        let unreachable = checked(temp.path(), &settings, Some("http://127.0.0.1:1")).await;
        assert!(unreachable.is_err());
    }

    /// A model picked keeps what OpenRouter said of it, from the models in
    /// memory, and the models fetched later bring it up to date.
    #[tokio::test]
    async fn a_model_picked_keeps_what_openrouter_said_of_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let body = list(vec![entry(
            "moonshotai/kimi-k3",
            "MoonshotAI: Kimi K3",
            262_144,
            "0.0000006",
            "0.0000025",
        )]);
        let server = Server::start(vec![("/models", 200, body, false)]).await;
        let key = key(&Settings::default(), &server);

        // Before the models are known, the slug alone is kept, and what
        // the models say of it later fills in.
        remember_in(root, "openrouter:moonshotai/kimi-k3", Some(&server.url));
        let file = std::fs::read_to_string(root.join(FILE)).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&file).unwrap()["recent"],
            json!(["moonshotai/kimi-k3"])
        );
        assert_eq!(recent(root)[0].label, "Kimi K3", "from the slug");
        assert_eq!(recent(root)[0].context, None);
        assert!(known_in(root, "moonshotai/kimi-k3", Some(&server.url)).is_none());

        refresh_at(&key, false, Some(root)).await;
        let kimi = &recent(root)[0];
        assert_eq!(kimi.name, "openrouter:moonshotai/kimi-k3");
        assert_eq!(kimi.label, "Kimi K3");
        assert_eq!(kimi.context, Some(262_144));
        assert_eq!(
            (kimi.input_price, kimi.output_price),
            (Some(0.6), Some(2.5))
        );
        assert_eq!(
            kimi.details(),
            "262k context, $0.60 in and $2.50 out per million tokens"
        );

        // Picked with the models in memory: the details are written.
        remember_in(root, "openrouter:other/unlisted", Some(&server.url));
        remember_in(root, "openrouter:moonshotai/kimi-k3", Some(&server.url));
        let file: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(FILE)).unwrap()).unwrap();
        assert_eq!(
            file["recent"][0],
            json!({"slug": "moonshotai/kimi-k3", "label": "Kimi K3", "context": 262144,
                   "input_price": 0.6, "output_price": 2.5})
        );
        assert_eq!(file["recent"][1], "other/unlisted");

        // A model not listed (or no longer) keeps what it had.
        assert!(known_in(root, "other/unlisted", Some(&server.url)).is_none());
        assert_eq!(
            known_in(root, "moonshotai/kimi-k3", Some(&server.url))
                .unwrap()
                .max_input_tokens,
            262_144
        );
    }

    /// Without the models in memory, a model picked sizes a handover from
    /// what was kept.
    #[test]
    fn a_model_picked_gives_its_context_without_the_models_in_memory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(FILE),
            r#"{"recent": [{"slug": "acme/kept", "context": 131072}, "acme/bare"]}"#,
        )
        .unwrap();
        assert_eq!(
            known(temp.path(), "acme/kept").unwrap().max_input_tokens,
            131_072
        );
        assert!(known(temp.path(), "acme/bare").is_none());
        assert!(known(temp.path(), "acme/unknown").is_none());
    }

    /// Zero data retention covers no data collection, and the file keeps
    /// the two apart.
    #[test]
    fn zero_data_retention_covers_no_data_collection() {
        let mut settings = Settings::default();
        assert!(!settings.data_collection_implied());
        assert_eq!(Control::DataCollection.value(&settings), "Off");
        settings.set_zdr(true);
        assert!(settings.data_collection_implied());
        assert!(!settings.deny_data_collection());
        assert_eq!(Control::DataCollection.value(&settings), "On");
        assert_eq!(settings.summary().as_deref(), Some("Zero data retention"));
        settings.set_deny_data_collection(true);
        assert_eq!(settings.summary().as_deref(), Some("Zero data retention"));
        settings.set_zdr(false);
        assert_eq!(settings.summary().as_deref(), Some("No data collection"));
    }

    /// Settings no other test has, and a root that holds them, for a test
    /// that reads the models in memory by the root.
    fn own_root(zdr_and_region: Region) -> (tempfile::TempDir, Settings) {
        let temp = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.set_zdr(true);
        settings.set_region(zdr_and_region);
        settings.save(temp.path()).unwrap();
        (temp, settings)
    }

    /// A model typed as `openrouter:` and more completes from the models in
    /// memory, whichever models were picked before. Nothing else does.
    #[test]
    fn an_openrouter_model_completes_from_the_models_in_memory() {
        let (temp, settings) = own_root(Region::Eu);
        let mut slugs: Vec<String> = (0..60).map(|n| format!("acme/model-{n:02}")).collect();
        slugs.push("moonshotai/kimi-k3".into());
        let slugs: Vec<&str> = slugs.iter().map(String::as_str).collect();
        seed_for_tests(&settings, &slugs);
        let root = temp.path();
        assert_eq!(
            completions(root, "openrouter:moon"),
            ["openrouter:moonshotai/kimi-k3"]
        );
        assert_eq!(completions(root, "OpenRouter:MOON").len(), 1, "any case");
        assert_eq!(completions(root, "openrouter:").len(), 40, "forty at most");
        assert_eq!(
            completions(root, "openrouter:acme/model-0")[0],
            "openrouter:acme/model-00"
        );
        for typed in ["", "o", "openrouter", "opus", "moon"] {
            assert!(completions(root, typed).is_empty(), "{typed:?}");
        }
        assert!(completions(root, "openrouter:nothing").is_empty());
        // Another folder's settings are another list, empty until fetched.
        let other = tempfile::tempdir().unwrap();
        assert!(completions(other.path(), "openrouter:moon").is_empty());
        // Once the settings are known, completing (as the input box does at
        // every draw) needs no file: it goes by them as this process has them.
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            completions(root, "openrouter:moon"),
            ["openrouter:moonshotai/kimi-k3"]
        );
        let _ = loading_for(root);
        assert!(!root.exists(), "no file was made either");
    }

    /// The models of another list being fetched don't hold back the models
    /// the settings need now: a change of region while the first fetch is
    /// slow still fetches the new list, with no one asking again.
    #[tokio::test]
    async fn a_fetch_of_other_models_does_not_hold_back_the_models_needed_now() {
        let body = list(vec![entry("a/b", "A: B", 8_000, "0.000001", "0.000002")]);
        let server = Server::start_slow(
            vec![
                ("/slow/models", 200, body.clone(), false),
                ("/models", 200, body, false),
            ],
            Some(("/slow", Duration::from_millis(600))),
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let before = Key {
            local: Some(format!("{}/slow", server.url)),
            region: Region::Any,
            zdr: false,
        };
        let now = Key {
            local: Some(server.url.clone()),
            region: Region::Any,
            zdr: true,
        };
        refresh_key_soon(before.clone(), temp.path());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !fetching(&before) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the first fetch starts");
        assert!(!fetching(&now));
        refresh_key_soon(now.clone(), temp.path());
        tokio::time::timeout(Duration::from_secs(5), async {
            while snapshot(&now).models.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the new list is fetched without another call");
        assert!(fetching(&before), "the slow one was still going");
        tokio::time::timeout(Duration::from_secs(5), async {
            while snapshot(&before).models.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("and the first ends too");
        assert!(!fetching(&before) && !fetching(&now));
    }
}
