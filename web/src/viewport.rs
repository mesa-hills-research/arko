//! The page's height while a phone's keyboard is up. Safari keeps the
//! layout viewport, and with it `100dvh`, at the whole screen behind its
//! keyboard, and shrinks only the visual viewport, which a scroll can then
//! pan over the empty page under the message box. So while a keyboard is
//! up the page takes the visual viewport's height and place, and the box
//! sits on the keyboard. Where the browser makes the page shorter for its
//! keyboard (`interactive-widget=resizes-content`), the two viewports agree
//! and nothing changes.
//!
//! What sizes itself by the screen's height takes the page's instead
//! (`--page-height` in app.css), so the answer panel and the other boxes
//! over the message box fit the page above the keyboard. The field the
//! keyboard is for stays in sight as the page shrinks, in a box that
//! scrolls, such as the panel's list of questions, and those boxes go back
//! to where they stood once the keyboard goes.

use std::cell::RefCell;

use leptos::prelude::{document, window};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

/// How much shorter than the layout viewport the visual one is with a
/// keyboard up (CSS px), more than a browser's bars take.
const KEYBOARD_PX: f64 = 120.0;

thread_local! {
    /// The boxes around the field with the focus, and where each stood
    /// scrolled as the keyboard came.
    static KEPT: RefCell<Vec<(web_sys::Element, f64)>> = const { RefCell::new(Vec::new()) };
}

/// Whether a keyboard takes the bottom of the screen. A text field has
/// the focus, the page isn't zoomed, and the visual viewport is shorter
/// than the layout viewport (`layout`) by at least [`KEYBOARD_PX`].
pub fn keyboard_up(layout: f64, visual: f64, scale: f64, typing: bool) -> bool {
    typing && (scale - 1.0).abs() < 0.01 && layout - visual >= KEYBOARD_PX
}

/// The field that takes text and has the focus.
fn typing() -> Option<web_sys::Element> {
    document().active_element().filter(|element| {
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

/// A box's scroll offset, unrounded (`Element::scroll_top` rounds it).
fn scroll_top(element: &web_sys::Element) -> f64 {
    js_sys::Reflect::get(element, &JsValue::from_str("scrollTop"))
        .ok()
        .and_then(|top| top.as_f64())
        .unwrap_or(0.0)
}

/// The boxes between `field` and the page's body, and where each stands
/// scrolled. (A box that can't scroll yet may once the page is shorter.)
fn scrolled(field: &web_sys::Element) -> Vec<(web_sys::Element, f64)> {
    let body = document().body().map(web_sys::Element::from);
    let mut boxes = Vec::new();
    let mut at = field.parent_element();
    while let Some(element) = at {
        if Some(&element) == body.as_ref() {
            break;
        }
        let top = scroll_top(&element);
        boxes.push((element.clone(), top));
        at = element.parent_element();
    }
    boxes
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
    let field = typing();
    let was_up = root.class_list().contains("keyboard");
    if keyboard_up(layout, viewport.height(), viewport.scale(), field.is_some()) {
        let height = format!("{}px", viewport.height());
        let resized =
            style.get_property_value("--app-height").ok().as_deref() != Some(height.as_str());
        if !was_up && let Some(field) = &field {
            KEPT.set(scrolled(field));
        }
        let _ = style.set_property("--app-height", &height);
        let _ = style.set_property("--app-top", &format!("{}px", viewport.offset_top()));
        let _ = root.class_list().add_1("keyboard");
        // The page is shorter, so the field may have gone out of its box's
        // sight. (A pan alone moves the whole page with it.)
        if resized && let Some(field) = field {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_block(web_sys::ScrollLogicalPosition::Nearest);
            options.set_inline(web_sys::ScrollLogicalPosition::Nearest);
            field.scroll_into_view_with_scroll_into_view_options(&options);
        }
    } else if was_up {
        let _ = style.remove_property("--app-height");
        let _ = style.remove_property("--app-top");
        let _ = root.class_list().remove_1("keyboard");
        for (element, top) in KEPT.take() {
            if element.is_connected() {
                let _ = js_sys::Reflect::set(
                    &element,
                    &JsValue::from_str("scrollTop"),
                    &JsValue::from_f64(top),
                );
            }
        }
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
    // A focus that leaves a field may be on its way to another, which keeps
    // the keyboard, so the page fits itself once the focus has settled.
    // (Sized for a moment as if the keyboard went, the transcript would lose
    // its place.)
    let on_leave = Closure::<dyn FnMut()>::new(|| {
        let _ = window().set_timeout_with_callback(&Closure::once_into_js(fit).unchecked_into());
    });
    let _ =
        document().add_event_listener_with_callback("focusout", on_leave.as_ref().unchecked_ref());
    // (They live as long as the page.)
    on_change.forget();
    on_leave.forget();
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
