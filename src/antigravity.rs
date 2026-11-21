//! The Antigravity CLI runs `agy:` models on the user's Google AI plan.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use arko_agents::AntigravitySettings;
use arko_ai::antigravity as agy;

use crate::agents::Effort;

pub(crate) use agy::SignInProblem as Problem;

pub(crate) const INSTALL: &str = "Antigravity installed";
pub(crate) const SIGN_IN: &str = "Antigravity signed in";
pub(crate) const UPDATE: &str = "Antigravity updated";
pub(crate) const MODEL_ACCESS: &str = "access on your Google AI plan";
pub(crate) const UNSUPPORTED_HOST: &str = "This app can't run agy: models.";

const INSTALL_ACTION: &str = "Install the Antigravity CLI, then run `agy` to sign in.";
const SIGN_IN_ACTION: &str = "Run `agy` to sign in.";
const UPDATE_ACTION: &str = "Update it with `agy update`.";

pub(crate) fn action(needs: &str) -> Option<&'static str> {
    match needs {
        INSTALL => Some(INSTALL_ACTION),
        SIGN_IN => Some(SIGN_IN_ACTION),
        UPDATE => Some(UPDATE_ACTION),
        MODEL_ACCESS => Some("Choose another Antigravity model."),
        _ => None,
    }
}

pub(crate) const CHECK_FAILED: &str = "Couldn't check Antigravity sign-in.";

pub(crate) fn problem_text(problem: &Problem) -> String {
    match problem {
        Problem::Missing => INSTALL_ACTION.to_string(),
        Problem::SignedOut => SIGN_IN_ACTION.to_string(),
        Problem::Outdated { found } => format!(
            "Antigravity CLI {found} is too old for Arko, which needs {}. {UPDATE_ACTION}",
            agy::MINIMUM_VERSION
        ),
        Problem::Failed(error) => format!("{CHECK_FAILED} {error}"),
    }
}

pub(crate) struct Ready {
    #[cfg(feature = "tui")]
    pub tier: Option<String>,
    models: BTreeMap<String, ModelOffer>,
}

pub(crate) type Status = Result<Ready, Problem>;
const RECENT: Duration = Duration::from_secs(10);
static STATUS: RwLock<Option<(Instant, Arc<Status>)>> = RwLock::new(None);

fn last_check() -> Option<(Instant, Arc<Status>)> {
    STATUS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

pub(crate) fn status() -> Option<Arc<Status>> {
    #[cfg(test)]
    if let Ok(status) = TEST_STATUS.try_with(Arc::clone) {
        return Some(status);
    }
    last_check().map(|(_, status)| status)
}

pub(crate) fn recent() -> Option<Arc<Status>> {
    last_check()
        .filter(|(at, _)| at.elapsed() < RECENT)
        .map(|(_, status)| status)
}

pub(crate) async fn check_recent() -> Arc<Status> {
    if let Some(status) = recent() {
        status
    } else {
        check().await
    }
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEST_STATUS: Arc<Status>;
}

#[cfg(test)]
pub(crate) fn test_offer(id: &str, name: &str, levels: &[&str]) -> Arc<Status> {
    Arc::new(Ok(Ready {
        #[cfg(feature = "tui")]
        tier: Some("Google AI Pro".to_string()),
        models: model_offers(vec![agy::AvailableModel {
            id: id.to_string(),
            name: name.to_string(),
            thought_levels: levels.iter().map(|level| level.to_string()).collect(),
        }]),
    }))
}

pub(crate) async fn check() -> Arc<Status> {
    let status = Arc::new(probe().await);
    *STATUS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Instant::now(), status.clone()));
    status
}

async fn probe() -> Status {
    let sign_in = agy::check_sign_in(None).await?;
    Ok(Ready {
        #[cfg(feature = "tui")]
        tier: sign_in.tier,
        models: model_offers(sign_in.models),
    })
}

struct ModelOffer {
    name: String,
    efforts: Vec<Effort>,
}

fn model_offers(models: Vec<agy::AvailableModel>) -> BTreeMap<String, ModelOffer> {
    models
        .into_iter()
        .map(|model| {
            let mut efforts: Vec<Effort> = model
                .thought_levels
                .iter()
                .filter_map(|level| level.parse().ok())
                .collect();
            efforts.sort();
            (
                model.id,
                ModelOffer {
                    name: model.name,
                    efforts,
                },
            )
        })
        .collect()
}

#[cfg(feature = "tui")]
pub(crate) fn check_at_setup() -> Result<Arc<Status>, String> {
    // Setup can run outside a runtime, or on the terminal's blocking thread.
    // It keeps the caller's runtime free to drive other sessions.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
        .map(|runtime| runtime.block_on(check()))
}

pub(crate) fn needs() -> Option<String> {
    let status = status();
    match status.as_deref() {
        Some(Ok(_)) => None,
        None | Some(Err(Problem::Missing)) => Some(INSTALL.to_string()),
        Some(Err(Problem::SignedOut)) => Some(SIGN_IN.to_string()),
        Some(Err(Problem::Outdated { .. })) => Some(UPDATE.to_string()),
        Some(Err(problem @ Problem::Failed(_))) => Some(problem_text(problem)),
    }
}

