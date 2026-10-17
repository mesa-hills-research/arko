//! A session's details as data: what `/cost`, `/session`, `/mcp`,
//! `/skills` and `/workers` say, for a front end that shows them its own
//! way. The commands' text is made from the same facts. Each getter reads
//! files or the cost ledger, briefly: call them off a UI thread.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::agents::{Effort, ExtraRules};
pub use crate::crew::WorkerFacts;
pub use crate::providers::CostShare;
use crate::session::Settings;
use crate::status::Gauges;
use crate::{index, lock, mcp, project, providers, remote, skills};

/// When the agents hand over, as `/session` says it: `workers` when the
/// session has them.
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

/// The project's instructions, as `/session` says them: `workers`, whether
/// workers get them (`None` without workers).
pub fn project_text(files: project::Files, workers: Option<bool>) -> String {
    let workers = match workers {
        None => "",
        Some(true) => ", workers on",
        Some(false) => ", workers off",
    };
    format!(
        "project instructions {}{workers}: {}",
        files.name(),
        files.describe()
    )
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
    /// What the search indexes' embeddings cost while Arko ran: tokens and
    /// dollars.
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
    /// As `/session` says it: "claude-opus-5-5 (xhigh effort) via Claude Code".
    pub shown: String,
    pub effort: Effort,
    pub fast: bool,
    pub cache_1h: bool,
}

/// The workers' model and how many run at once.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkersFacts {
    pub model: ModelFacts,
    /// `None`: no limit.
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
    /// `None`: the session has no workers.
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
    /// After how many seconds shell commands move to the background (0:
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
    /// What it is for (empty for one always in the instructions).
    pub description: String,
    /// It is always in the agents' instructions (else they load it when
    /// they need it).
    pub always: bool,
}

/// What `/skills` says.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillsFacts {
    pub skills: Vec<SkillFacts>,
    /// The folders skills are read from.
    pub folders: Vec<PathBuf>,
    /// Arko's own skills folder.
    pub own_folder: PathBuf,
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

    /// What the session billed, and, where a plan covered calls, what they
    /// would have cost: the main agent's part and the workers', the plans'
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
        let plans = stats
            .plans
            .iter()
            .map(|(plan, text)| remote::PlanStatus {
                plan: plan.clone(),
                windows: stats.plan_windows.get(plan).cloned().unwrap_or_default(),
                text: text.clone(),
            })
            .collect();
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

    /// The skills the agents have.
    pub fn skills(&self) -> Result<SkillsFacts, String> {
        let workspace = self.session().workspace;
        let found =
            skills::list(&self.skill_folders, &workspace).map_err(|error| error.to_string())?;
        let mut skills: Vec<SkillFacts> = found
            .on_demand_skills
            .iter()
            .map(|skill| SkillFacts {
                name: skill.name.clone(),
                description: skill.description.clone(),
                always: false,
            })
            .collect();
        skills.extend(found.auto_loaded_skills.iter().map(|skill| SkillFacts {
            name: skill.name.clone(),
            description: String::new(),
            always: true,
        }));
        Ok(SkillsFacts {
            skills,
            folders: self.skill_folders.clone(),
            own_folder: self.root.join("skills"),
        })
    }

    /// The session's workers.
    pub fn workers(&self) -> Vec<WorkerFacts> {
        lock(&self.gauges.crew)
            .as_ref()
            .map(|crew| crew.facts())
            .unwrap_or_default()
    }

    /// What switching the session to `to` (`Request::SwitchMode`) costs:
    /// its next request writes the whole conversation to the prompt cache
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

/// What a switch between Chat and Agent costs: see [`Details::switch_cost`].
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
