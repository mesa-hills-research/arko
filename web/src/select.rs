//! The page's own select: a button that opens its list of choices over the
//! page, below it or above it where there's more room, with a filter for a
//! long list. It opens on the first tap every
//! time, where the browser's own select on iOS stays focused once its
//! picker closes, and the next tap only takes the focus away.

use std::cell::Cell;

use leptos::prelude::*;
use lucide_leptos::{Check, ChevronDown};

/// One choice: what it sets, its name, and the group it's listed under.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub value: String,
    pub name: String,
    pub group: Option<&'static str>,
}

thread_local! {
    /// Numbers the selects, for their options' IDs.
    static MADE: Cell<u32> = const { Cell::new(0) };
}

/// After this long without a key, typing starts a new word to jump to (ms).
const TYPED_FOR: f64 = 700.0;

/// The choices whose name holds `filter`, whatever its case.
fn shown(choices: &[Choice], filter: &str) -> Vec<Choice> {
    let filter = filter.trim().to_lowercase();
    choices
        .iter()
        .filter(|choice| choice.name.to_lowercase().contains(&filter))
        .cloned()
        .collect()
}

/// The choice an arrow, a page or Home and End keys move to from `active`
/// in a list of `count`, or `None` for another key.
fn moved(key: &str, active: Option<usize>, count: usize) -> Option<Option<usize>> {
    if count == 0 {
        return None;
    }
    let last = count - 1;
    Some(Some(match (key, active) {
        ("ArrowDown", None) | ("Home", _) => 0,
        ("ArrowDown", Some(at)) => (at + 1).min(last),
        ("ArrowUp", None) | ("End", _) => last,
        ("ArrowUp", Some(at)) => at.saturating_sub(1),
        ("PageDown", at) => (at.unwrap_or(0) + 8).min(last),
        ("PageUp", at) => at.unwrap_or(0).saturating_sub(8),
        _ => return None,
    }))
}

/// The first choice whose name starts with `typed`.
fn jump(choices: &[Choice], typed: &str) -> Option<usize> {
    let typed = typed.to_lowercase();
    choices
        .iter()
        .position(|choice| choice.name.to_lowercase().starts_with(&typed))
}

