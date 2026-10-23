//! The models a session can switch to, as the model picker offers them.
//! They are grouped by how they are reached, with what each group needs
//! that isn't there, for any front end (the full-screen view's picker, an
//! app's).

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

/// The models the picker lets you choose, as `/model` takes them, with
/// each one's alias, then its name. The input box completes these.
pub fn names(models: &Models, needs: impl Fn(&str) -> Option<String>) -> Vec<String> {
    menu(models, needs)
        .into_iter()
        .filter(|group| group.needs.is_none())
        .flat_map(|group| group.models)
        .flat_map(|(name, alias)| [alias.to_string(), name])
        .filter(|word| !word.is_empty())
        .collect()
}

/// What the picker says beside the model `name` (as `/model` takes it),
/// when there is something to know before choosing it, such as the access
/// Claude Mythos 5.1 needs.
pub fn note(name: &str) -> Option<&'static str> {
    providers::model_note(name)
}

/// What the model `name` needs that the session doesn't have. That is
/// Claude Code installed and signed in, or a provider the session's client
/// reaches (`reachable`, as the session publishes it).
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

/// The models a new session can start on, before it opens. They are
/// grouped as the picker groups them, each group with what it needs that
/// isn't there, with the models a new session takes by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionModels {
    pub groups: Vec<ModelGroup>,
    /// The main agent's model a new session takes by default (the last
    /// session's, else one the credentials allow), or `None` without any
    /// credential.
    pub main: Option<String>,
    /// The workers' model by default, when it isn't the main agent's.
    pub worker: Option<String>,
    /// Why the default isn't the last session's models as they were, when
    /// a credential one of them needs is gone.
    pub note: Option<String>,
}

/// The models a new session in Arko's folder `root` can start on, checked
/// as opening one checks them. That covers the keys (also those in the
/// `.env` files of `workspace`, the session's folder, else the current
/// directory), the ChatGPT login and Claude Code. It may run
/// `claude auth status`, so call it off a UI thread.
pub fn for_new_session(
    root: &std::path::Path,
    workspace: Option<&std::path::Path>,
) -> NewSessionModels {
    let folder = workspace
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok());
    let env = crate::credentials::Env::for_folder_in(folder.as_deref(), true, Some(root));
    crate::context::with_env(env, || models_for_new_session(root))
}

fn models_for_new_session(root: &std::path::Path) -> NewSessionModels {
    let default = providers::default_models(None, root);
    let auth = providers::choose_openai_auth(None, None)
        .ok()
        .map(|(auth, _)| auth);
    let current: Vec<&str> = default
        .iter()
        .flat_map(|default| std::iter::once(default.main.as_str()).chain(default.worker.as_deref()))
        .collect();
    let groups = providers::model_menu(
        |name| providers::missing_credential(name, None),
        auth,
        &current,
    );
    NewSessionModels {
        groups,
        main: default.as_ref().map(|default| default.main.clone()),
        worker: default.as_ref().and_then(|default| default.worker.clone()),
        note: default.and_then(|default| default.reason),
    }
}

/// Why an image of type `mime` can't go to the model `model` (an ID or
/// alias as `/model` takes it, as a session's status names its model) with
/// a message, or `None` when it can. The model may take no images, or not
/// this type. It checks nothing else, so it is cheap, and a front end asks
/// before it sends.
pub fn image_refusal(model: &str, mime: &str) -> Option<String> {
    let model = providers::resolve_model(model).ok()?;
    model.image_refusal(&arko_ai::Image::Base64 {
        data: "".into(),
        mime_type: mime.to_string(),
    })
}

/// Whether a session's 1-hour prompt cache ([`Options::cache_1h`](crate::Options::cache_1h))
/// applies to the model `model` (as `/model` takes it). It does to Claude
/// models on an API key, on OpenRouter or in Claude Code (which otherwise
/// chooses its own, an hour on a Claude plan), and the other providers have
/// none to choose.
pub fn takes_cache_1h(model: &str) -> bool {
    providers::resolve_model(model).is_ok_and(|model| providers::takes_cache_ttl(&model))
}

