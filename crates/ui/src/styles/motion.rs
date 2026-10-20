//! Bloo motion tokens, shared so every animated surface moves the same way.
//!
//! Values follow Apple's fluid-interface guidance: critically damped springs for anything
//! that repositions, short ease-out entrances that drift in from where the surface lives,
//! and nothing at all when [`gpui::App::reduce_motion`] is set (GPUI's animation elements
//! already render their end state in that case).

use std::time::Duration;

use gpui::{Animation, SpringConfig, ease_out_quint};

/// Converts Apple's spring parameters into GPUI's physical ones (unit mass).
///
/// `response` is roughly how long the spring takes to reach its target, in seconds.
/// `damping_ratio` 1.0 settles without overshoot; lower values bounce.
pub fn apple_spring(response: f32, damping_ratio: f32) -> SpringConfig {
    let natural_frequency = std::f32::consts::TAU / response;
    SpringConfig::new(
        natural_frequency * natural_frequency,
        2.0 * damping_ratio * natural_frequency,
        1.0,
    )
}

/// The spring for scroll jumps and other repositioning: quick, no overshoot.
pub fn reposition_spring() -> SpringConfig {
    apple_spring(0.32, 1.0)
}

/// How long an entering surface (modal, panel content, toast) takes to arrive.
pub const ENTER_DURATION: Duration = Duration::from_millis(180);

/// How far an entering surface drifts, in pixels, from the side it belongs to.
pub const ENTER_DISTANCE: f32 = 8.0;

/// The animation for surfaces appearing. Pair with [`enter_offset`] and an opacity of `delta`.
pub fn enter_animation() -> Animation {
    Animation::new(ENTER_DURATION).with_easing(ease_out_quint())
}

/// The remaining drift at animation progress `delta` (0.0 at the start, 1.0 when settled).
pub fn enter_offset(delta: f32) -> f32 {
    ENTER_DISTANCE * (1.0 - delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_spring_round_trips_through_canonical_form() {
        let (natural_frequency, damping_ratio) = apple_spring(0.5, 0.8).canonical();
        assert!((natural_frequency - std::f32::consts::TAU / 0.5).abs() < 1e-3);
        assert!((damping_ratio - 0.8).abs() < 1e-4);
    }

    #[test]
    fn enter_offset_reaches_zero() {
        assert_eq!(enter_offset(0.0), ENTER_DISTANCE);
        assert_eq!(enter_offset(1.0), 0.0);
    }
}