/// A select named `label` (for assistive technology), showing `value` and
/// calling `choose` with the value picked. With `filter`, a field above the
/// list narrows it as one types.
#[component]
pub fn Select(
    label: &'static str,
    choices: Vec<Choice>,
    #[prop(into)] value: Signal<String>,
    choose: impl Fn(String) + Clone + Send + Sync + 'static,
    #[prop(optional)] filter: bool,
) -> impl IntoView {
    let id = MADE.with(|made| {
        made.set(made.get() + 1);
        made.get()
    });
    let choices = StoredValue::new(choices);
    let open = RwSignal::new(false);
    let query = RwSignal::new(String::new());
    let active = RwSignal::new(None::<usize>);
    let typed = StoredValue::new((String::new(), 0.0));
    let button: NodeRef<leptos::html::Button> = NodeRef::new();
    let field: NodeRef<leptos::html::Input> = NodeRef::new();
    let list: NodeRef<leptos::html::Ul> = NodeRef::new();
    let pop: NodeRef<leptos::html::Div> = NodeRef::new();
    let visible = Memo::new(move |_| choices.with_value(|choices| shown(choices, &query.get())));
    let name = move || {
        let value = value.get();
        choices.with_value(|choices| {
            choices
                .iter()
                .find(|choice| choice.value == value)
                .map(|choice| choice.name.clone())
                .unwrap_or_default()
        })
    };
    let option_id = move |at: usize| format!("select-{id}-{at}");

    // The active choice stays in view as it moves.
    Effect::new(move |_| {
        let Some(at) = active.get() else { return };
        if !open.get() {
            return;
        }
        if let Some(row) = document().get_element_by_id(&option_id(at)) {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_block(web_sys::ScrollLogicalPosition::Nearest);
            row.scroll_into_view_with_scroll_into_view_options(&options);
        }
    });

    let show =
        move || {
            query.set(String::new());
            let current = value.get_untracked();
            active.set(visible.with_untracked(|visible| {
                visible.iter().position(|choice| choice.value == current)
            }));
            open.set(true);
            // Once drawn, the list takes its place and the keys go to the
            // filter, or to the list.
            let frame = wasm_bindgen::closure::Closure::once_into_js(move || {
                if let (Some(button), Some(pop)) = (button.get_untracked(), pop.get_untracked()) {
                    place(&button, &pop);
                }
                if let Some(field) = field.get_untracked() {
                    let _ = field.focus();
                } else if let Some(list) = list.get_untracked() {
                    let _ = list.focus();
                }
            });
            use wasm_bindgen::JsCast;
            let _ = window().request_animation_frame(frame.unchecked_ref());
        };
    let hide = move |refocus: bool| {
        open.set(false);
        if refocus && let Some(button) = button.get_untracked() {
            let _ = button.focus();
        }
    };
    let pick = {
        let choose = choose.clone();
        move |at: usize| {
            let Some(choice) = visible.with_untracked(|visible| visible.get(at).cloned()) else {
                return;
            };
            hide(true);
            if choice.value != value.get_untracked() {
                choose(choice.value);
            }
        }
    };
    let keys = {
        let pick = pick.clone();
        move |event: web_sys::KeyboardEvent| {
            let key = event.key();
            let count = visible.with_untracked(Vec::len);
            if let Some(next) = moved(&key, active.get_untracked(), count) {
                event.prevent_default();
                active.set(next);
                return;
            }
            match key.as_str() {
                "Enter" => {
                    event.prevent_default();
                    if let Some(at) = active.get_untracked() {
                        pick(at);
                    }
                }
                // (Esc closes the list, not the sheet it's in.)
                "Escape" => {
                    event.prevent_default();
                    event.stop_propagation();
                    hide(true);
                }
                "Tab" => hide(false),
                // Typing jumps to the choice whose name starts with it.
                typed_key
                    if !filter
                        && typed_key.chars().count() == 1
                        && !event.ctrl_key()
                        && !event.meta_key() =>
                {
                    let now = js_sys::Date::now();
                    let word = typed.with_value(|(word, at)| {
                        if now - at < TYPED_FOR {
                            format!("{word}{typed_key}")
                        } else {
                            typed_key.to_string()
                        }
                    });
                    typed.set_value((word.clone(), now));
                    if let Some(at) = visible.with_untracked(|visible| jump(visible, &word)) {
                        active.set(Some(at));
                    }
                }
                _ => {}
            }
        }
    };
    let field_keys = keys.clone();

    view! {
        <div class="select">
        <button
            type="button"
            class="select-button"
            node_ref=button
            aria-label=label
            aria-haspopup="listbox"
            aria-expanded=move || open.get().to_string()
            on:click=move |_| if open.get_untracked() { hide(false) } else { show() }
            on:keydown=move |event: web_sys::KeyboardEvent| {
                if matches!(event.key().as_str(), "ArrowDown" | "ArrowUp") && !open.get_untracked() {
                    event.prevent_default();
                    show();
                }
            }
        >
            <span class="select-value">{name}</span>
            <ChevronDown size=16 />
        </button>
        <Show when=move || open.get()>
            <div class="select-scrim" on:click=move |_| hide(false)></div>
            <div class="select-pop" node_ref=pop>
                {filter.then(|| {
                    let field_keys = field_keys.clone();
                    view! {
                        <input
                            class="select-filter"
                            node_ref=field
                            type="search"
                            placeholder="Filter"
                            aria-label=format!("Filter {}", label.to_lowercase())
                            aria-controls=format!("select-{id}")
                            aria-activedescendant=move || active.get().map(option_id)
                            autocomplete="off"
                            on:input=move |event| {
                                query.set(event_target_value(&event));
                                active.set((visible.with_untracked(Vec::len) > 0).then_some(0));
                            }
                            on:keydown=field_keys
                        />
                    }
                })}
                <ul
                    class="select-list"
                    id=format!("select-{id}")
                    role="listbox"
                    tabindex="-1"
                    aria-label=label
                    aria-activedescendant=move || active.get().map(option_id)
                    node_ref=list
                    on:keydown=keys.clone()
                >
                    {
                        let pick = pick.clone();
                        move || {
                            let filtering = !query.get().trim().is_empty();
                            let current = value.get();
                            let mut group = None;
                            let mut rows = Vec::new();
                            for (at, choice) in visible.get().into_iter().enumerate() {
                                if !filtering && choice.group.is_some() && choice.group != group {
                                    group = choice.group;
                                    rows.push(view! { <li class="select-group" role="presentation">{group}</li> }.into_any());
                                }
                                let picked = choice.value == current;
                                let pick = pick.clone();
                                rows.push(view! {
                                    <li
                                        id=option_id(at)
                                        role="option"
                                        aria-selected=picked.to_string()
                                        class:active=move || active.get() == Some(at)
                                        on:pointermove=move |_| if active.get_untracked() != Some(at) { active.set(Some(at)) }
                                        on:click=move |_| pick(at)
                                    >
                                        <span class="select-check">{picked.then(|| view! { <Check size=16 /> })}</span>
                                        <span>{choice.name}</span>
                                    </li>
                                }.into_any());
                            }
                            if rows.is_empty() {
                                rows.push(view! { <li class="select-empty" role="presentation">"No match"</li> }.into_any());
                            }
                            rows
                        }
                    }
                </ul>
            </div>
        </Show>
        </div>
    }
}

