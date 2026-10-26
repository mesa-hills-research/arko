//! The sheets: the workers, and the settings (the theme and view, the
//! session's settings, links for other devices, and what the session is).

use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::app::{STORE, Sheet, get_json, local_get, local_set, on_item, page, post};
use crate::composer::gloo_wait;
use crate::layout::View;
use crate::protocol::{Delivery, ItemData, Kind, Level, LinkData, SettingsData};
use crate::theme_list::THEMES;
use crate::transcript::{WorkerCard, show_worker};
use crate::words;

const THEME: &str = "arko.theme";

/// A dialog that shows while `sheet` is `which`.
#[component]
fn Dialog(which: Sheet, title: &'static str, children: ChildrenFn) -> impl IntoView {
    let page = page();
    let dialog: NodeRef<leptos::html::Dialog> = NodeRef::new();
    Effect::new(move |_| {
        let open = page.sheet.get() == Some(which);
        let Some(dialog) = dialog.get() else { return };
        if open && !dialog.open() {
            let _ = dialog.show_modal();
        } else if !open && dialog.open() {
            dialog.close();
        }
    });
    view! {
        <dialog
            class="sheet"
            node_ref=dialog
            aria-label=title
            on:close=move |_| if page.sheet.get_untracked() == Some(which) { page.sheet.set(None) }
            on:click=move |event| {
                // A tap on the backdrop (the dialog itself, outside its content) closes it.
                if event.target() == event.current_target() {
                    page.sheet.set(None);
                }
            }
        >
            <div class="sheet-head">
                <h2>{title}</h2>
                <button type="button" class="btn ghost small" on:click=move |_| page.sheet.set(None)>"Done"</button>
            </div>
            <div class="sheet-body">
                <Show when=move || page.sheet.get() == Some(which)>{children()}</Show>
            </div>
        </dialog>
    }
}

#[component]
pub fn WorkersSheet() -> impl IntoView {
    let page = page();
    let order = move || {
        page.workers.track();
        STORE.with(|store| {
            let store = store.borrow();
            let list = store.worker_list();
            words::sheet_order(&list)
                .into_iter()
                .map(|summary| summary.id.clone())
                .collect::<Vec<_>>()
        })
    };
    view! {
        <Dialog which=Sheet::Workers title="Workers">
            <div class="pick">
                <button type="button" class="wcard main" on:click=move |_| { page.sheet.set(None); show_worker(page, None); }>
                    <span class="wtitle">"Main agent"</span>
                    <span class="wmeta">{move || if page.worker.get().is_some() { "Back to the main transcript" } else { "Shown now" }}</span>
                </button>
                <For each=order key=|id| id.clone() let:id>
                    <WorkerCard id=id listed=true />
                </For>
            </div>
        </Dialog>
    }
}

// ----- the settings' commands -----

/// A command sent from the settings, waiting for Arko. The status shows a
/// setting once it changed, and the lines are what Arko answered in the
/// transcript (an error for a bad value), shown under the setting.
struct Pending {
    /// Tells it from a command it took the place of.
    id: u64,
    command: String,
    note: &'static str,
    done: Option<Box<dyn Fn(&SettingsData) -> bool>>,
    lines: Vec<String>,
    answered: bool,
    timer: i32,
    wait: i32,
}

