//! The models a session can switch to, as the model picker offers them:
//! grouped by how they are reached, with what each group needs that isn't
//! there. For any front end (the full-screen view's picker, an app's).

use crate::providers;
pub use crate::providers::ModelGroup;
use crate::remote::Models;

/// The models by the way they are reached, as the picker lists them.
pub fn menu(models: &Models, needs: impl Fn(&str) -> Option<String>) -> Vec<providers::ModelGroup> {
    let auth = match models.openai_auth.as_deref() {
        Some("OpenAI API key") => Some(providers::OpenAiAuth::ApiKey),
        Some(_) => Some(providers::OpenAiAuth::ChatGpt),
        None => None,
    };
    let current = [models.main.as_str(), models.workers.as_str()];
    providers::model_menu(needs, auth, &current)
}

/// The models the picker lets you choose, as `/model` takes them: each
/// one's alias, then its name. What the input box completes.
pub fn names(models: &Models, needs: impl Fn(&str) -> Option<String>) -> Vec<String> {
    menu(models, needs)
        .into_iter()
        .filter(|group| group.needs.is_none())
        .flat_map(|group| group.models)
        .flat_map(|(name, alias)| [alias.to_string(), name])
        .filter(|word| !word.is_empty())
        .collect()
}

/// What the model `name` needs that the session doesn't have: Claude Code
/// installed and signed in, or a provider the session's client reaches
/// (`reachable`, as the session publishes it).
pub fn needs(reachable: &[String], name: &str) -> Option<String> {
    let model = providers::resolve_model(name).ok()?;
    match &model.provider {
        arko_ai::Provider::ClaudeCode => providers::missing_credential(name, None),
        provider
            if reachable
                .iter()
                .any(|key| key == providers::provider_key(provider)) =>
        {
            None
        }
        provider => Some(providers::credential_name(provider).to_string()),
    }
}