/// A model's name as people say it ("Opus 5.5", "GPT-6.1 Sol", "Gemini
/// 3.1 Pro Preview"), from an ID or alias as `/model` takes it. `cc:` and
/// `openrouter:` say how a model is reached rather than which it is, so
/// they are left out, as is an OpenRouter slug's maker.
pub fn display_name(model: &str) -> String {
    let id =
        providers::resolve_model(model).map_or_else(|_| model.trim().to_string(), |model| model.id);
    let id = id.rsplit('/').next().unwrap_or(&id);
    let mut words: Vec<&str> = id.split('-').filter(|word| !word.is_empty()).collect();
    // A date at the end (claude-sonnet-4-5-20250929) isn't part of the name.
    if words
        .last()
        .is_some_and(|last| last.len() == 8 && last.chars().all(|c| c.is_ascii_digit()))
    {
        words.pop();
    }
    let capital = |word: &&str| -> String {
        let mut chars = word.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default()
    };
    match words.as_slice() {
        // claude-opus-5-5 → Opus 5.5
        ["claude", family, version @ ..] if !version.is_empty() => {
            format!("{} {}", capital(family), version.join("."))
        }
        // gpt-6.1-sol → GPT-6.1 Sol
        ["gpt", version, rest @ ..] => std::iter::once(format!("GPT-{version}"))
            .chain(rest.iter().map(capital))
            .collect::<Vec<_>>()
            .join(" "),
        words => words.iter().map(capital).collect::<Vec<_>>().join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 1-hour cache applies to Claude models on an API key, on OpenRouter
    /// or in Claude Code, and to no other.
    #[test]
    fn the_1_hour_cache_applies_to_claude_by_api_key_openrouter_or_claude_code() {
        for model in [
            "opus",
            "sonnet",
            "claude-opus-5-5",
            "openrouter:anthropic/claude-opus-5.5",
            "cc:opus",
        ] {
            assert!(takes_cache_1h(model), "{model}");
        }
        for model in [
            "sol",
            "gemini",
            "openrouter:meta/small-model",
            "no such model",
        ] {
            assert!(!takes_cache_1h(model), "{model}");
        }
    }

    /// Models go by the names people say, also by their aliases and on any
    /// way to reach them.
    #[test]
    fn models_have_names_people_say() {
        assert_eq!(display_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(display_name("gpt-6.1-sol"), "GPT-6.1 Sol");
        assert_eq!(
            display_name("gemini-3.1-pro-preview"),
            "Gemini 3.1 Pro Preview"
        );
        assert_eq!(display_name("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(
            display_name("openrouter:meta-llama/llama-4-maverick"),
            "Llama 4 Maverick"
        );
        assert_eq!(
            display_name("openrouter:anthropic/claude-opus-5.5"),
            "Opus 5.5"
        );
        assert_eq!(display_name("luna"), "GPT-6 Luna");
        assert_eq!(display_name("cc:opus"), "Opus 5.5");
    }

    /// A front end learns before it sends which images a model can't take.
    #[test]
    fn images_a_model_cant_take_are_known_before_they_are_sent() {
        assert_eq!(image_refusal("luna", "image/png"), None);
        assert_eq!(image_refusal("opus", "image/jpeg"), None);
        let gif = image_refusal("gemini", "image/gif").expect("Gemini takes no GIF");
        assert!(gif.to_lowercase().contains("gif"), "{gif}");
        assert_eq!(image_refusal("no such model", "image/png"), None);
    }

    /// A new session's models come grouped, before any session opens.
    #[test]
    fn a_new_sessions_models_are_listed_before_it_opens() {
        let root = tempfile::tempdir().unwrap();
        let models = for_new_session(root.path(), Some(root.path()));
        assert!(!models.groups.is_empty());
        assert!(models.groups.iter().all(|group| !group.models.is_empty()));
        // The thread is back in its own context, outside any session.
        assert!(crate::context::env().is_none());
    }
}