thread_local! {
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
    static SENT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How long the status may take to show a change Arko answered for. The
/// server looks for changes in the status every 2 s.
const ANSWER_WAIT_MS: i32 = 3000;

/// What the settings show meanwhile: the setting's key and the value it's
/// turning to.
#[derive(Clone, Copy)]
struct Settings {
    shows: RwSignal<Option<(&'static str, String)>>,
    notes: RwSignal<HashMap<&'static str, (String, bool)>>,
}

impl Settings {
    fn note(&self, key: &'static str, text: String, error: bool) {
        self.notes.update(|notes| {
            if text.is_empty() {
                notes.remove(key);
            } else {
                notes.insert(key, (text, error));
            }
        });
    }

    /// The command `id` is through, if it's still the one waiting.
    fn settle_if(&self, id: u64, ok: bool) {
        if PENDING.with(|pending| {
            pending
                .borrow()
                .as_ref()
                .is_some_and(|change| change.id == id)
        }) {
            self.settle(ok);
        }
    }

    /// The command is through: it worked (`ok`), or Arko said why not.
    fn settle(&self, ok: bool) {
        let Some(change) = PENDING.with(|pending| pending.borrow_mut().take()) else {
            return;
        };
        window().clear_timeout_with_handle(change.timer);
        window().clear_timeout_with_handle(change.wait);
        self.shows.set(None);
        let said = change.lines.join("\n");
        if ok {
            self.note(change.note, said, false);
        } else if said.is_empty() {
            self.note(
                change.note,
                "Arko didn’t change it. The transcript says why.".into(),
                true,
            );
        } else {
            self.note(change.note, said, true);
        }
    }

    /// What Arko says after the settings' command, for the settings.
    fn heard(&self, item: &ItemData) {
        let settings = *self;
        let settle = PENDING.with(|pending| {
            let mut pending = pending.borrow_mut();
            let change = pending.as_mut()?;
            if item.kind == Kind::User
                && item.delivery == Some(Delivery::Command)
                && item.text == change.command
            {
                change.answered = true;
                return None;
            }
            if !change.answered || !matches!(item.kind, Kind::Output | Kind::Notice) {
                return None;
            }
            change.lines.push(item.text.clone());
            if change.done.is_none() {
                // A command that changes no setting (a handover) is through once Arko answers.
                return Some(item.level != Some(Level::Error));
            }
            // An answer, and the status still the same a moment later: it didn't take.
            window().clear_timeout_with_handle(change.wait);
            let id = change.id;
            let later = Closure::once_into_js(move || settings.settle_if(id, false));
            change.wait = window()
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    later.unchecked_ref(),
                    ANSWER_WAIT_MS,
                )
                .unwrap_or(0);
            None
        });
        if let Some(ok) = settle {
            self.settle(ok);
        }
    }

    /// Sends `text` from the settings. `done(settings)` says when the status
    /// shows it took, and `shows` what the settings show meanwhile. It takes
    /// the place of a command still waiting, so the last choice wins.
    fn command(
        &self,
        text: String,
        note: &'static str,
        done: Option<Box<dyn Fn(&SettingsData) -> bool>>,
        shows: Option<(&'static str, String)>,
    ) {
        if let Some(earlier) = PENDING.with(|pending| pending.borrow_mut().take()) {
            window().clear_timeout_with_handle(earlier.timer);
            window().clear_timeout_with_handle(earlier.wait);
        }
        self.note(note, String::new(), false);
        let settings = *self;
        let id = SENT.with(|sent| {
            sent.set(sent.get() + 1);
            sent.get()
        });
        let timeout = Closure::once_into_js(move || settings.settle_if(id, false));
        let timer = window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(timeout.unchecked_ref(), 10_000)
            .unwrap_or(0);
        PENDING.with(|pending| {
            *pending.borrow_mut() = Some(Pending {
                id,
                command: text.clone(),
                note,
                done,
                lines: Vec::new(),
                answered: false,
                timer,
                wait: 0,
            })
        });
        self.shows.set(shows);
        spawn_local(async move {
            if !post("/api/message", serde_json::json!({ "text": text })).await {
                let mine = PENDING.with(|pending| {
                    let mut pending = pending.borrow_mut();
                    pending.take_if(|change| change.id == id)
                });
                let Some(change) = mine else { return };
                window().clear_timeout_with_handle(change.timer);
                settings.shows.set(None);
                let why = page()
                    .error
                    .get_untracked()
                    .unwrap_or_else(|| "Not sent.".into());
                settings.note(note, why, true);
            }
        });
    }
}

/// A switch for a setting. `key` names it in what the settings show.
#[component]
fn Switch(
    name: &'static str,
    #[prop(into)] about: Signal<String>,
    #[prop(into)] on: Signal<bool>,
    #[prop(into, optional)] disabled: Signal<bool>,
    key: &'static str,
    toggle: impl Fn(bool) + 'static,
) -> impl IntoView {
    let settings = expect_context::<Settings>();
    let shown = move || match settings.shows.get() {
        Some((which, value)) if which == key => value == "true",
        _ => on.get(),
    };
    let busy = move || {
        settings
            .shows
            .with(|shows| shows.as_ref().is_some_and(|(which, _)| *which == key))
    };
    view! {
        <div class="setting">
            <div class="setting-text">
                <span class="setting-name">{name}</span>
                <span class="setting-about">{about}</span>
            </div>
            <button type="button" class="switch" role="switch" aria-label=name
                aria-checked=move || shown().to_string()
                aria-busy=move || busy().to_string()
                disabled=move || disabled.get() && !busy()
                on:click=move |_| toggle(!shown())>
                <span></span>
            </button>
        </div>
    }
}

/// A set of options, one chosen.
#[component]
fn Options(
    label: &'static str,
    options: Vec<(String, String, String)>,
    #[prop(into)] chosen: Signal<String>,
    #[prop(into, optional)] disabled: Signal<bool>,
    key: &'static str,
    choose: impl Fn(String) + Clone + 'static,
) -> impl IntoView {
    let settings = expect_context::<Settings>();
    let shown = move || match settings.shows.get() {
        Some((which, value)) if which == key => value,
        _ => chosen.get(),
    };
    view! {
        <div class="options" role="radiogroup" aria-label=label>
            {options.into_iter().map(|(value, name, about)| {
                let choose = choose.clone();
                let mine = value.clone();
                let checked = move || shown() == mine;
                // (A tap on the option shown, which may be one still on its way, changes nothing.)
                view! {
                    <button type="button" class="option" role="radio" aria-checked=move || checked().to_string()
                        disabled=move || disabled.get()
                        on:click=move |_| if shown() != value { choose(value.clone()) }>
                        <span class="name">{name}</span>
                        {(!about.is_empty()).then(|| view! { <span class="about">{about}</span> })}
                    </button>
                }
            }).collect_view()}
        </div>
    }
}

#[component]
fn Note(key: &'static str) -> impl IntoView {
    let settings = expect_context::<Settings>();
    move || {
        settings
            .notes
            .with(|notes| notes.get(key).cloned())
            .map(|(text, error)| {
                view! {
                    <p class="note" class:error=error>{text}</p>
                }
            })
    }
}

/// An inline form that sends `command TEXT` (Arko reads the value and says
/// what's wrong with it).
#[component]
fn Change(
    label: &'static str,
    placeholder: &'static str,
    empty: &'static str,
    note: &'static str,
    send: impl Fn(String) + 'static,
) -> impl IntoView {
    let settings = expect_context::<Settings>();
    let field: NodeRef<leptos::html::Input> = NodeRef::new();
    view! {
        <details class="more">
            <summary>"Change"</summary>
            <form class="inline-form" on:submit=move |event| {
                event.prevent_default();
                let Some(input) = field.get_untracked() else { return };
                let value = input.value().trim().to_string();
                if value.is_empty() {
                    settings.note(note, empty.into(), true);
                    return;
                }
                input.set_value("");
                send(value);
            }>
                <label>{label}<input node_ref=field autocomplete="off" placeholder=placeholder /></label>
                <button class="btn small" type="submit">"Set"</button>
            </form>
        </details>
    }
}

/// A select on a touch screen lets go of the focus once a choice is made.
/// iOS keeps it focused after its picker closes, and its next tap then only
/// takes the focus away, so reopening it took two taps. (With a keyboard, the
/// focus stays for the next choice.)
fn let_go(event: &leptos::ev::Event) {
    let touch = window()
        .match_media("(any-pointer: coarse)")
        .ok()
        .flatten()
        .is_some_and(|list| list.matches());
    if touch {
        let _ = event_target::<web_sys::HtmlElement>(event).blur();
    }
}

/// Applies a theme by its slug (none for the system's pair), and keeps it
/// in this browser.
fn apply_theme(slug: Option<&str>) {
    let Some(root) = document().document_element() else {
        return;
    };
    match slug {
        Some(slug) => {
            if document().get_element_by_id("all-themes").is_none()
                && let Ok(link) = document().create_element("link")
            {
                let _ = link.set_attribute("rel", "stylesheet");
                let _ = link.set_attribute("href", "/all-themes.css");
                let _ = link.set_attribute("id", "all-themes");
                if let Some(head) = document().head() {
                    let _ = head.append_child(&link);
                }
            }
            let _ = root.set_attribute("data-theme", slug);
        }
        None => {
            let _ = root.remove_attribute("data-theme");
        }
    }
    local_set(THEME, slug);
}

#[component]
pub fn SettingsSheet() -> impl IntoView {
    let page = page();
    let settings = Settings {
        shows: RwSignal::new(None),
        notes: RwSignal::new(HashMap::new()),
    };
    provide_context(settings);
    on_item(move |item| settings.heard(item));
    // A command that took shows in the status.
    Effect::new(move |_| {
        page.status.with(|status| {
            let took = PENDING.with(|pending| {
                pending
                    .borrow()
                    .as_ref()
                    .and_then(|change| change.done.as_ref())
                    .is_some_and(|done| done(&status.settings))
            });
            if took {
                settings.settle(true);
            }
        })
    });
    view! {
        <Dialog which=Sheet::Settings title="Settings">
            <SettingsBody />
        </Dialog>
    }
}

#[component]
fn SettingsBody() -> impl IntoView {
    let page = page();
    let settings = expect_context::<Settings>();
    let s = move || page.status.with(|status| status.settings.clone());
    let started = move || page.status.with(|status| status.models.started);
    let has_workers = move || page.status.with(|status| status.models.has_workers);
    let info = move || page.status.with(|status| status.info.clone());
    let theme = local_get(THEME).unwrap_or_default();
    let outside_confirm = RwSignal::new(false);
    let handover_confirm = RwSignal::new(false);
    let whose: NodeRef<leptos::html::Select> = NodeRef::new();
    let links = RwSignal::new(None::<Vec<LinkData>>);
    spawn_local(async move {
        links.set(get_json::<Vec<LinkData>>("/api/links", None).await);
    });
    let cc = move || {
        page.status.with(|status| {
            [&status.models.main, &status.models.workers]
                .iter()
                .any(|model| model.starts_with("cc:"))
        })
    };
    let compacts = move || match settings.shows.get() {
        Some(("compact", value)) => value == "true",
        _ => s().cc_compact != Some(false),
    };
    let option_list = |list: &[(&str, &str, &str)]| {
        list.iter()
            .map(|(key, name, about)| (key.to_string(), name.to_string(), about.to_string()))
            .collect::<Vec<_>>()
    };
    view! {
        <section class="group">
            <h3>"Appearance"</h3>
            // (Not a label around the select, and nothing written to it after a
            // pick: either can make iOS take two taps to open it again.)
            <div class="setting">
                <span class="setting-text"><span class="setting-name" id="theme-name">"Theme"</span></span>
                <select class="select" aria-labelledby="theme-name" on:change=move |event| {
                    let value = event_target_value(&event);
                    apply_theme((!value.is_empty()).then_some(value.as_str()));
                    let_go(&event);
                }>
                    <option value="" selected=theme.is_empty()>"System"</option>
                    {[(false, "Light"), (true, "Dark")].map(|(dark, group)| view! {
                        <optgroup label=group>
                            {THEMES.iter().filter(|(_, _, mode)| *mode == dark).map(|(name, slug, _)| view! {
                                <option value=*slug selected=theme == *slug>{*name}</option>
                            }).collect_view()}
                        </optgroup>
                    })}
                </select>
            </div>
            <Options label="View" key="view"
                options=View::ALL.iter().map(|view| (view.key().to_string(), view.name().to_string(), view.about().to_string())).collect()
                chosen=Signal::derive(move || page.view.get().key().to_string())
                choose=move |key: String| if let Some(view) = View::from_key(&key) { page.set_view(view) } />
        </section>

        <section class="group">
            <h3>"Access"</h3>
            <Switch name="Outside the workspace" key="outside"
                about=Signal::derive(move || if s().outside {
                    "The agents may work anywhere on this computer.".to_string()
                } else {
                    let workspace = info().workspace;
                    format!("The agents stay in {}.", if workspace.is_empty() { "the workspace".into() } else { workspace })
                })
                on=Signal::derive(move || s().outside)
                toggle=move |on| {
                    if on {
                        outside_confirm.set(true);
                    } else {
                        // Turning it off is the safe way, so it asks nothing.
                        settings.command("/outside off".into(), "outside", Some(Box::new(|s| !s.outside)), Some(("outside", "false".into())));
                    }
                } />
            <Show when=move || outside_confirm.get()>
                <div class="confirm">
                    <p>"Let the agents read and change files anywhere this computer lets you? Commands that leave the workspace stop being refused."</p>
                    <div class="confirm-buttons">
                        <button type="button" class="btn" on:click=move |_| outside_confirm.set(false)>"Cancel"</button>
                        <button type="button" class="btn primary" on:click=move |_| {
                            outside_confirm.set(false);
                            settings.command("/outside on".into(), "outside", Some(Box::new(|s| s.outside)), Some(("outside", "true".into())));
                        }>"Allow"</button>
                    </div>
                </div>
            </Show>
            <Note key="outside" />
        </section>

        <section class="group">
            <h3>"Replies"</h3>
            <Switch name="Text replies" key="replies"
                about=Signal::derive(move || format!("{}{}",
                    if s().text_replies { "The agent ends its turn with a plain reply." } else { "The agent ends its turn by calling complete_task." },
                    if started() { " It keeps this for the session." } else { " It changes before the conversation starts." }))
                on=Signal::derive(move || s().text_replies)
                disabled=Signal::derive(started)
                toggle=move |on| settings.command(format!("/text-replies {}", if on { "on" } else { "off" }), "replies",
                    Some(Box::new(move |s| s.text_replies == on)), Some(("replies", on.to_string()))) />
            <Note key="replies" />
        </section>

        <section class="group">
            <h3>"Extra rules"</h3>
            <Options label="Extra rules" key="rules" options=option_list(&words::RULES)
                chosen=Signal::derive(move || s().extra_rules.unwrap_or_else(|| "off".into()))
                disabled=Signal::derive(started)
                choose=move |key: String| {
                    let want = key.clone();
                    settings.command(format!("/extra-rules {key}"), "rules", Some(Box::new(move |s| s.extra_rules.as_deref() == Some(want.as_str()))), Some(("rules", key)));
                } />
            <p class="setting-about">{move || if started() { "The session keeps this." } else { "It changes before the conversation starts." }}</p>
            <Note key="rules" />
        </section>

        <section class="group">
            <h3>"Project instructions"</h3>
            <Options label="Project instructions" key="project" options=option_list(&words::PROJECT)
                chosen=Signal::derive(move || s().project_instructions.unwrap_or_else(|| "one".into()))
                choose=move |key: String| {
                    let want = key.clone();
                    settings.command(format!("/project-instructions {key}"), "project", Some(Box::new(move |s| s.project_instructions.as_deref() == Some(want.as_str()))), Some(("project", key)));
                } />
            <Show when=has_workers>
                <Switch name="Workers get them" key="project-workers"
                    about=Signal::derive(move || if s().worker_project_instructions { "Each worker gets them once, in its first message.".to_string() } else { "Only the main agent gets them.".to_string() })
                    on=Signal::derive(move || s().worker_project_instructions)
                    toggle=move |on| settings.command(format!("/project-instructions workers {}", if on { "on" } else { "off" }), "project",
                        Some(Box::new(move |s| s.worker_project_instructions == on)), Some(("project-workers", on.to_string()))) />
            </Show>
            <p class="setting-about">"The main agent gets them once per conversation, in its first message. A change reaches the conversations and workers that start after it."</p>
            <Note key="project" />
        </section>

        <Show when=move || !s().builtin_skill_list.is_empty()>
            <section class="group">
                <h3>"Skills"</h3>
                <Switch name="Built-in skills" key="skills"
                    about=Signal::derive(move || words::builtin_skills_about(started()))
                    on=Signal::derive(move || s().builtin_skills)
                    toggle=move |on| settings.command(format!("/builtin-skills {}", if on { "on" } else { "off" }), "skills",
                        Some(Box::new(move |s| s.builtin_skills == on)), Some(("skills", on.to_string()))) />
                <For each=move || s().builtin_skill_list key=|skill| skill.name.clone() let:skill>
                    {
                        let name = skill.name.clone();
                        let key: &'static str = Box::leak(format!("skill-{name}").into_boxed_str());
                        let label: &'static str = Box::leak(name.clone().into_boxed_str());
                        let on_now = {
                            let name = name.clone();
                            Signal::derive(move || s().builtin_skill_list.iter().any(|skill| skill.name == name && skill.on))
                        };
                        view! {
                            <Switch name=label key=key about=String::new() on=on_now
                                toggle=move |on| {
                                    let name = name.clone();
                                    let check = name.clone();
                                    settings.command(format!("/builtin-skills {name} {}", if on { "on" } else { "off" }), "skills",
                                        Some(Box::new(move |s| s.builtin_skill_list.iter().any(|skill| skill.name == check && skill.on == on))),
                                        Some((key, on.to_string())));
                                } />
                        }
                    }
                </For>
                <Note key="skills" />
            </section>
        </Show>

        <Show when=has_workers>
            <section class="group">
                <h3>"Workers"</h3>
                <div class="setting"><div class="setting-text">
                    <span class="setting-name">"Models for workers"</span>
                    <span class="setting-about">"Who picks a worker's model. A change applies to the next worker started."</span>
                </div></div>
                <Options label="Models for workers" key="worker-models" options=option_list(&words::WORKER_MODELS)
                    chosen=Signal::derive(move || s().worker_models.unwrap_or_else(|| "agent".into()))
                    choose=move |key: String| {
                        let want = key.clone();
                        settings.command(format!("/worker-models {key}"), "workers", Some(Box::new(move |s| s.worker_models.as_deref() == Some(want.as_str()))), Some(("worker-models", key)));
                    } />
                <div class="setting"><div class="setting-text">
                    <span class="setting-name">"Worker depth"</span>
                    <span class="setting-about">{words::WORKER_DEPTH_ABOUT}</span>
                </div></div>
                <Options label="Worker depth" key="worker-depth"
                    options=(1..=4).map(|level: u32| (level.to_string(), level.to_string(), String::new())).collect()
                    chosen=Signal::derive(move || s().worker_depth.unwrap_or(2).to_string())
                    choose=move |key: String| {
                        let Ok(level) = key.parse::<u32>() else { return };
                        settings.command(format!("/worker-depth {level}"), "workers", Some(Box::new(move |s| s.worker_depth == Some(level))), Some(("worker-depth", key)));
                    } />
                <Switch name="Worker retry cap" key="retry-cap"
                    about=Signal::derive(move || words::retry_cap_about(s().worker_retry_cap))
                    on=Signal::derive(move || s().worker_retry_cap.is_some())
                    toggle=move |on| settings.command(format!("/worker-retry-cap {}", if on { "on" } else { "off" }), "workers",
                        Some(Box::new(move |s| s.worker_retry_cap.is_some() == on)), Some(("retry-cap", on.to_string()))) />
                <Switch name="Message copies" key="copies"
                    about=Signal::derive(move || words::copies_about(s().message_copies).to_string())
                    on=Signal::derive(move || s().message_copies)
                    toggle=move |on| settings.command(format!("/message-copies {}", if on { "on" } else { "off" }), "workers",
                        Some(Box::new(move |s| s.message_copies == on)), Some(("copies", on.to_string()))) />
                <Note key="workers" />
            </section>
        </Show>

        <section class="group">
            <h3>"Handover"</h3>
            <div class="setting">
                <div class="setting-text">
                    <span class="setting-name">"Main agent"</span>
                    <span class="setting-about">"At this size it writes a report and goes on in a fresh conversation."
                        {move || compacts().then_some(" On cc: models, Claude Code compacts the conversation instead.")}</span>
                </div>
                <span class="setting-value">{move || { let at = s().handover_at; if at > 0 { format!("{} tokens", words::tokens(at)) } else { "Not set".into() } }}</span>
            </div>
            <Show when=has_workers>
                <div class="setting">
                    <div class="setting-text">
                        <span class="setting-name">"Workers"</span>
                        <span class="setting-about">"The running ones go by it from their next step. A worker on a model with less room goes by the most its model allows."</span>
                    </div>
                    <span class="setting-value">{move || { let at = s().workers_handover_at; if at > 0 { format!("{} tokens", words::tokens(at)) } else { "Not set".into() } }}</span>
                </div>
            </Show>
            <Show when=cc>
                <Switch name="Claude Code compacts" key="compact"
                    about=Signal::derive(move || format!("{}{}",
                        if compacts() { "At the handover size, Claude Code compacts the conversation of the agents on cc: models." }
                        else { "At the handover size, the agents on cc: models write a report and go on in a fresh conversation." },
                        if started() { " The session keeps this." } else { " It changes before the conversation starts." }))
                    on=Signal::derive(move || s().cc_compact != Some(false))
                    disabled=Signal::derive(started)
                    toggle=move |on| settings.command(format!("/cc-compact {}", if on { "on" } else { "off" }), "handover",
                        Some(Box::new(move |s| s.cc_compact.unwrap_or(true) == on)), Some(("compact", on.to_string()))) />
            </Show>
            <Change label="Tokens" placeholder="250k" empty="Give a size, such as 200k." note="handover"
                send=move |size| {
                    let before = (s().handover_at, s().workers_handover_at);
                    let whose = whose.get_untracked().map(|select| select.value()).unwrap_or_default();
                    let target = if whose.is_empty() { String::new() } else { format!("{whose} ") };
                    settings.command(format!("/handover-at {target}{size}"), "handover",
                        Some(Box::new(move |s| (s.handover_at, s.workers_handover_at) != before)), None);
                } />
            <Show when=has_workers>
                <div class="setting"><span class="setting-text"><span class="setting-name">"Whose size"</span></span>
                    <select class="select" node_ref=whose aria-label="Whose size" on:change=|event| let_go(&event)>
                        <option value="">"Both"</option>
                        <option value="main">"Main agent"</option>
                        <option value="workers">"Workers"</option>
                    </select>
                </div>
            </Show>
            <div class="setting">
                <div class="setting-text"><span class="setting-name">"Hand over now"</span></div>
                <button type="button" class="btn small" on:click=move |_| handover_confirm.set(true)>"Hand over…"</button>
            </div>
            <Show when=move || handover_confirm.get()>
                <div class="confirm">
                    <p>"Hand over now? The agent stops to write its report, then continues from it alone."
                        {move || compacts().then_some(" On cc: models, Claude Code compacts the conversation instead.")}</p>
                    <div class="confirm-buttons">
                        <button type="button" class="btn" on:click=move |_| handover_confirm.set(false)>"Cancel"</button>
                        <button type="button" class="btn primary" on:click=move |_| {
                            handover_confirm.set(false);
                            settings.command("/handover".into(), "handover", None, None);
                        }>"Hand over"</button>
                    </div>
                </div>
            </Show>
            <Note key="handover" />
        </section>

        <section class="group">
            <h3>"Background jobs"</h3>
            <div class="setting">
                <div class="setting-text">
                    <span class="setting-name">"Move to the background after"</span>
                    <span class="setting-about">"A shell command or another long call still running after this long goes on as a job, and the agent goes on with its work."</span>
                </div>
                <span class="setting-value">{move || { let after = s().background_after; if after > 0 { words::seconds_words(after) } else { "Off".into() } }}</span>
            </div>
            <Change label="Time" placeholder="10m or off" empty="Give a time, such as 10m, or off." note="background"
                send=move |time| {
                    let before = s().background_after;
                    settings.command(format!("/background-after {time}"), "background", Some(Box::new(move |s| s.background_after != before)), None);
                } />
            <Note key="background" />
            <div class="setting">
                <div class="setting-text">
                    <span class="setting-name">"Monitor wakes"</span>
                    <span class="setting-about">"How long after your last message a monitor's warning wakes the agent, so it can keep the monitor. Auto follows the prompt cache."</span>
                </div>
                <span class="setting-value">{move || { let s = s(); words::wakes_words(s.monitor_wakes.as_deref(), s.monitor_wakes_within) }}</span>
            </div>
            <Change label="Time" placeholder="2h, auto, always or off" empty="Give a time, such as 2h, or auto, always or off." note="wakes"
                send=move |time| {
                    let before = s().monitor_wakes;
                    settings.command(format!("/monitor-wakes {time}"), "wakes", Some(Box::new(move |s| s.monitor_wakes != before)), None);
                } />
            <Note key="wakes" />
        </section>

        {move || links.get().filter(|links| !links.is_empty()).map(|links| {
            let rest = links[1..].to_vec();
            let count = rest.len();
            view! {
                <section class="group">
                    <h3>"Open on another device"</h3>
                    <LinkCard link=links[0].clone() />
                    {(count > 0).then(|| view! {
                        <details class="more">
                            <summary>{format!("Other addresses ({count})")}</summary>
                            {rest.into_iter().map(|link| view! { <LinkCard link=link /> }).collect_view()}
                        </details>
                    })}
                </section>
            }
        })}

        <section class="group">
            <h3>"Session"</h3>
            <dl class="session-info">
                {move || {
                    let info = info();
                    [("Model", info.model), ("Workspace", info.workspace), ("Session", info.session)]
                        .into_iter()
                        .filter(|(_, value)| !value.is_empty())
                        .map(|(name, value)| view! { <dt>{name}</dt><dd>{value}</dd> })
                        .collect_view()
                }}
            </dl>
            {move || page.status.with(|status| status.workers.clone()).map(|text| view! {
                <details class="more">
                    <summary>"Workers in detail"</summary>
                    <pre class="block">{text}</pre>
                </details>
            })}
        </section>
    }
}

/// A link to the page, with its QR code.
#[component]
fn LinkCard(link: LinkData) -> impl IntoView {
    let kind = match link.kind.as_str() {
        "lan" => "Local network",
        "tailscale" => "Tailscale",
        "vpn" => "VPN",
        "other" => "Other network",
        "loopback" => "This computer only",
        _ => "",
    };
    let qr = link
        .qr_svg
        .as_deref()
        .and_then(safe_svg)
        .unwrap_or_default();
    let copied = RwSignal::new(false);
    let url = link.url.clone();
    view! {
        <div class="link-card">
            <div class="qr" inner_html=qr></div>
            <div class="link-text">
                <div class="link-where">{link.interface.clone().unwrap_or_else(|| "Link".into())}</div>
                <div class="link-kind">{kind}</div>
                <input class="link-url" readonly value=link.url.clone() aria-label="Link"
                    on:focus=move |event| { event_target::<web_sys::HtmlInputElement>(&event).select(); } />
                <button type="button" class="btn small" on:click=move |_| {
                    let clipboard = window().navigator().clipboard();
                    let _ = clipboard.write_text(&url);
                    copied.set(true);
                    spawn_local(async move { gloo_wait(1500).await; copied.set(false); });
                }>{move || if copied.get() { "Copied" } else { "Copy link" }}</button>
            </div>
        </div>
    }
}

/// Arko's QR code, kept only if it is shapes alone: no scripts, handlers,
/// links or styles, whatever the text says.
fn safe_svg(source: &str) -> Option<String> {
    let source = source.trim();
    if !source.starts_with("<svg") {
        return None;
    }
    let lower = source.to_lowercase();
    let mut rest = lower.as_str();
    while let Some(at) = rest.find('<') {
        rest = &rest[at + 1..];
        let name: String = rest
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if !["svg", "path", "rect", "g"].contains(&name.as_str()) && !rest.starts_with('?') {
            return None;
        }
    }
    if lower.contains(" on")
        || lower.contains("href")
        || lower.contains("style")
        || lower.contains("javascript")
    {
        return None;
    }
    Some(source.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_qr_code_with_anything_but_shapes_is_refused() {
        assert!(
            super::safe_svg(
                r#"<svg viewBox="0 0 2 2"><path d="M0 0h1v1H0z"/><rect width="2"/></svg>"#
            )
            .is_some()
        );
        assert!(super::safe_svg(r#"<svg><script>alert(1)</script></svg>"#).is_none());
        assert!(super::safe_svg(r#"<svg><path onload="x"/></svg>"#).is_none());
        assert!(super::safe_svg(r#"<svg><a href="javascript:x"><path/></a></svg>"#).is_none());
    }
}
