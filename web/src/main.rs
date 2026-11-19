//! Arko's remote page, in Rust with Leptos.

// Callbacks kept for the page's life have long types, and naming each once
// would only move them.
#![allow(clippy::type_complexity)]

mod app;
mod approvals;
mod attach;
mod chrome;
mod composer;
mod contents;
mod copy;
mod files;
mod frame;
mod images;
mod layout;
mod markdown;
mod opened;
mod questions;
mod scroll;
mod select;
mod sheets;
mod shimmer;
mod slash;
mod spinner;
mod store;
#[cfg(test)]
mod testing;
mod theme_list;
mod transcript;
mod words;

// The protocol, compiled from the server's own file.
#[allow(dead_code)]
#[path = "../../src/remote/protocol.rs"]
mod protocol;

// A call's content, as the terminal reads it.
#[allow(dead_code)]
#[path = "../../src/remote/content.rs"]
mod content;

// The Quiet view's rows, as the terminal makes them.
#[allow(dead_code)]
#[path = "../../src/remote/quiet.rs"]
mod quiet;

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::app::Page;

fn main() {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&info.to_string().into());
        // The page can't go on, so it says so rather than look alive (boot.js
        // shows it, with Reload).
        if let Ok(failed) = js_sys::Reflect::get(&js_sys::global(), &"arkoFailed".into())
            && let Some(failed) = failed.dyn_ref::<js_sys::Function>()
        {
            let _ = failed.call2(
                &wasm_bindgen::JsValue::NULL,
                &"The page stopped.".into(),
                &info.to_string().into(),
            );
        }
    }));
    // Apple's systems keep their own scrollbars (see app.css).
    if scroll::apple()
        && let Some(root) = document().document_element()
    {
        let _ = root.set_attribute("class", "apple");
    }
    leptos::mount::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    let page = Page::new();
    // The page opens at the main transcript, whichever view this entry of
    // the history showed before a reload.
    if let Ok(history) = window().history() {
        let _ = history.replace_state(&transcript::history_state(&Default::default()), "");
    }
    let key = app::take_key();
    spawn_local(app::start(page, key));
    listen(page);
    copy::listen_for_code_copies();
    questions::follow();
    // The lock screen hides the page's views rather than take them away, so
    // what they hold (a message on its way, the box's text) goes on through
    // a lock, and nothing that outlasts the lock reads a view that went.
    Effect::new(move |_| {
        let locked = page.locked.get().is_some();
        if let Some(body) = document().body() {
            let _ = body.class_list().toggle_with_force("locked", locked);
        }
    });
    view! {
        <Show when=move || page.locked.get().is_some()>
            <chrome::Lock />
        </Show>
        <chrome::Header />
        <chrome::Banner />
        <chrome::ConnectionLine />
        <transcript::Transcript />
        <composer::Composer />
        <sheets::WorkersSheet />
        <sheets::SettingsSheet />
        <images::ImageViewer />
    }
}

/// The page's listeners, the clock, a phone back from sleep, a new link
/// opened in this tab, the back button, and the keys that move about.
fn listen(page: Page) {
    let window = window();
    let tick = Closure::<dyn FnMut()>::new(move || page.now.set(js_sys::Date::now()));
    let _ = window.set_interval_with_callback_and_timeout_and_arguments_0(
        tick.as_ref().unchecked_ref(),
        1000,
    );
    tick.forget();

    let on = |name: &str, handler: Box<dyn FnMut(web_sys::Event)>| {
        let closure = Closure::<dyn FnMut(web_sys::Event)>::new(handler);
        let _ = window.add_event_listener_with_callback(name, closure.as_ref().unchecked_ref());
        closure.forget();
    };
    // A phone back from sleep or another app checks its stream, and
    // reconnects at once without one.
    let _ = document().add_event_listener_with_callback(
        "visibilitychange",
        {
            let closure = Closure::<dyn FnMut()>::new(move || {
                let visible = document().visibility_state() == web_sys::VisibilityState::Visible;
                if visible && page.locked.get_untracked().is_none() {
                    app::check_stream(page);
                }
            });
            let function = closure.as_ref().unchecked_ref::<js_sys::Function>().clone();
            closure.forget();
            function
        }
        .unchecked_ref(),
    );
    // So does a page the browser kept as it was, back with Back or Forward.
    on(
        "pageshow",
        Box::new(move |event| {
            let kept = event
                .unchecked_ref::<web_sys::PageTransitionEvent>()
                .persisted();
            if kept && page.locked.get_untracked().is_none() {
                app::check_stream(page);
            }
        }),
    );
    // A link that differs only after the # (a new key after Arko restarted)
    // doesn't load the page again, so trade its key and connect again.
    on(
        "hashchange",
        Box::new(move |_| {
            if let Some(key) = app::take_key() {
                app::close_stream();
                spawn_local(app::start(page, Some(key)));
            }
        }),
    );
    // Back and Forward (and the edge swipes of phones) step through the
    // views the reader saw.
    on(
        "popstate",
        Box::new(move |event| {
            let state = event.unchecked_ref::<web_sys::PopStateEvent>().state();
            transcript::show_history_entry(page, &state);
        }),
    );
    on(
        "keydown",
        Box::new(move |event| {
            let event = event.unchecked_ref::<web_sys::KeyboardEvent>();
            let target = event
                .target()
                .and_then(|target| target.dyn_into::<web_sys::Element>().ok());
            let typing = target.as_ref().is_some_and(|element| {
                matches!(element.tag_name().as_str(), "TEXTAREA" | "INPUT" | "SELECT")
            });
            // (The message box, of the fields, lets the keys that scroll
            // the transcript through.)
            let message_box = target
                .as_ref()
                .is_some_and(|element| element.matches(".compose textarea").unwrap_or(false));
            // Alt+Q (Option+Q on a Mac, whose key gives another letter)
            // opens the answer panel, or gives it the keyboard.
            if event.alt_key() && !event.ctrl_key() && !event.meta_key() && event.code() == "KeyQ" {
                event.prevent_default();
                questions::open_panel(None);
                return;
            }
            if page.sheet.get_untracked().is_some() || page.image.get_untracked().is_some() {
                return;
            }
            if event.default_prevented() || event.alt_key() || (typing && !message_box) {
                return;
            }
            match event.key().as_str() {
                "Escape" if page.worker.get_untracked().is_some() => {
                    transcript::show_worker(page, None)
                }
                // Esc follows, where nothing else took it.
                "Escape" => scroll::escape(event.time_stamp()),
                _ => {
                    // The keys that scroll the transcript, which the page
                    // takes so the browser doesn't glide. With the message
                    // box focused, Home and End only while it's empty.
                    let empty = typing
                        && event
                            .target()
                            .and_then(|target| {
                                target.dyn_into::<web_sys::HtmlTextAreaElement>().ok()
                            })
                            .is_some_and(|area| area.value().is_empty());
                    if scroll::key(event, typing, empty) {
                        event.prevent_default();
                    }
                }
            }
        }),
    );
}
