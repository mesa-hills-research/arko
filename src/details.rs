//! A session's details as data, for a front end that shows what `/cost`,
//! `/session`, `/mcp`, `/skills` and `/workers` say in its own way, and the
//! search indexes the banner lists. The commands' text is made from the same
//! facts. Each getter briefly reads files or the cost ledger, so call them
//! off a UI thread.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::agents::{Effort, ExtraRules};
use crate::builtin_skills::BuiltinSkills;
pub use crate::crew::WorkerFacts;
pub use crate::providers::CostShare;
use crate::session::Settings;
use crate::status::Gauges;
use crate::{index, lock, mcp, project, providers, remote, skills};

/// When the agents hand over, as `/session` says it. `workers` is set when
/// the session has workers.
pub fn handover_text(facts: &HandoverFacts, workers: bool) -> String {
    let what = |compacts: bool| if compacts { "compaction" } else { "handover" };
    let main = format!(
        "{} at {} tokens",
        what(facts.main_compacts),
        crate::handover::shown(facts.main_at)
    );
    let worker = format!(
        "{} at {} tokens",
        what(facts.workers_compact),
        crate::handover::shown(facts.workers_at)
    );
    if !workers || main == worker {
        format!("{main} of input")
    } else {
        format!("{main} of input (main), {worker} (workers)")
    }
}

/// The project's instructions, as `/session` says them. `workers` says
/// whether workers get them (`None` without workers).
pub fn project_text(files: project::Files, workers: Option<bool>) -> String {
    let workers = match workers {
        None => "",
        Some(true) => ", workers on",
        Some(false) => ", workers off",
    };
    format!(
        "project instructions {}{workers} ({})",
        files.name(),
        files.describe()
    )
}

/// A search index of the session's workspace, one whose folder holds the
/// workspace or lies in it. These are the indexes the banner lists.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct IndexFacts {
    /// The name the agents' tools and `arko index` use.
    pub name: String,
    pub kind: IndexKind,
    /// The indexed folder.
    pub root: PathBuf,
    /// The index is set to stay current as files change while Arko runs.
    pub monitor: bool,
    /// Who keeps it current now.
    pub watch: IndexWatch,
    /// Its chunks, or `None` when it hasn't been built or its state can't
    /// be read.
    pub chunks: Option<u64>,
    /// When it was last built, and how many files that build indexed.
    pub built: Option<DateTime<Utc>>,
    pub files: Option<u64>,
    /// Its embeddings for semantic search, or `None` for an index searched
    /// by keywords only.
    pub embeddings: Option<IndexEmbeddings>,
    /// Why its last build or embedding run failed, until one succeeds.
    pub last_error: Option<String>,
}

impl IndexFacts {
    /// The index `name`, of `kind`, of the folder `root`, with nothing else
    /// known yet. It isn't monitored or built, and is searched by keywords.
    /// The fields are public, so an app sets the others (in its tests or
    /// previews, say).
    ///
    /// ```
    /// use arko::details::{IndexEmbeddings, IndexFacts, IndexKind, IndexWatch};
    ///
    /// let mut index = IndexFacts::new("app", IndexKind::Code, "/work/app");
    /// index.watch = IndexWatch::Here;
    /// index.chunks = Some(1_200);
    /// let mut embeddings = IndexEmbeddings::new("voyage-code-4");
    /// embeddings.embedded = 1_200;
    /// embeddings.complete = true;
    /// index.embeddings = Some(embeddings);
    /// assert!(index.monitored());
    /// ```
    pub fn new(name: impl Into<String>, kind: IndexKind, root: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            kind,
            root: root.into(),
            monitor: false,
            watch: IndexWatch::Off,
            chunks: None,
            built: None,
            files: None,
            embeddings: None,
            last_error: None,
        }
    }

    /// An Arko keeps the index current as files change (this one or
    /// another), which is what the banner calls monitored.
    pub fn monitored(&self) -> bool {
        matches!(self.watch, IndexWatch::Here | IndexWatch::Elsewhere)
    }
}

/// What an index holds, which chooses its embedding model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexKind {
    /// Source code.
    Code,
    /// Prose, such as Markdown and reStructuredText.
    Docs,
}

impl IndexKind {
    /// `code` or `docs`, as the banner and the agents' tools say it.
    pub fn as_str(self) -> &'static str {
        match self {
            IndexKind::Code => "code",
            IndexKind::Docs => "docs",
        }
    }
}

