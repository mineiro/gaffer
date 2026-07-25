//! The state of a light, and partial changes to it.
//!
//! gaffer keeps *desired* and *reported* state separate. A command sets the
//! desired state; the reconciler pushes it and records what the hardware says
//! came back. The two drifting apart is normal and temporary — a device that
//! never converges is how we notice it has gone away.
//!
//! This is the daemon-shaped version of the command/reconciliation split that
//! Photon expressed as two sets of setters on one field.

use crate::color::{MAX_KELVIN, MIN_KELVIN, clamp_kelvin};

/// Highest brightness the hardware accepts, as a percentage.
pub const MAX_BRIGHTNESS: u8 = 100;

/// A complete, valid light state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightState {
    pub on: bool,
    /// Brightness percentage, 0..=100.
    pub brightness: u8,
    /// Colour temperature in Kelvin, 2900..=7000.
    pub kelvin: u16,
}

impl Default for LightState {
    /// Matches the defaults the earlier apps opened with, so a light that has
    /// never reported yet reads plausibly rather than as 0% at 0K.
    fn default() -> Self {
        Self { on: false, brightness: 20, kelvin: 4000 }
    }
}

impl LightState {
    /// Force every field into its supported range.
    pub const fn clamped(self) -> Self {
        Self {
            on: self.on,
            brightness: if self.brightness > MAX_BRIGHTNESS {
                MAX_BRIGHTNESS
            } else {
                self.brightness
            },
            kelvin: clamp_kelvin(self.kelvin),
        }
    }
}

/// What a command asks of a light's power.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    On,
    Off,
    Toggle,
}

/// An absolute or relative change to a numeric field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adjust {
    /// Set the field to this value.
    Set(i32),
    /// Add this signed delta to the field's current value.
    By(i32),
}

impl Adjust {
    /// Resolve against the current value, clamped to `min..=max`.
    pub fn apply(self, current: i32, min: i32, max: i32) -> i32 {
        let raw = match self {
            Adjust::Set(value) => value,
            Adjust::By(delta) => current.saturating_add(delta),
        };
        raw.clamp(min, max)
    }
}

/// A partial change to a light's state: only the named fields move.
///
/// Deliberately does *not* imply power. `gaffer set left 42%` changes brightness
/// and nothing else; `gaffer on left 42%` is the one-shot that also powers on.
/// Implicit power-on reads as magic and makes scenes harder to reason about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatePatch {
    pub power: Option<Power>,
    pub brightness: Option<Adjust>,
    pub kelvin: Option<Adjust>,
}

impl StatePatch {
    /// A patch that changes nothing.
    pub const EMPTY: Self = Self { power: None, brightness: None, kelvin: None };

    /// True when the patch would leave any state untouched.
    pub const fn is_empty(&self) -> bool {
        self.power.is_none() && self.brightness.is_none() && self.kelvin.is_none()
    }

    /// Shorthand for a bare power change.
    pub const fn power(power: Power) -> Self {
        Self { power: Some(power), ..Self::EMPTY }
    }

    /// Resolve this patch against a base state, producing a valid new state.
    pub fn apply(&self, base: LightState) -> LightState {
        let mut next = base;

        match self.power {
            Some(Power::On) => next.on = true,
            Some(Power::Off) => next.on = false,
            Some(Power::Toggle) => next.on = !base.on,
            None => {}
        }
        if let Some(adjust) = self.brightness {
            next.brightness =
                adjust.apply(i32::from(base.brightness), 0, i32::from(MAX_BRIGHTNESS)) as u8;
        }
        if let Some(adjust) = self.kelvin {
            next.kelvin =
                adjust.apply(i32::from(base.kelvin), i32::from(MIN_KELVIN), i32::from(MAX_KELVIN))
                    as u16;
        }

        next.clamped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: LightState = LightState { on: false, brightness: 50, kelvin: 4000 };

    #[test]
    fn an_empty_patch_changes_nothing() {
        assert!(StatePatch::EMPTY.is_empty());
        assert_eq!(StatePatch::EMPTY.apply(BASE), BASE);
    }

    #[test]
    fn setting_brightness_leaves_power_alone() {
        let patch = StatePatch { brightness: Some(Adjust::Set(42)), ..StatePatch::EMPTY };
        let next = patch.apply(BASE);
        assert_eq!(next.brightness, 42);
        assert!(!next.on, "set must not implicitly power a light on");
        assert_eq!(next.kelvin, BASE.kelvin);
    }

    #[test]
    fn toggle_reads_the_base_state() {
        assert!(StatePatch::power(Power::Toggle).apply(BASE).on);
        let on = LightState { on: true, ..BASE };
        assert!(!StatePatch::power(Power::Toggle).apply(on).on);
    }

    #[test]
    fn relative_adjustments_accumulate_from_the_base() {
        let patch = StatePatch { brightness: Some(Adjust::By(10)), ..StatePatch::EMPTY };
        assert_eq!(patch.apply(BASE).brightness, 60);

        let patch = StatePatch { brightness: Some(Adjust::By(-10)), ..StatePatch::EMPTY };
        assert_eq!(patch.apply(BASE).brightness, 40);
    }

    #[test]
    fn relative_adjustments_saturate_at_the_bounds() {
        let dim = LightState { brightness: 5, ..BASE };
        let patch = StatePatch { brightness: Some(Adjust::By(-40)), ..StatePatch::EMPTY };
        assert_eq!(patch.apply(dim).brightness, 0, "must clamp, not underflow");

        let bright = LightState { brightness: 95, ..BASE };
        let patch = StatePatch { brightness: Some(Adjust::By(40)), ..StatePatch::EMPTY };
        assert_eq!(patch.apply(bright).brightness, 100);
    }

    #[test]
    fn kelvin_is_clamped_to_the_hardware_range() {
        let patch = StatePatch { kelvin: Some(Adjust::Set(9000)), ..StatePatch::EMPTY };
        assert_eq!(patch.apply(BASE).kelvin, MAX_KELVIN);

        let patch = StatePatch { kelvin: Some(Adjust::Set(1000)), ..StatePatch::EMPTY };
        assert_eq!(patch.apply(BASE).kelvin, MIN_KELVIN);

        let patch = StatePatch { kelvin: Some(Adjust::By(-5000)), ..StatePatch::EMPTY };
        assert_eq!(patch.apply(BASE).kelvin, MIN_KELVIN);
    }

    #[test]
    fn a_full_patch_moves_every_field_at_once() {
        let patch = StatePatch {
            power: Some(Power::On),
            brightness: Some(Adjust::Set(42)),
            kelvin: Some(Adjust::Set(4200)),
        };
        assert_eq!(patch.apply(BASE), LightState { on: true, brightness: 42, kelvin: 4200 });
    }

    #[test]
    fn extreme_deltas_do_not_overflow() {
        let patch = StatePatch { brightness: Some(Adjust::By(i32::MAX)), ..StatePatch::EMPTY };
        assert_eq!(patch.apply(BASE).brightness, 100);
        let patch = StatePatch { kelvin: Some(Adjust::By(i32::MIN)), ..StatePatch::EMPTY };
        assert_eq!(patch.apply(BASE).kelvin, MIN_KELVIN);
    }
}
