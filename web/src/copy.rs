//! Copying text.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;

/// Puts `text` on the clipboard, then calls `done` with whether it did.
/// Browsers give the Clipboard API only to pages on HTTPS or on this
/// computer, and the page is often served over plain HTTP on a network, so
/// there it copies from a hidden text field instead.
pub fn copy_text(text: String, done: impl FnOnce(bool) + 'static) {
    let window = window();
    let clipboard = window
        .is_secure_context()
        .then(|| window.navigator().clipboard())
        .filter(|clipboard| !clipboard.is_undefined());
    match clipboard {
        Some(clipboard) => {
            let written = wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&text));
            spawn_local(async move {
                let copied = written.await.is_ok();
                done(copied || copy_from_field(&text));
            });
        }
        // Still in the click, which the old way needs.
        None => done(copy_from_field(&text)),
    }
}

/// Copies `text` the way pages did before the Clipboard API, from a text
/// field off the screen. The focus stays where it was.
fn copy_from_field(text: &str) -> bool {
    let document = document();
    // While a dialog is open, the page outside it can't be selected.
    let place = match document.query_selector("dialog[open]") {
        Ok(Some(dialog)) => Some(dialog),
        _ => document.body().map(Into::into),
    };
    let (Some(place), Ok(field)) = (place, document.create_element("textarea")) else {
        return false;
    };
    let Ok(field) = field.dyn_into::<web_sys::HtmlTextAreaElement>() else {
        return false;
    };
    let focused = document
        .active_element()
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok());
    field.set_class_name("copy-field");
    // Read-only, so a phone shows no keyboard.
    field.set_read_only(true);
    let _ = field.set_attribute("aria-hidden", "true");
    field.set_value(text);
    let _ = place.append_child(&field);
    field.select();
    // (iOS selects only with this.)
    let _ = field.set_selection_range(0, text.encode_utf16().count() as u32);
    let copied = document
        .dyn_ref::<web_sys::HtmlDocument>()
        .and_then(|document| document.exec_command("copy").ok())
        .unwrap_or(false);
    field.remove();
    if let Some(focused) = focused {
        let _ = focused.focus();
    }
    copied
}