/// Who keeps an index current as files change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexWatch {
    /// No Arko watches it now, so a search brings it up to date first.
    Off,
    /// This Arko watches it (or is starting to).
    Here,
    /// Another Arko watches it.
    Elsewhere,
    /// This Arko's watcher failed, for the reason given, and tries again.
    Failed(String),
}

/// An index's embeddings.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct IndexEmbeddings {
    /// The embedding model, such as `voyage-code-4`.
    pub model: String,
    /// The chunks that have a vector, of the index's
    /// [`chunks`](IndexFacts::chunks).
    pub embedded: u64,
    /// Every chunk that can have a vector has one. Chunks too long for the
    /// model have none.
    pub complete: bool,
    /// An embedding run is adding vectors now.
    pub running: bool,
    /// The last run stopped at its token cap, and an explicit run
    /// continues it.
    pub cap_reached: bool,
    /// Arko has no Voyage key, so the index is searched by keywords until
    /// one is added.
    pub needs_key: bool,
    /// The tokens spent embedding this index in all, and what they cost in
    /// dollars.
    pub tokens: u64,
    pub dollars: f64,
}

impl IndexEmbeddings {
    /// Embeddings by the model `model`, none made yet. The fields are
    /// public, so an app sets the others. See [`IndexFacts::new`].
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            embedded: 0,
            complete: false,
            running: false,
            cap_reached: false,
            needs_key: false,
            tokens: 0,
            dollars: 0.0,
        }
    }
}

/// The facts of the indexes `indexes` reads as the workspace's.
pub(crate) fn index_facts(indexes: &index::Indexes) -> Vec<IndexFacts> {
    indexes
        .workspace_reports()
        .into_iter()
        .filter_map(|report| {
            let manifest = report.manifest.ok()?;
            let watch = match report.watch {
                index::WatchState::Off => IndexWatch::Off,
                index::WatchState::Starting | index::WatchState::Watching { .. } => {
                    IndexWatch::Here
                }
                index::WatchState::Elsewhere => IndexWatch::Elsewhere,
                index::WatchState::Failed(why) => IndexWatch::Failed(why),
            };
            let embeddings = manifest.embed.then(|| {
                let dense = report.dense.as_ref();
                IndexEmbeddings {
                    model: dense.map_or(manifest.model.clone(), |info| info.embedder.model.clone()),
                    embedded: dense.map_or(0, |info| info.embedded),
                    complete: dense
                        .is_some_and(|info| info.state == arko_index::dense::EmbedState::Complete),
                    running: report.embedding_active,
                    cap_reached: manifest.cap_reached,
                    needs_key: !report.has_key,
                    tokens: manifest.embed_tokens,
                    dollars: manifest.embed_cost,
                }
            });
            Some(IndexFacts {
                kind: match manifest.kind {
                    index::Kind::Code => IndexKind::Code,
                    index::Kind::Docs => IndexKind::Docs,
                },
                monitor: manifest.monitor,
                watch,
                chunks: report.chunks,
                built: manifest.last_build.as_ref().and_then(|build| build.at),
                files: manifest.last_build.as_ref().map(|build| build.files),
                embeddings,
                last_error: manifest.last_error,
                name: manifest.name,
                root: manifest.root,
            })
        })
        .collect()
}

/// A session's details.
pub struct Details {
    session_id: String,
    session_dir: PathBuf,
    /// Arko's folder.
    root: PathBuf,
    gauges: Arc<Gauges>,
    indexes: Option<Arc<index::Indexes>>,
    skill_folders: Vec<PathBuf>,
    mcp: Mutex<McpFacts>,
    /// The session's facts and settings, as the session last published
    /// them.
    session: Mutex<Option<(SessionFacts, Settings)>>,
}

/// What `/cost` says.
#[derive(Debug, Clone, PartialEq)]
pub struct CostReport {
    /// The main agent's part and the workers'.
    pub main: CostShare,
    pub workers: CostShare,
    /// Each plan's usage windows, as last reported.
    pub plans: Vec<remote::PlanStatus>,
    /// What the search indexes' embeddings cost while Arko ran, in tokens
    /// and dollars.
    pub embeddings: Option<(u64, f64)>,
}

impl CostReport {
    /// What was billed in all.
    pub fn billed(&self) -> f64 {
        self.main.billed + self.workers.billed
    }
}

/// A model and how the session runs it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelFacts {
    /// Its ID, as the session keeps it.
    pub model: String,
    /// As `/session` says it ("claude-opus-5-5 (xhigh effort) via Claude Code").
    pub shown: String,
    pub effort: Effort,
    pub fast: bool,
    pub cache_1h: bool,
}

/// The workers' model and how many run at once.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkersFacts {
    pub model: ModelFacts,
    /// `None` for no limit.
    pub max_running: Option<u32>,
}

