//! Pure light-control logic, shared by the daemon and the CLI.
//!
//! Everything here is total, synchronous and dependency-free. No sockets, no
//! clocks, no configuration files — those belong to `gafferd`. Keeping this
//! boundary strict is what makes the interesting behaviour (unit parsing,
//! relative adjustments, group aggregation, the mired conversion) testable
//! without hardware or a network.
//!
//! The three ideas worth knowing:
//!
//! * [`LightState`] is a complete state; [`StatePatch`] is a partial change to
//!   one. Commands are patches, so `gaffer set left +10%` needs no round-trip
//!   to know what it means — it resolves against whatever the light reports.
//! * [`Selector`] decides which lights a command addresses, and may match more
//!   than one.
//! * [`group::aggregate`] collapses several lights into one state for display,
//!   which is how the "all lights" pseudo-device works.

#![deny(missing_debug_implementations)]

pub mod color;
pub mod group;
pub mod link;
pub mod selector;
pub mod state;
pub mod text;
pub mod units;

pub use color::{MAX_KELVIN, MIN_KELVIN, kelvin_to_mired, mired_to_kelvin};
pub use link::{Link, LinkMode};
pub use selector::Selector;
pub use state::{Adjust, LightState, MAX_BRIGHTNESS, Power, StatePatch};
pub use text::{normalize_id, sanitize};
pub use units::{ParseError, Token, parse_patch, parse_token};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cli_invocation_resolves_end_to_end() {
        // `gaffer set left +10% -200k` against a light reporting 50% at 4200K.
        let reported = LightState { on: true, brightness: 50, kelvin: 4200 };
        let patch = parse_patch(["+10%", "-200k"]).unwrap();
        let desired = patch.apply(reported);

        assert_eq!(desired, LightState { on: true, brightness: 60, kelvin: 4000 });
        assert_eq!(kelvin_to_mired(desired.kelvin), 250);
    }

    #[test]
    fn a_toggle_of_a_two_light_group_uses_the_aggregate() {
        // One light on, one off: the group reads as off, so a toggle turns the
        // pair on rather than flipping each member independently.
        let members = [
            LightState { on: true, ..Default::default() },
            LightState { on: false, ..Default::default() },
        ];
        let group = group::aggregate(&members).unwrap();
        assert!(!group.on);

        let toggled = StatePatch::power(Power::Toggle).apply(group);
        assert!(toggled.on);
    }
}
