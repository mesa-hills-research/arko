//! A running call's spinner.

use leptos::prelude::*;

/// Each arc's sweep, in degrees.
const SWEEP: f64 = 120.0;

/// One arc of the circle of radius 9 about (12, 12) in a 24 by 24 drawing,
/// Lucide's loader circle's, from `start` degrees on.
fn arc(start: f64) -> String {
    let at = |degrees: f64| {
        let radians = degrees.to_radians();
        (12.0 + 9.0 * radians.cos(), 12.0 + 9.0 * radians.sin())
    };
    let (x, y) = at(start);
    let (to_x, to_y) = at(start + SWEEP);
    format!("M{x:.3} {y:.3}A9 9 0 0 1 {to_x:.3} {to_y:.3}")
}

/// The spinner's two arcs, half a turn apart. Balanced so, they keep the
/// center of the ink at the circle's center as they turn. The single arc of
/// Lucide's loader circle swings it about a pixel round it at this size,
/// which reads as the spinner wobbling.
fn arcs() -> String {
    format!("{}{}", arc(0.0), arc(180.0))
}

/// The spinner. It turns inside its own drawing, about the circle's center
/// (`.turn` in app.css), so where the line puts its box doesn't move it.
#[component]
pub fn Spinner() -> impl IntoView {
    view! {
        <svg class="spinner" xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
            <g class="turn"><path d=arcs() /></g>
        </svg>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points every degree along the arc of `sweep` degrees from `start`.
    fn ink(start: f64, sweep: f64) -> Vec<(f64, f64)> {
        (0..=sweep as usize)
            .map(|step| {
                let radians = (start + step as f64).to_radians();
                (12.0 + 9.0 * radians.cos(), 12.0 + 9.0 * radians.sin())
            })
            .collect()
    }

    /// How far, in pixels of the 12 pixel spinner, the points' middle is from the circle's.
    fn off(points: &[(f64, f64)]) -> f64 {
        let count = points.len() as f64;
        let x = points.iter().map(|p| p.0).sum::<f64>() / count;
        let y = points.iter().map(|p| p.1).sum::<f64>() / count;
        (x - 12.0).hypot(y - 12.0) / 2.0
    }

    /// Turned to any angle, the two arcs' ink is centered on the circle.
    #[test]
    fn the_arcs_balance_around_the_center() {
        for turn in (0..360).step_by(15) {
            let turn = turn as f64;
            let mut points = ink(turn, SWEEP);
            points.extend(ink(turn + 180.0, SWEEP));
            assert!(off(&points) < 0.01, "{turn}: {}", off(&points));
        }
    }

    /// Lucide's loader circle, one arc of 288 degrees, is about a pixel off
    /// center at this size, which the pair avoids.
    #[test]
    fn a_single_arc_is_off_center() {
        assert!(off(&ink(0.0, 288.0)) > 0.9);
    }

    /// The path is two arcs of the circle, one from the right and one from
    /// the left, each a third of a turn long.
    #[test]
    fn the_path_has_two_arcs_half_a_turn_apart() {
        assert_eq!(
            arcs(),
            "M21.000 12.000A9 9 0 0 1 7.500 19.794M3.000 12.000A9 9 0 0 1 16.500 4.206"
        );
    }
}