/// When the agents hand over (or Claude Code compacts their conversation).
#[derive(Debug, Clone, PartialEq)]
pub struct HandoverFacts {
    pub main_at: usize,
    pub main_compacts: bool,
    /// New workers', on their model.
    pub workers_at: usize,
    pub workers_compact: bool,
}

/// What `/session` says.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionFacts {
    pub id: String,
    pub dir: PathBuf,
    pub workspace: PathBuf,
    pub title: Option<String>,
    pub created: DateTime<Utc>,
    pub main: ModelFacts,
    /// `None` when the session has no workers.
    pub workers: Option<WorkersFacts>,
    pub handover: HandoverFacts,
    pub text_replies: bool,
    pub extra_rules: ExtraRules,
    /// The agents on `cc:` models compact instead of handing over.
    pub cc_compact: bool,
    pub project_files: project::Files,
    /// Whether workers get the project instructions, or `None` without
    /// workers.
    pub project_workers: Option<bool>,
    pub outside: bool,
    /// After how many seconds shell commands move to the background (0 for
    /// never).
    pub background_after: u64,
    pub show_thinking: bool,
    pub hide_thinking: bool,
}

/// An MCP server the session started (or tried to).
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerFacts {
    pub name: String,
    /// Its command line, as its file writes it.
    pub shown: String,
    /// The file that sets it up.
    pub file: PathBuf,
    /// Its tools, as the server names them, or why it didn't start.
    pub tools: Result<Vec<String>, String>,
}

/// What a chat's [`McpFacts::notes`] say in place of servers, which a chat
/// doesn't start.
pub const CHAT_MCP_NOTE: &str = "A chat runs no MCP servers. Switch to Agent to use them.";

/// What `/mcp` says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpFacts {
    pub servers: Vec<McpServerFacts>,
    /// What was left out, and why.
    pub notes: Vec<String>,
    /// Arko's own MCP file, where servers for every project go.
    pub user_file: PathBuf,
    /// The tools a resumed session's agents weren't offered, as they started
    /// without them.
    pub unoffered: Option<String>,
}

impl McpFacts {
    /// The servers in `running`, and `notes`.
    pub(crate) fn of(running: &mcp::Running, notes: &[String], root: &std::path::Path) -> Self {
        McpFacts {
            servers: running
                .servers
                .iter()
                .map(|started| McpServerFacts {
                    name: started.server.name.clone(),
                    shown: started.server.shown.clone(),
                    file: started.server.file.clone(),
                    tools: started
                        .result
                        .as_ref()
                        .map(|tools| {
                            tools
                                .iter()
                                .map(|name| {
                                    mcp::split_tool_name(name)
                                        .map_or(name.clone(), |(_, tool)| tool.to_string())
                                })
                                .collect()
                        })
                        .map_err(Clone::clone),
                })
                .collect(),
            notes: notes.to_vec(),
            user_file: root.join(mcp::USER_FILE),
            unoffered: None,
        }
    }
}

/// A skill the agents can load, or that is always in their instructions.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillFacts {
    pub name: String,
    /// What it is for.
    pub description: String,
    /// It is always in the agents' instructions (else they load it when
    /// they need it).
    pub always: bool,
    /// It is one of Arko's built-in skills, held in the program and not in
    /// a folder.
    pub builtin: bool,
    /// It is a skill of the user's with the name of a built-in one, which it
    /// replaces. This is where it is, its folder or its file.
    pub replaces_builtin: Option<PathBuf>,
    /// Where it is, its folder or its file (none for a built-in skill).
    pub path: Option<PathBuf>,
    /// The models whose agents list it, as its front matter writes them
    /// (empty for every model).
    pub models: Vec<String>,
    /// Workers get it (else only the main agent).
    pub workers: bool,
    /// When, why and from where an agent saved it, if an agent did.
    pub added: Option<crate::agent_added::SkillRecord>,
}

/// What `/skills` says.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillsFacts {
    /// The skills the agents have: those of the folders, then the built-in
    /// ones that no skill of the folders replaces.
    pub skills: Vec<SkillFacts>,
    /// The folders skills are read from.
    pub folders: Vec<PathBuf>,
    /// Arko's own skills folder.
    pub own_folder: PathBuf,
    /// The built-in skills the session leaves out, by name (see
    /// [`Options::builtin_skills`](crate::Options::builtin_skills)).
    pub builtin_off: Vec<String>,
}

