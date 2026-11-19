//! The page's height while a phone's keyboard is up. Safari keeps the
//! layout viewport, and with it `100dvh`, at the whole screen behind its
//! keyboard, and shrinks only the visual viewport, which a scroll can then
//! pan over the empty page under the message box. So while a keyboard is
//! up the page takes the visual viewport's height and place, and the box
//! sits on the keyboard. Where the browser makes the page shorter for its
//! keyboard (`interactive-widget=resizes-content`), the two viewports agree
//! and nothing changes.

use leptos::prelude::{document, window};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

/// How much shorter than the layout viewport the visual one is with a
/// keyboard up (CSS px), more than a browser's bars take.
const KEYBOARD_PX: f64 = 120.0;

/// Whether a keyboard takes the bottom of the screen. A text field has
/// the focus, the page isn't zoomed, and the visual viewport is shorter
/// than the layout viewport (`layout`) by at least [`KEYBOARD_PX`].
pub fn keyboard_up(layout: f64, visual: f64, scale: f64, typing: bool) -> bool {
    typing && (scale - 1.0).abs() < 0.01 && layout - visual >= KEYBOARD_PX
}

/// Whether the focus is in a field that takes text.
fn typing() -> bool {
    document().active_element().is_some_and(|element| {
        element.is_instance_of::<web_sys::HtmlTextAreaElement>()
            || element
                .dyn_ref::<web_sys::HtmlInputElement>()
                .is_some_and(|input| {
                    matches!(
                        input.type_().as_str(),
                        "text" | "search" | "email" | "url" | "tel" | "password" | "number"
                    )
                })
    })
}

/// Sizes the page to the visual viewport while a keyboard is up
/// (`--app-height`, `--app-top` and the root's `keyboard` class), and gives
/// it back once the keyboard goes.
fn fit() {
    let (Some(viewport), Some(root)) = (window().visual_viewport(), document().document_element())
    else {
        return;
    };
    let Ok(root) = root.dyn_into::<web_sys::HtmlElement>() else {
        return;
    };
    // (Safari's inner height may follow the visual viewport, the layout
    // viewport's own height doesn't.)
    let inner = window()
        .inner_height()
        .ok()
        .and_then(|height| height.as_f64())
        .unwrap_or(0.0);
    let layout = inner.max(f64::from(root.client_height()));
    let style = root.style();
    if keyboard_up(layout, viewport.height(), viewport.scale(), typing()) {
        let _ = style.set_property("--app-height", &format!("{}px", viewport.height()));
        let _ = style.set_property("--app-top", &format!("{}px", viewport.offset_top()));
        let _ = root.class_list().add_1("keyboard");
    } else if root.class_list().contains("keyboard") {
        let _ = style.remove_property("--app-height");
        let _ = style.remove_property("--app-top");
        let _ = root.class_list().remove_1("keyboard");
    }
}

/// Follows the visual viewport as it changes and pans, and the focus as it
/// comes to a field and leaves it.
pub fn attach() {
    let Some(viewport) = window().visual_viewport() else {
        return;
    };
    let on_change = Closure::<dyn FnMut()>::new(fit);
    let listen = on_change.as_ref().unchecked_ref();
    let _ = viewport.add_event_listener_with_callback("resize", listen);
    let _ = viewport.add_event_listener_with_callback("scroll", listen);
    let _ = document().add_event_listener_with_callback("focusin", listen);
    let _ = document().add_event_listener_with_callback("focusout", listen);
    // (It lives as long as the page.)
    on_change.forget();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyboard_is_a_shorter_visual_viewport_while_typing_unzoomed() {
        // An iPhone 15 Pro in Safari, before and with its keyboard.
        assert!(!keyboard_up(745.0, 745.0, 1.0, true));
        assert!(keyboard_up(745.0, 409.0, 1.0, true));
        // Its bars as they show and hide, a zoom, no field with the focus.
        assert!(!keyboard_up(745.0, 659.0, 1.0, true));
        assert!(!keyboard_up(745.0, 372.0, 2.0, true));
        assert!(!keyboard_up(745.0, 409.0, 1.0, false));
    }
}
