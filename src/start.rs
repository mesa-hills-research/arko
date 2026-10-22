//! What a session starts with: a new session's settings, made from its
//! [`Options`], and its client, made once its credentials are there.

use std::path::PathBuf;

use crate::agents::{DEFAULT_WORKER_DEPTH, ExtraRules, ModelSettings, NO_LIMIT, resolve_model};
use crate::credentials::Need;
use crate::options::Options;
use crate::prompter::Prompter;
use crate::providers::Failure;
use crate::remote::Level;
use crate::session::Settings;
use crate::shell::WindowsShell;
use crate::{agents, credentials, handover, providers, session, shell};

/// The working directory the options name, made absolute.
pub(crate) fn workspace(options: &Options) -> Result<PathBuf, String> {
    let path = match &options.workspace {
        Some(path) => path.clone(),
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    };
    let path = std::path::absolute(&path).map_err(|error| error.to_string())?;
    let path = arko_agents::utils::normalize_path(&path);
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    Ok(path)
}

/// Whether a new session's main agent ends its turns with a reply in plain
/// text: as `--text-replies` or `--no-text-replies` says, else when the
/// user takes part in the session as it runs, at the full screen or the
/// line prompt, on the web page or in an app. Not with -p, nor with lines
/// piped in (each runs as a turn of its own): with no one there to say "go
/// on", a reply that wasn't meant to end the work would end the run.
pub(crate) fn text_replies_for(options: &Options) -> bool {
    options
        .text_replies
        .unwrap_or(!options.one_shot && (options.interactive || options.remote.is_some()))
}

/// After how many seconds this run's shell commands move to the background
/// (0: never): as the flag says, else never with -p (Arko exits when the
/// agent finishes, jobs or not), else as the session keeps it (`saved`).
pub(crate) fn background_after_for(options: &Options, saved: u64) -> u64 {
    match options.background_after {
        Some(after) => after,
        None if options.one_shot => 0,
        None => saved,
    }
}

/// Which agents of a new session get the extra rules for small models: as
/// `--extra-rules` (all) or `--no-extra-rules` (none) says, else those on
/// small models.
pub(crate) fn extra_rules_for(options: &Options) -> ExtraRules {
    options.extra_rules.unwrap_or(ExtraRules::Small)
}

/// The project's instructions as the options choose them, over `settings`'
/// own: a new session's defaults (one file, workers get it), or a resumed
/// session's. Returns whether they changed.
pub(crate) fn project_from_options(options: &Options, settings: &mut Settings) -> bool {
    let before = (
        settings.project_instructions,
        settings.worker_project_instructions,
    );
    if let Some(files) = options.project_instructions {
        settings.project_instructions = files;
    }
    if let Some(workers) = options.worker_project_instructions {
        settings.worker_project_instructions = workers;
    }
    (
        settings.project_instructions,
        settings.worker_project_instructions,
    ) != before
}

/// The session's client, made by `make`: credentials it lacks are asked for
/// through `prompter`, and it is made again after each answer.
pub(crate) async fn client_asking(
    prompter: &dyn Prompter,
    mut make: impl FnMut() -> Result<arko_ai::LLMClient, Failure>,
) -> Result<arko_ai::LLMClient, String> {
    loop {
        match make() {
            Ok(client) => return Ok(client),
            Err(Failure::Need(need)) => prompter.need_credentials(&need).await?,
            Err(Failure::Other(message)) => return Err(message),
        }
    }
}