impl Details {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_id: String,
        session_dir: PathBuf,
        root: PathBuf,
        gauges: Arc<Gauges>,
        indexes: Option<Arc<index::Indexes>>,
        skill_folders: Vec<PathBuf>,
        mcp: McpFacts,
    ) -> Self {
        Self {
            session_id,
            session_dir,
            root,
            gauges,
            indexes,
            skill_folders,
            mcp: Mutex::new(mcp),
            session: Mutex::new(None),
        }
    }

    /// The session's facts change (a command, a model, a setting): what
    /// `/session` and `/cost` go by from now on.
    pub(crate) fn publish_session(&self, facts: SessionFacts, settings: Settings) {
        *lock(&self.session) = Some((facts, settings));
    }

    /// What a resumed session's agents weren't offered, as they started
    /// without it.
    pub(crate) fn set_unoffered(&self, note: Option<String>) {
        lock(&self.mcp).unoffered = note;
    }

    /// What the session billed, and what the calls a plan covered would
    /// have cost. It has the main agent's part and the workers', the plans'
    /// usage, and the embeddings.
    pub fn cost(&self) -> CostReport {
        let workflow = lock(&self.gauges.workflow).clone();
        let stats = lock(&self.gauges.stats).clone();
        let (main, workers) = match (&workflow, self.settings()) {
            (Some(workflow), Some(settings)) => {
                providers::cost_shares(&workflow.cost_ledger(), &settings)
            }
            _ => Default::default(),
        };
        let plans = stats.plan_status.values().cloned().collect();
        let embeddings = self
            .indexes
            .as_ref()
            .map(|indexes| indexes.spent())
            .filter(|(tokens, _)| *tokens > 0);
        CostReport {
            main,
            workers,
            plans,
            embeddings,
        }
    }

    /// The session's settings, as last published.
    fn settings(&self) -> Option<Settings> {
        lock(&self.session)
            .as_ref()
            .map(|(_, settings)| settings.clone())
    }

    /// The session's facts, as `/session` says them.
    pub fn session(&self) -> SessionFacts {
        lock(&self.session)
            .as_ref()
            .map(|(facts, _)| facts.clone())
            .unwrap_or_else(|| SessionFacts {
                id: self.session_id.clone(),
                dir: self.session_dir.clone(),
                workspace: PathBuf::new(),
                title: None,
                created: Utc::now(),
                main: ModelFacts {
                    model: String::new(),
                    shown: String::new(),
                    effort: Effort::Xhigh,
                    fast: false,
                    cache_1h: false,
                },
                workers: None,
                handover: HandoverFacts {
                    main_at: 0,
                    main_compacts: false,
                    workers_at: 0,
                    workers_compact: false,
                },
                text_replies: false,
                extra_rules: ExtraRules::default(),
                cc_compact: false,
                project_files: project::Files::default(),
                project_workers: None,
                outside: false,
                background_after: 0,
                show_thinking: false,
                hide_thinking: false,
            })
    }

    /// The MCP servers the session started, and what was left out.
    pub fn mcp(&self) -> McpFacts {
        lock(&self.mcp).clone()
    }

    /// The built-in skills the session gives its agents.
    pub(crate) fn builtin_skills(&self) -> BuiltinSkills {
        lock(&self.session)
            .as_ref()
            .map(|(_, settings)| settings.builtin_skills.clone())
            .unwrap_or_default()
    }

    /// The skills the agents have.
    pub fn skills(&self) -> Result<SkillsFacts, String> {
        let workspace = self.session().workspace;
        let builtin = self.builtin_skills();
        let scanned = skills::scan(&self.skill_folders, &builtin, &workspace);
        let loaded = || scanned.iter().filter(|skill| skill.skipped.is_none());
        let facts = |skill: &arko_agents::skill::SkillScan| SkillFacts {
            name: skill.name.clone(),
            description: skill.description.clone(),
            always: skill.auto_load,
            builtin: skill.embedded,
            // The built-in skill of its name is among the skills scanned,
            // and has left the loaded ones.
            replaces_builtin: (!skill.embedded
                && scanned
                    .iter()
                    .any(|other| other.embedded && other.name == skill.name))
            .then(|| skill.path.clone()),
            path: (!skill.embedded).then(|| skill.path.clone()),
            models: skill.models.clone(),
            workers: skill.workers,
            added: (!skill.embedded)
                .then(|| crate::agent_added::skill_at(&self.root, &skill.path))
                .flatten(),
        };
        let mut skills: Vec<SkillFacts> = loaded()
            .filter(|skill| !skill.auto_load)
            .map(facts)
            .collect();
        skills.extend(loaded().filter(|skill| skill.auto_load).map(facts));
        Ok(SkillsFacts {
            skills,
            folders: self.skill_folders.clone(),
            own_folder: self.root.join("skills"),
            builtin_off: builtin.left_out().into_iter().map(String::from).collect(),
        })
    }

    /// The search indexes of the session's workspace, by name, as the
    /// banner lists them. A chat has none, and neither does a session whose
    /// indexes couldn't be opened.
    pub fn indexes(&self) -> Vec<IndexFacts> {
        self.indexes.as_deref().map(index_facts).unwrap_or_default()
    }

    /// The session's workers.
    pub fn workers(&self) -> Vec<WorkerFacts> {
        lock(&self.gauges.crew)
            .as_ref()
            .map(|crew| crew.facts())
            .unwrap_or_default()
    }

    /// What switching the session to `to` (`Request::SwitchMode`) costs.
    /// Its next request writes the whole conversation to the prompt cache
    /// again, under the new mode's tools and instructions. `None` when the
    /// session is in `to` already or its conversation hasn't started (then
    /// it costs nothing).
    pub fn switch_cost(&self, to: crate::session::Mode) -> Option<SwitchCost> {
        let settings = lock(&self.session).as_ref()?.1.clone();
        if settings.mode == to {
            return None;
        }
        let tokens = lock(&self.gauges.stats).context_tokens?;
        Some(switch_cost(&settings, tokens))
    }
}