#[cfg(feature = "tui")]
pub(crate) fn status_text(status: &Status) -> String {
    match status {
        Ok(ready) => ready.tier.as_ref().map_or_else(
            || "signed in".to_string(),
            |tier| format!("signed in ({tier})"),
        ),
        Err(Problem::Missing) => "not installed".to_string(),
        Err(Problem::SignedOut) => "not signed in".to_string(),
        Err(Problem::Outdated { found }) => format!("{found}, too old"),
        Err(Problem::Failed(_)) => "couldn't check sign-in".to_string(),
    }
}

pub(crate) fn needs_model(model: &str) -> Option<String> {
    needs().or_else(|| (offers(model) == Some(false)).then(|| MODEL_ACCESS.to_string()))
}

pub(crate) fn default_model(models: &[(&'static str, &'static str)]) -> Option<&'static str> {
    models.iter().find_map(|(full, _)| {
        let id = full.strip_prefix(crate::providers::ANTIGRAVITY)?;
        (offers(id) == Some(true)).then_some(*full)
    })
}

pub(crate) fn efforts(model: &str) -> Vec<Effort> {
    status()
        .and_then(|status| {
            status
                .as_ref()
                .as_ref()
                .ok()?
                .models
                .get(model)
                .map(|offer| offer.efforts.clone())
        })
        .unwrap_or_default()
}

pub(crate) fn offers(model: &str) -> Option<bool> {
    status()?
        .as_ref()
        .as_ref()
        .ok()
        .map(|ready| ready.models.contains_key(model))
}

pub(crate) fn for_agent(
    setup: &AntigravitySettings,
    model: &crate::agents::ModelSettings,
) -> Result<AntigravitySettings, String> {
    if setup.helper_command.is_none() {
        return Err(UNSUPPORTED_HOST.to_string());
    }
    let mut settings = setup.clone();
    let level = crate::providers::resolve_model(&model.model)
        .ok()
        .and_then(|resolved| {
            let no_levels = offers(&resolved.id) == Some(true) && efforts(&resolved.id).is_empty();
            (!no_levels).then(|| crate::providers::fit_effort(&resolved, model.effort))
        });
    settings.thought_level = level.map(|effort| effort.to_string());
    Ok(settings)
}

pub(crate) fn for_session(setup: &mut AntigravitySettings, directory: &Path) {
    setup.profile_root = Some(directory.join("antigravity"));
}

/// The latest counters for the plan, and the groups agents here have used.
#[derive(Clone, Debug, Default)]
pub(crate) struct Usage {
    groups: Arc<[agy::UsageGroup]>,
    used: BTreeSet<String>,
}

impl Usage {
    pub(crate) fn record(&mut self, usage: arko_ai::PlanUsage) {
        if let Some(group) = usage.active_limit {
            self.used.insert(group);
        }
        self.groups = usage.antigravity_groups.into();
    }

    pub(crate) fn for_model(&self, model: &str) -> Option<String> {
        let status = status()?;
        let offer = status.as_ref().as_ref().ok()?.models.get(model)?;
        self.for_model_name(&offer.name)
    }

    fn for_model_name(&self, name: &str) -> Option<String> {
        let group = agy::group_for_model(&self.groups, name)?;
        let windows = windows(group);
        (!windows.is_empty()).then(|| figures(&windows))
    }

    pub(crate) fn plan_status(&self) -> Option<crate::remote::PlanStatus> {
        let active: Vec<_> = self
            .groups
            .iter()
            .filter_map(|group| {
                let windows = self.used.contains(&group.name).then(|| windows(group))?;
                (!windows.is_empty()).then_some((&group.name, windows))
            })
            .collect();
        let plan = crate::providers::plan_name(Some(arko_ai::SubscriptionPlan::Antigravity));
        if active.is_empty() {
            return None;
        }
        let multiple = active.len() > 1;
        let mut text = plan.to_string();
        let mut windows = Vec::new();
        for (at, (name, mut group_windows)) in active.into_iter().enumerate() {
            text.push_str(if at > 0 { " · " } else { " " });
            if multiple {
                text.push_str(name);
                text.push(' ');
            }
            text.push_str(&figures(&group_windows));
            if multiple {
                for window in &mut group_windows {
                    window.label = format!("{name} {}", window.label);
                }
            }
            windows.extend(group_windows);
        }
        Some(crate::remote::PlanStatus {
            plan: plan.to_string(),
            windows,
            credits: None,
            set_aside: 0,
            extra_usage: None,
            text,
        })
    }
}

