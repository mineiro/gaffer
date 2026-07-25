//! Conversions between Kelvin and the Elgato "mired" wire unit.
//!
//! Ported verbatim (third time now: `luz-device.c` → Photon's `ColorTemperature.h`
//! → here) so on-the-wire behaviour stays byte-identical across the rewrites.
//! Elgato exposes colour temperature as mireds — micro reciprocal degrees — in
//! the range 143..=344, mapping to the usable Kelvin range 7000K (cool) down to
//! 2900K (warm). Note the inversion: a *higher* mired is a *warmer* light.
//!
//! The integer rounding is deliberate and load-bearing. Floating point would
//! drift from what the previous implementations sent, and the round-trip tests
//! below pin the exact behaviour.

/// Warmest colour temperature the hardware accepts.
pub const MIN_KELVIN: u16 = 2900;
/// Coolest colour temperature the hardware accepts.
pub const MAX_KELVIN: u16 = 7000;
/// Coolest mired value (corresponds to [`MAX_KELVIN`]).
pub const MIN_MIRED: u16 = 143;
/// Warmest mired value (corresponds to [`MIN_KELVIN`]).
pub const MAX_MIRED: u16 = 344;

/// Clamp an arbitrary Kelvin value into the hardware's supported range.
pub const fn clamp_kelvin(kelvin: u16) -> u16 {
    if kelvin < MIN_KELVIN {
        MIN_KELVIN
    } else if kelvin > MAX_KELVIN {
        MAX_KELVIN
    } else {
        kelvin
    }
}

/// Kelvin (2900..=7000) → Elgato mired (143..=344).
pub fn kelvin_to_mired(kelvin: u16) -> u16 {
    let k = u32::from(clamp_kelvin(kelvin));
    let mired = (1_000_000 + k / 2) / k;
    mired.clamp(u32::from(MIN_MIRED), u32::from(MAX_MIRED)) as u16
}

/// Do two Kelvin values land on the same mired, i.e. the same hardware setting?
///
/// The mired grid is coarser than Kelvin — near the cool end one step spans
/// almost 50K — so 4200K and 4202K are not merely close, they are *identical*
/// on the wire. Callers use this to keep showing what a user asked for instead
/// of the reciprocal-rounding artifact that comes back in the device's echo.
pub fn same_mired(a: u16, b: u16) -> bool {
    kelvin_to_mired(a) == kelvin_to_mired(b)
}

/// Elgato mired (143..=344) → Kelvin (2900..=7000).
pub fn mired_to_kelvin(mired: u16) -> u16 {
    // Guard against a zero from malformed firmware output; the reference
    // implementations did the same before dividing.
    let m = u32::from(mired.max(1));
    let kelvin = (1_000_000 + m / 2) / m;
    kelvin.clamp(u32::from(MIN_KELVIN), u32::from(MAX_KELVIN)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_map_to_the_documented_mireds() {
        // 2900K computes to 345 before clamping, so the clamp is what produces
        // the documented 344. Removing it would send an out-of-range value.
        assert_eq!(kelvin_to_mired(MIN_KELVIN), MAX_MIRED);
        assert_eq!(kelvin_to_mired(MAX_KELVIN), MIN_MIRED);
        // The reverse is not symmetric: the mired grid is coarser than the
        // Kelvin range, so the endpoints land just inside it.
        assert_eq!(mired_to_kelvin(MAX_MIRED), 2907);
        assert_eq!(mired_to_kelvin(MIN_MIRED), 6993);
    }

    #[test]
    fn known_values_match_the_reference_implementations() {
        assert_eq!(kelvin_to_mired(4200), 238);
        assert_eq!(kelvin_to_mired(4000), 250);
        assert_eq!(kelvin_to_mired(5000), 200);
        assert_eq!(mired_to_kelvin(238), 4202);
        assert_eq!(mired_to_kelvin(250), 4000);
        assert_eq!(mired_to_kelvin(200), 5000);
    }

    #[test]
    fn kelvin_values_within_one_mired_step_are_the_same_setting() {
        // The case this exists for: asking for 4200K and having the device echo
        // back 4202K is not a change, it is the same wire value.
        assert!(same_mired(4200, 4202));
        assert!(same_mired(4200, 4200));
        // A genuine move — someone used the Elgato app — must still register.
        assert!(!same_mired(4200, 4400));
        assert!(!same_mired(MIN_KELVIN, MAX_KELVIN));
    }

    #[test]
    fn out_of_range_input_is_clamped_not_wrapped() {
        assert_eq!(kelvin_to_mired(0), MAX_MIRED);
        assert_eq!(kelvin_to_mired(u16::MAX), MIN_MIRED);
        assert_eq!(mired_to_kelvin(0), MAX_KELVIN);
        assert_eq!(mired_to_kelvin(u16::MAX), MIN_KELVIN);
    }

    #[test]
    fn kelvin_round_trip_stays_within_one_mired_step() {
        // A mired step near the warm end is worth ~25K, so exact round-tripping
        // is impossible; assert the error never exceeds one step.
        for kelvin in MIN_KELVIN..=MAX_KELVIN {
            let back = mired_to_kelvin(kelvin_to_mired(kelvin));
            let step = kelvin_step_at(kelvin);
            let drift = i32::from(back).abs_diff(i32::from(kelvin));
            assert!(
                drift <= step,
                "{kelvin}K round-tripped to {back}K (drift {drift} > step {step})"
            );
        }
    }

    #[test]
    fn mired_round_trip_is_exact() {
        // The other direction *is* exact: every mired maps to a distinct Kelvin
        // that maps back to itself. This is the property the daemon relies on
        // when reconciling reported state against desired state.
        for mired in MIN_MIRED..=MAX_MIRED {
            let kelvin = mired_to_kelvin(mired);
            assert_eq!(kelvin_to_mired(kelvin), mired, "mired {mired} did not survive");
        }
    }

    /// Kelvin distance covered by one mired step at the given temperature.
    fn kelvin_step_at(kelvin: u16) -> u32 {
        let m = u32::from(kelvin_to_mired(kelvin));
        let hi = 1_000_000 / m.saturating_sub(1).max(1);
        let lo = 1_000_000 / (m + 1);
        hi - lo
    }
}