/// A new session's settings, as `options` choose them, and why it doesn't run
/// the last session's models, if Arko chose others.
pub(crate) fn new_settings(
    options: &Options,
    prompter: &dyn Prompter,
) -> Result<(Settings, Option<String>), String> {
    let workspace = workspace(options)?;
    let name = |name: &str| Ok::<_, String>(providers::model_name(&resolve_model(name)?));
    let default = match &options.model {
        Some(_) => None,
        None => Some(default_model(options).ok_or_else(|| Need::Any.message())?),
    };
    let main_model = match (&options.model, &default) {
        (Some(model), _) => name(model)?,
        (None, Some(default)) => name(&default.main)?,
        (None, None) => unreachable!("a default without --model"),
    };
    // The 1-hour cache is Anthropic's, by API key or on OpenRouter; other
    // models ignore it.
    let want_1h = match options.cache_1h {
        Some(on) => on,
        None => credentials::switch(credentials::CACHE_1H)?.unwrap_or(false),
    };
    let cache_1h = |id: &str| {
        want_1h && resolve_model(id).is_ok_and(|model| providers::takes_cache_ttl(&model))
    };
    // The last session's workers' model goes with its main model.
    let worker_model = match &options.worker_model {
        Some(model) => Some(name(model)?),
        None => default
            .as_ref()
            .and_then(|default| default.worker.as_deref())
            .map(name)
            .transpose()?,
    };
    // OpenAI models are all reached one way: the session keeps it.
    let uses_openai = std::iter::once(&main_model)
        .chain(&worker_model)
        .any(|id| resolve_model(id).is_ok_and(|model| model.provider == arko_ai::Provider::OpenAI));
    let openai_auth = if uses_openai {
        let (auth, both) =
            providers::choose_openai_auth(options.openai_auth, options.codex_auth.as_deref())?;
        if both {
            prompter.note(
                Level::Info,
                "this session reaches OpenAI models over your ChatGPT login (your plan), which comes before OPENAI_API_KEY. --openai-auth api-key (or ARKO_OPENAI_AUTH=api-key in a .env file) starts sessions on the API key instead, billed per token",
            );
        }
        Some(auth)
    } else {
        if options.openai_auth.is_some() {
            prompter.note(
                Level::Warn,
                "--openai-auth is for OpenAI models, and this session has none",
            );
        }
        None
    };
    // On the ChatGPT login, OpenAI models hand over below its backend's
    // input limit. A size given is held under what the model leaves room
    // for, as /handover-at holds it.
    let handover_for = |id: &str, given: Option<usize>| {
        let model = resolve_model(id).ok();
        match given {
            Some(tokens) => Some(
                model
                    .as_ref()
                    .and_then(|model| agents::max_handover(model, openai_auth))
                    .map_or(tokens, |most| tokens.min(most)),
            ),
            None => model.map(|model| agents::default_handover(&model, openai_auth)),
        }
    };
    let handover_at = |id: &str| handover_for(id, options.handover_at);
    // The workers' size, when it is their own.
    let workers_given = options.workers_handover_at.or(options.handover_at);
    let main = ModelSettings {
        cache_1h: cache_1h(&main_model),
        handover_at: handover_at(&main_model),
        model: main_model,
        effort: options.effort,
        fast: options.fast,
    };
    // A workers' model of their own (`--worker-model`, or the last
    // session's) doesn't follow the main agent's.
    let own_worker_model = worker_model.is_some();
    let worker = match worker_model {
        Some(model) => ModelSettings {
            cache_1h: cache_1h(&model),
            handover_at: handover_for(&model, workers_given),
            model,
            effort: options.worker_effort.unwrap_or(options.effort),
            fast: options.worker_fast,
        },
        None => ModelSettings {
            effort: options.worker_effort.unwrap_or(main.effort),
            handover_at: match options.workers_handover_at {
                Some(_) => handover_for(&main.model, workers_given),
                None => main.handover_at,
            },
            ..main.clone()
        },
    };
    // A size given that a model holds lower is said.
    let mut said = Vec::new();
    for (model, given) in [(&main, options.handover_at), (&worker, workers_given)] {
        if let Some(tokens) = given
            && let Some(at) = model.handover_at.filter(|at| *at < tokens)
            && !said.contains(&model.model)
        {
            prompter.note(
                Level::Info,
                &format!(
                    "{} hands over at {}, the largest size Arko allows for its model",
                    model.model,
                    handover::shown(at)
                ),
            );
            said.push(model.model.clone());
        }
    }
    // What sessions read before the handover was set per model.
    let session_handover = main.handover_at.unwrap_or(200_000);
    if options.cache_1h == Some(true) && !main.cache_1h && !worker.cache_1h {
        prompter.note(
            Level::Warn,
            "--cache-1h is for Claude models by API key or on OpenRouter (Claude Code chooses its own). This session has none, so it is ignored",
        );
    }
    let mut settings = Settings {
        text_replies: text_replies_for(options),
        cc_handover: options.cc_compact == Some(false),
        extra_rules: extra_rules_for(options),
        project_instructions: Default::default(),
        worker_project_instructions: true,
        workspace,
        main,
        worker,
        max_workers: options.workers.unwrap_or(NO_LIMIT),
        handover_at: session_handover,
        main_handover_at: None,
        workers_handover_at: options.workers_handover_at,
        allow_outside_workspace: options.allow_outside_workspace,
        show_thinking: options.show_thinking,
        hide_thinking: options.hide_thinking,
        created: chrono::Utc::now(),
        windows_shell: Some(windows_shell(options)?),
        title: options.title.clone(),
        mode: options.mode,
        chat_instructions: options.chat_instructions.clone(),
        switched_mode: false,
        branched_from: None,
        openai_auth,
        own_worker_model,
        background_after: background_after_for(options, shell::DEFAULT_BACKGROUND_AFTER),
        monitor_wakes: options.monitor_wakes.unwrap_or_default(),
        worker_models: options.worker_models.unwrap_or_default(),
        worker_depth: match options.worker_depth {
            Some(depth) => crate::workers::check_depth(depth)?,
            None => DEFAULT_WORKER_DEPTH,
        },
        nested_start: true,
        builtin_skills: Default::default(),
    };
    project_from_options(options, &mut settings);
    if settings.mode == crate::session::Mode::Chat {
        // A chat has no tools, workers or project, and the user reads its
        // replies.
        settings.max_workers = 0;
        settings.text_replies = true;
        settings.project_instructions = crate::project::Files::Off;
        settings.worker_project_instructions = false;
    }
    providers::check_openai_choices(&settings)?;
    // The model Arko chose: the last session's, else one the credentials
    // allow.
    let mut model_note = None;
    if let Some(default) = default {
        let mut line = format!("This session runs {}.", settings.describe(&settings.main));
        if let Some(reason) = &default.reason {
            line = format!("{line} {reason}");
        }
        prompter.note(Level::Info, &line);
        model_note = default.reason;
    }
    Ok((settings, model_note))
}

/// The shell a new session runs commands in on Windows: --shell, else
/// ARKO_SHELL, else cmd.exe.
pub(crate) fn windows_shell(options: &Options) -> Result<WindowsShell, String> {
    if let Some(shell) = options.shell {
        return Ok(shell);
    }
    match credentials::var(credentials::SHELL) {
        None => Ok(WindowsShell::Cmd),
        Some(value) => value
            .trim()
            .parse::<WindowsShell>()
            .map_err(|_| format!("ARKO_SHELL is `{value}`. Set it to cmd or powershell")),
    }
}

/// The models of a new session without --model; `None` without
/// credentials.
pub(crate) fn default_model(options: &Options) -> Option<providers::DefaultModels> {
    let root = match &options.root {
        Some(root) => root.clone(),
        None => session::data_root().ok()?,
    };
    providers::default_models(options.codex_auth.as_deref(), &root)
}