fn windows(group: &agy::UsageGroup) -> Vec<crate::remote::PlanWindow> {
    group
        .buckets
        .iter()
        .filter_map(|bucket| {
            let label = match bucket.window.as_str() {
                "weekly" => "7 d",
                "5h" => "5 h",
                _ => return None,
            };
            Some(crate::remote::PlanWindow {
                label: label.to_string(),
                used_percent: (1.0 - bucket.remaining_fraction) * 100.0,
                resets_at: chrono::DateTime::parse_from_rfc3339(&bucket.reset_time)
                    .ok()
                    .map(|time| time.timestamp()),
            })
        })
        .collect()
}

fn figures(windows: &[crate::remote::PlanWindow]) -> String {
    windows
        .iter()
        .map(|window| format!("{} {:.0}%", window.label, window.used_percent))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_sign_in_check_keeps_the_reason() {
        let detail = "The CLI closed before opening a session.";
        TEST_STATUS.sync_scope(Arc::new(Err(Problem::Failed(detail.to_string()))), || {
            let needs = needs_model("gemini-4-neon").unwrap();
            assert_eq!(
                crate::providers::needs_text("agy:neon", &needs),
                "Couldn't check Antigravity sign-in. The CLI closed before opening a session."
            );
        });
    }

    #[test]
    fn picker_efforts_come_from_each_advertised_model() {
        let offer = |id: &str, levels: &[&str]| agy::AvailableModel {
            id: id.to_string(),
            name: id.to_string(),
            thought_levels: levels.iter().map(|level| level.to_string()).collect(),
        };
        let models = model_offers(vec![
            offer("gemini-4-neon", &["xhigh", "low", "high"]),
            offer("gemini-4-argon", &["high", "medium"]),
            offer("gpt-oss-120b-medium", &[]),
        ]);
        assert_eq!(
            models["gemini-4-neon"].efforts,
            [Effort::Low, Effort::High, Effort::Xhigh]
        );
        assert_eq!(
            models["gemini-4-argon"].efforts,
            [Effort::Medium, Effort::High]
        );
        assert!(models["gpt-oss-120b-medium"].efforts.is_empty());
        assert!(!models.contains_key("gemini-3.8-flash"));
    }

    fn buckets(name: &str, description: &str, weekly: f64, five: f64) -> agy::UsageGroup {
        agy::UsageGroup {
            name: name.to_string(),
            description: description.to_string(),
            buckets: [("weekly", weekly), ("5h", five)]
                .into_iter()
                .map(|(window, remaining_fraction)| agy::UsageBucket {
                    id: window.to_string(),
                    name: window.to_string(),
                    window: window.to_string(),
                    remaining_fraction,
                    reset_time: "2026-10-01T13:30:00Z".to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn cost_names_the_groups_only_after_more_than_one_was_used() {
        let gemini = buckets(
            "Gemini Models",
            "Models within this group: Gemini 4 Neon, Gemini 4 Hydrogen",
            0.97,
            0.91,
        );
        let claude = buckets(
            "Claude and GPT models",
            "Models within this group: Claude Opus 5.5",
            0.75,
            0.68,
        );
        let event = |active: &agy::UsageGroup, gemini: &agy::UsageGroup| arko_ai::PlanUsage {
            plan: Some(arko_ai::SubscriptionPlan::Antigravity),
            active_limit: Some(active.name.clone()),
            antigravity_groups: vec![gemini.clone(), claude.clone()],
            ..arko_ai::PlanUsage::default()
        };
        let mut usage = Usage::default();
        assert!(usage.plan_status().is_none());
        usage.record(event(&gemini, &gemini));
        let status = usage.plan_status().unwrap();
        assert_eq!(status.text, "Google AI plan 7 d 3%, 5 h 9%");
        assert_eq!(status.windows[0].resets_at, Some(1_790_861_400));
        assert_eq!(
            usage.for_model_name("Claude Opus 5.5").as_deref(),
            Some("7 d 25%, 5 h 32%")
        );
        assert_eq!(
            usage.for_model_name("Gemini 4 Hydrogen").as_deref(),
            Some("7 d 3%, 5 h 9%")
        );
        let updated = buckets(&gemini.name, &gemini.description, 0.83, 0.66);
        usage.record(event(&claude, &updated));
        let status = usage.plan_status().unwrap();
        assert_eq!(
            status.text,
            "Google AI plan Gemini Models 7 d 17%, 5 h 34% · Claude and GPT models 7 d 25%, 5 h 32%"
        );
        let lines = crate::providers::cost_lines(&[], &Default::default(), &[status]);
        assert_eq!(
            lines[1].text,
            "Google AI plan Gemini Models 7 d 17%, 5 h 34% · Claude and GPT models 7 d 25%, 5 h 32%"
        );
    }
}

#[cfg(feature = "tui")]
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub(crate) enum Helper {
    Relay,
    Gate,
    Tripwire,
}

#[cfg(feature = "tui")]
impl Helper {
    pub(crate) fn run(self, path: &Path) -> std::io::Result<()> {
        match self {
            Self::Relay => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(agy::run_relay(path)),
            Self::Gate => agy::run_gate(path),
            Self::Tripwire => agy::run_tripwire(path),
        }
    }
}
