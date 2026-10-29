//! Arko's remote page, in Rust with Leptos.

// Callbacks kept for the page's life have long types, and naming each once
// would only move them.
#![allow(clippy::type_complexity)]

mod app;
mod chrome;
mod composer;
mod copy;
mod images;
mod layout;
mod markdown;
mod questions;
mod scroll;
mod select;
mod sheets;
mod slash;
mod spinner;
mod store;
mod theme_list;
mod transcript;
mod words;

// The protocol, compiled from the server's own file.
#[allow(dead_code)]
#[path = "../../src/remote/protocol.rs"]
mod protocol;

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
    let key = app::take_key();
    spawn_local(app::start(page, key));
    listen(page);
    copy::listen_for_code_copies();
    questions::follow();
    view! {
        <Show
            when=move || page.locked.get().is_none()
            fallback=|| view! { <chrome::Lock /> }
        >
            <chrome::Header />
            <chrome::Banner />
            <chrome::ConnectionLine />
            <transcript::Transcript />
            <composer::Composer />
            <sheets::WorkersSheet />
            <sheets::SettingsSheet />
            <images::ImageViewer />
        </Show>
    }
}

/// The page's listeners: the clock, a phone back from sleep, a new link
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
    // doesn't load the page again: trade its key and connect again.
    on(
        "hashchange",
        Box::new(move |_| {
            if let Some(key) = app::take_key() {
                app::close_stream();
                spawn_local(app::start(page, Some(key)));
            }
        }),
    );
    on(
        "popstate",
        Box::new(move |event| {
            let state = event.unchecked_ref::<web_sys::PopStateEvent>().state();
            let worker = js_sys::Reflect::get(&state, &"worker".into())
                .ok()
                .and_then(|worker| worker.as_string());
            if worker != page.worker.get_untracked() {
                page.worker.set(worker);
            }
        }),
    );
    on(
        "keydown",
        Box::new(move |event| {
            let event = event.unchecked_ref::<web_sys::KeyboardEvent>();
            let typing = event
                .target()
                .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                .is_some_and(|element| {
                    matches!(element.tag_name().as_str(), "TEXTAREA" | "INPUT" | "SELECT")
                });
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
            match event.key().as_str() {
                "Escape" if page.worker.get_untracked().is_some() => {
                    transcript::show_worker(page, None)
                }
                // End: back to the latest, as Latest does.
                "End" if !typing && !page.pinned.get_untracked() => {
                    event.prevent_default();
                    scroll::jump_to_latest();
                }
                _ => {}
            }
        }),
    );
}
