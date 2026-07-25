//! Collapsing several lights into one displayed state.
//!
//! Elgato firmware has no notion of grouping — controlling several lights as one
//! is entirely client-side, and it is two separate concerns:
//!
//! * **Fan-out** — send the identical command to every member. That lives in the
//!   daemon's reconciler, because it is about I/O.
//! * **Aggregation** — collapse members into one state to display. That is this
//!   module, and it is pure.
//!
//! The rules are inherited from Photon's `LightGroup`: power is on only if
//! *every* member is on, while brightness and temperature are averaged. The
//! asymmetry is deliberate — "all on" is the only reading of group power that
//! makes the toggle behave predictably.

use crate::state::LightState;

/// Collapse member states into a single representative state.
///
/// Returns `None` for an empty slice: a group with no members has no state, as
/// distinct from a group that is off. Callers pass only *online* members — an
/// unreachable light should not drag the average around.
pub fn aggregate(states: &[LightState]) -> Option<LightState> {
    let count = u32::try_from(states.len()).ok()?;
    if count == 0 {
        return None;
    }

    let on = states.iter().all(|state| state.on);
    let brightness = mean(states.iter().map(|state| u32::from(state.brightness)), count);
    let kelvin = mean(states.iter().map(|state| u32::from(state.kelvin)), count);

    Some(LightState { on, brightness: brightness as u8, kelvin: kelvin as u16 }.clamped())
}

/// Arithmetic mean, rounded half-up, in integers.
fn mean(values: impl Iterator<Item = u32>, count: u32) -> u32 {
    let sum: u32 = values.sum();
    (sum + count / 2) / count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light(on: bool, brightness: u8, kelvin: u16) -> LightState {
        LightState { on, brightness, kelvin }
    }

    #[test]
    fn no_members_means_no_state() {
        assert_eq!(aggregate(&[]), None);
    }

    #[test]
    fn a_lone_member_is_reported_verbatim() {
        let only = light(true, 42, 4200);
        assert_eq!(aggregate(&[only]), Some(only));
    }

    #[test]
    fn power_is_the_conjunction_of_members() {
        let on = |a, b| aggregate(&[light(a, 50, 4000), light(b, 50, 4000)]).unwrap().on;

        assert!(on(true, true));
        // One member off is enough to read the group as off. The asymmetry is
        // what makes a group toggle behave predictably from a mixed state.
        assert!(!on(true, false));
        assert!(!on(false, true));
        assert!(!on(false, false));
    }

    #[test]
    fn brightness_and_temperature_are_averaged() {
        let group = aggregate(&[light(true, 40, 4000), light(true, 60, 5000)]).unwrap();
        assert_eq!(group.brightness, 50);
        assert_eq!(group.kelvin, 4500);
    }

    #[test]
    fn averages_round_half_up_rather_than_truncating() {
        let group = aggregate(&[light(true, 40, 4000), light(true, 41, 4001)]).unwrap();
        assert_eq!(group.brightness, 41, "40.5 should round to 41, not 40");
        assert_eq!(group.kelvin, 4001);
    }

    #[test]
    fn a_large_group_does_not_overflow_the_running_sum() {
        let members = vec![light(true, 100, 7000); 1000];
        let group = aggregate(&members).unwrap();
        assert_eq!(group.brightness, 100);
        assert_eq!(group.kelvin, 7000);
    }

    #[test]
    fn the_aggregate_is_always_a_valid_state() {
        let group = aggregate(&[light(true, 100, 7000), light(true, 0, 2900)]).unwrap();
        assert_eq!(group, group.clamped());
    }
}