/// Places the list below its button, or above it where there's more room,
/// within what shows of the sheet it's in (or the window), and no taller
/// than that room.
fn place(button: &web_sys::HtmlElement, pop: &web_sys::HtmlElement) {
    const GAP: f64 = 6.0;
    const MARGIN: f64 = 8.0;
    let anchor = button.get_bounding_client_rect();
    let (top, bottom) = match button.closest(".sheet-body").ok().flatten() {
        Some(region) => {
            let region = region.get_bounding_client_rect();
            (region.top(), region.bottom())
        }
        None => (
            0.0,
            window()
                .inner_height()
                .ok()
                .and_then(|height| height.as_f64())
                .unwrap_or(0.0),
        ),
    };
    let below = bottom - anchor.bottom() - GAP - MARGIN;
    let above = anchor.top() - top - GAP - MARGIN;
    let wanted = f64::from(pop.scroll_height());
    let style = web_sys::HtmlElement::style(pop);
    let (side, other, room) = if below >= wanted || below >= above {
        ("top", "bottom", below)
    } else {
        ("bottom", "top", above)
    };
    let _ = style.set_property(side, &format!("calc(100% + {GAP}px)"));
    let _ = style.remove_property(other);
    let _ = style.set_property("max-height", &format!("{}px", room.max(120.0).floor()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices(names: &[&str]) -> Vec<Choice> {
        names
            .iter()
            .map(|name| Choice {
                value: name.to_lowercase(),
                name: name.to_string(),
                group: None,
            })
            .collect()
    }

    #[test]
    fn the_filter_keeps_the_choices_whose_name_holds_it() {
        let list = choices(&["Ayu Dark", "Catppuccin Mocha", "Nord Dark"]);
        let names: Vec<String> = shown(&list, " dark")
            .into_iter()
            .map(|choice| choice.name)
            .collect();
        assert_eq!(names, ["Ayu Dark", "Nord Dark"]);
        assert_eq!(shown(&list, "").len(), 3);
    }

    #[test]
    fn the_arrows_pages_and_ends_move_within_the_list() {
        assert_eq!(moved("ArrowDown", None, 3), Some(Some(0)));
        assert_eq!(moved("ArrowDown", Some(2), 3), Some(Some(2)));
        assert_eq!(moved("ArrowUp", Some(0), 3), Some(Some(0)));
        assert_eq!(moved("ArrowUp", None, 3), Some(Some(2)));
        assert_eq!(moved("End", Some(0), 20), Some(Some(19)));
        assert_eq!(moved("PageDown", Some(15), 20), Some(Some(19)));
        assert_eq!(moved("a", Some(0), 3), None);
        assert_eq!(moved("ArrowDown", None, 0), None);
    }

    #[test]
    fn typing_jumps_to_the_first_choice_that_starts_with_it() {
        let list = choices(&["Ayu Dark", "Ayu Light", "Nord"]);
        assert_eq!(jump(&list, "ayu l"), Some(1));
        assert_eq!(jump(&list, "N"), Some(2));
        assert_eq!(jump(&list, "x"), None);
    }
}
