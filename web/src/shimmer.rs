//! A label with a band of light that stays on its text.

use leptos::prelude::*;

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
            <span class="shine" aria-hidden="true">
                <span class="shine-text">{move || text.get()}</span>
            </span>
        </span>
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