/// What a switch between Chat and Agent costs ([`Details::switch_cost`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchCost {
    /// The conversation's input at its last request, which the next one
    /// writes to the cache again.
    pub tokens: usize,
    /// What writing them costs at the model's price, or `None` on a plan.
    pub dollars: Option<f64>,
    /// The plan that pays instead ("your Claude plan").
    pub plan: Option<&'static str>,
}

fn switch_cost(settings: &Settings, tokens: usize) -> SwitchCost {
    let model = providers::resolve_model(&settings.main.model).ok();
    let plan = match model.as_ref().map(|model| &model.provider) {
        Some(arko_ai::Provider::ClaudeCode) => Some("your Claude plan"),
        Some(arko_ai::Provider::OpenAI)
            if settings.openai_auth == Some(providers::OpenAiAuth::ChatGpt) =>
        {
            Some("your ChatGPT plan")
        }
        _ => None,
    };
    let dollars = model.filter(|_| plan.is_none()).map(|model| {
        let (write, _) =
            providers::cache_prices(&model, tokens, settings.main.fast, settings.main.cache_1h);
        tokens as f64 * write / 1e6
    });
    SwitchCost {
        tokens,
        dollars,
        plan,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An app can make an index's facts and its embeddings, which start with
    /// nothing known.
    #[test]
    fn index_facts_and_embeddings_can_be_made() {
        let facts = IndexFacts::new("docs", IndexKind::Docs, "/work/docs");
        assert_eq!(
            (facts.name.as_str(), facts.kind, facts.root.as_path()),
            ("docs", IndexKind::Docs, std::path::Path::new("/work/docs"))
        );
        assert!(!facts.monitor && !facts.monitored());
        assert!(facts.chunks.is_none() && facts.built.is_none() && facts.embeddings.is_none());
        let embeddings = IndexEmbeddings::new("voyage-4");
        assert_eq!(
            (
                embeddings.model.as_str(),
                embeddings.embedded,
                embeddings.complete
            ),
            ("voyage-4", 0, false)
        );
    }

    /// A switch costs writing the conversation to the cache again: at the
    /// model's write price, or nothing on a plan.
    #[test]
    fn a_switch_costs_writing_the_conversation_again() {
        let model = crate::agents::ModelSettings {
            model: "claude-opus-5-5".into(),
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let mut settings = Settings::for_test(std::path::Path::new("/work"), &model);
        let opus = providers::resolve_model("opus").unwrap().pricing;
        let cost = switch_cost(&settings, 100_000);
        assert_eq!(cost.tokens, 100_000);
        assert_eq!(cost.plan, None);
        let expected = 0.1 * opus.cache_write_cost_per_million.unwrap();
        assert!((cost.dollars.unwrap() - expected).abs() < 1e-9);
        settings.main.cache_1h = true;
        let expected = 0.1 * opus.cache_write_1h_cost_per_million.unwrap();
        assert!((switch_cost(&settings, 100_000).dollars.unwrap() - expected).abs() < 1e-9);
        settings.main.model = "cc:opus".into();
        let cost = switch_cost(&settings, 100_000);
        assert_eq!((cost.dollars, cost.plan), (None, Some("your Claude plan")));
    }
}
