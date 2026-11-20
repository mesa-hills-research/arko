//! A label with a band of light that sweeps across it.

use leptos::prelude::*;

/// The label, which while `active` shows a band of the text color sweeping
/// across it (`.shimmer` in app.css). The label's own glyphs draw the band,
/// so there's one copy of its words, which a reader selects, copies and
/// hears once, and the band is on them wherever the label stands.
#[component]
pub fn Shimmer(
    text: impl Fn() -> String + Send + Sync + 'static,
    active: impl Fn() -> bool + Send + Sync + 'static,
    class: &'static str,
) -> impl IntoView {
    let text = Memo::new(move |_| text());
    view! {
        <span class=format!("shimmer-label {class}") class:shimmer=active>
            <span class="shimmer-text">{move || text.get()}</span>
        </span>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_holds_its_words_once() {
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
                assert_eq!(html.matches(&escaped).count(), 1, "{html}");
                assert!(!html.contains("aria-hidden"), "{html}");
                assert!(html.contains("shimmer-label state-label shimmer"), "{html}");
            }
            active.set(false);
            let html = render();
            assert!(html.contains("shimmer-label state-label"), "{html}");
            assert!(!html.contains("state-label shimmer\""), "{html}");
        });
    }
}
