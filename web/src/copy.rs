//! Copying text, and the copy buttons. A copy button's icon cross-fades to a
//! check, which holds for a second, then fades back.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::composer::gloo_wait;

/// How long a copy button shows its check after a click (its fade in and
/// its hold).
const COPIED_MS: i32 = 120 + 1000;

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

/// Shows the check on a copy button for its time.
fn confirm(copied: RwSignal<bool>) {
    copied.set(true);
    spawn_local(async move {
        gloo_wait(COPIED_MS).await;
        copied.try_set(false);
    });
}

/// The icons of a copy button, Lucide's Copy with its Check over it. They're
/// HTML because a code block's button is in its Markdown's HTML, which
/// lucide-leptos can't write, and every copy button draws them the same.
const ICONS: &str = concat!(
    r#"<span class="copy-icon"><svg class="lucide" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="14" height="14" x="8" y="8" rx="2" ry="2"></rect><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"></path></svg></span>"#,
    r#"<span class="copied-icon"><svg class="lucide" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"></path></svg></span>"#,
);

/// A copy button for `text`, as written. `label` names it for screen
/// readers ("Copy message").
#[component]
pub fn CopyButton(label: &'static str, text: RwSignal<String>) -> impl IntoView {
    let copied = RwSignal::new(false);
    view! {
        <button
            type="button"
            class="btn ghost small icon copy"
            class:copied=move || copied.get()
            aria-label=label
            title="Copy"
            inner_html=ICONS
            on:click=move |_| {
                if !copied.get_untracked() {
                    copy_text(text.get_untracked(), move |done| if done { confirm(copied) });
                }
            }
        ></button>
    }
}

/// The copy button of a code block, as the HTML of a reply's Markdown holds
/// it. [`listen_for_code_copies`] hears it.
pub fn code_button() -> String {
    format!(
        r#"<button type="button" class="btn ghost small icon copy code-copy" aria-label="Copy code" title="Copy">{ICONS}</button>"#
    )
}

/// Copies a code block's code when its copy button is clicked. The blocks
/// are HTML, so one listener on the page hears all their buttons.
pub fn listen_for_code_copies() {
    let listener = Closure::<dyn Fn(web_sys::MouseEvent)>::new(|event: web_sys::MouseEvent| {
        let Some(button) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|target| target.closest(".code-copy").ok().flatten())
        else {
            return;
        };
        let code = button
            .closest(".code")
            .ok()
            .flatten()
            .and_then(|block| block.query_selector("code").ok().flatten())
            .and_then(|code| code.text_content())
            .unwrap_or_default();
        copy_text(code_of(&code).to_string(), move |done| {
            if !done {
                return;
            }
            let _ = button.class_list().add_1("copied");
            spawn_local(async move {
                gloo_wait(COPIED_MS).await;
                let _ = button.class_list().remove_1("copied");
            });
        });
    });
    let _ = document().add_event_listener_with_callback("click", listener.as_ref().unchecked_ref());
    listener.forget();
}

/// The code of a code block's text, which ends in the line end Markdown
/// gives its last line.
fn code_of(text: &str) -> &str {
    text.strip_suffix('\n').unwrap_or(text)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_code_blocks_copy_leaves_out_the_last_line_end_only() {
        assert_eq!(super::code_of("fn main() {}\n"), "fn main() {}");
        assert_eq!(super::code_of("a\n\n"), "a\n");
        assert_eq!(super::code_of("a"), "a");
    }

    #[test]
    fn a_code_blocks_copy_button_has_both_icons() {
        let button = super::code_button();
        assert!(button.contains(r#"aria-label="Copy code""#), "{button}");
        assert!(
            button.contains("copy-icon") && button.contains("copied-icon"),
            "{button}"
        );
        assert_eq!(button.matches("<svg").count(), 2, "{button}");
    }
}
