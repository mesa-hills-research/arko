//! A label with a band of light that stays on its text.

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::wasm_bindgen;

#[wasm_bindgen]
extern "C" {
    /// An element, for its `getAnimations()`, which web-sys has only among
    /// its unstable APIs.
    #[wasm_bindgen(extends = web_sys::Element)]
    type Animated;

    #[wasm_bindgen(method, js_name = getAnimations)]
    fn get_animations(this: &Animated, options: &js_sys::Object) -> js_sys::Array;
}

/// The label, with a window that sweeps across it and, in the window, a copy
/// of the label in the text color that moves back as far, so it stays on the
/// label. Both layers are made again with each label, so they always start
/// together at its width. (Safari on Apple's systems works out each layer's
/// move from the layer's width when it sets the animation up, and not again
/// when the width changes.)
#[component]
pub fn Shimmer(
    text: impl Fn() -> String + Send + Sync + 'static,
    active: impl Fn() -> bool + Send + Sync + 'static,
    class: &'static str,
) -> impl IntoView {
    let text = Memo::new(move |_| text());
    view! {
        <span class=format!("shimmer-label {class}") class:shimmer=active on:animationstart=keep_in_step>
            <span class="shimmer-text">{move || text.get()}</span>
            <For
                each=move || [text.get()]
                key=String::clone
                children=|text| view! {
                    <span class="shine" aria-hidden="true">
                        <span class="shine-text">{text}</span>
                    </span>
                }
            />
        </span>
    }
}

/// Starts every animation of the label's layers at the timeline's origin.
/// The window's move and its copy's cancel only while both are at the same
/// point of the sweep, so whichever of them starts, or starts again, both
/// are put back in step, also with every other shimmer on the page.
fn keep_in_step(event: web_sys::AnimationEvent) {
    let Some(label) = event
        .current_target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
    else {
        return;
    };
    let label: &Animated = label.unchecked_ref();
    let options = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&options, &"subtree".into(), &true.into());
    for animation in label.get_animations(&options).iter() {
        if let Ok(animation) = animation.dyn_into::<web_sys::Animation>() {
            animation.set_start_time(Some(0.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_layers_use_the_current_label() {
        let owner = Owner::new();
        owner.with(|| {
            let text = RwSignal::new("r123 working".to_string());
            let active = RwSignal::new(true);
            let render = || {
                view! { <Shimmer text=move || text.get() active=move || active.get_untracked() class="state-label" /> }
                    .to_html()
            };
            for label in ["r123 working", "Working", "Thought for 2s", "Waiting for r1 & r2…"] {
                text.set(label.to_string());
                let html = render();
                let escaped = label.replace('&', "&amp;");
                assert_eq!(html.matches(&escaped).count(), 2, "{html}");
                assert!(html.contains("aria-hidden=\"true\""), "{html}");
                assert!(!html.contains("data-text"), "{html}");
            }
            active.set(false);
            let html = render();
            assert!(html.contains("shimmer-label state-label"), "{html}");
            assert!(!html.contains("state-label shimmer\""), "{html}");
        });
    }
}
