//! A running call's spinner.

use leptos::prelude::*;

/// The spinner, a ring of two arcs half a turn apart that go round inside a
/// still square box (`.spinner` in app.css). The arcs are painted at their
/// angle each frame, so they turn about the ring's centre in any browser.
/// (Turned by the compositor instead, the box went round off centre in
/// Safari on the iPhone.) Balanced so, the arcs keep the ink centred.
#[component]
pub fn Spinner() -> impl IntoView {
    view! { <span class="spinner"></span> }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The box is still and empty, and its ring is drawn by the style.
    #[test]
    fn the_spinner_is_a_still_box() {
        let owner = Owner::new();
        owner.with(|| {
            let html = view! { <Spinner /> }.to_html();
            assert!(html.contains("<span class=\"spinner\"></span>"), "{html}");
            assert!(!html.contains("<svg"), "{html}");
        });
    }

    /// The ring's arcs in app.css are a third of a turn each and half a
    /// turn apart, so their ink is centred at any angle.
    #[test]
    fn the_arcs_are_half_a_turn_apart() {
        let css = include_str!("../style/app.css");
        assert!(
            css.contains("#000 120deg, transparent 0 180deg, #000 0 300deg, transparent 0"),
            "the spinner's arcs changed"
        );
    }
}
