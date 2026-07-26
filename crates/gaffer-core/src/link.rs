//! Ganging lamps so they move as one instrument.
//!
//! Two modes, from the panel sketches:
//!
//! * **Mirror** — every member takes identical values.
//! * **Offset** — members ride together keeping the brightness difference they
//!   had when the link was made, so a key/fill ratio survives.
//!
//! Brightness carries the offset; **colour temperature and power mirror in both
//! modes**. That follows the design: the link editor shows `brt 42% / 35%` in
//! two columns but a single `tmp 4200 K` spanning both, and a temperature-only
//! link is listed as a future variation rather than current behaviour.
//!
//! # The level
//!
//! Offsets are stored per member relative to a notional *level* — the link's
//! position as one fader. A member's brightness is `level + offset`, so moving
//! any member re-derives the level and every other member follows:
//!
//! ```text
//! level  = mover_brightness - offset[mover]
//! each m = clamp(level + offset[m])
//! ```
//!
//! Deriving the level from the mover on every change, rather than accumulating
//! it, is what keeps the link stable. The level is deliberately *not* clamped —
//! only the members are. Pushing one lamp to the ceiling therefore compresses
//! the gang against it and restores the spacing on the way back down, instead
//! of quietly rewriting the offsets or leaving dead travel.

use std::collections::BTreeMap;

use crate::state::{LightState, MAX_BRIGHTNESS};

/// How linked lamps track each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkMode {
    /// Identical values.
    Mirror,
    /// Ride together, keeping the learned brightness difference.
    Offset,
}

impl LinkMode {
    /// The wire's ink in the panel: solid for mirror, dashed for offset.
    pub const fn as_str(self) -> &'static str {
        match self {
            LinkMode::Mirror => "mirror",
            LinkMode::Offset => "offset",
        }
    }
}

/// A gang of lamps that move together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub mode: LinkMode,
    /// Members, each with its brightness offset from the link's level.
    /// Mirror links hold every offset at zero.
    offsets: BTreeMap<String, i32>,
}

impl Link {
    /// Gang these members so they take identical values.
    pub fn mirror<I, S>(members: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            mode: LinkMode::Mirror,
            offsets: members.into_iter().map(|m| (m.into(), 0)).collect(),
        }
    }

    /// Gang these members, learning the brightness differences they have now.
    ///
    /// Offsets are taken relative to the first member in iteration order, so
    /// the level starts out equal to that member's brightness.
    pub fn learn(states: &BTreeMap<String, LightState>) -> Self {
        let reference = states.values().next().map_or(0, |s| i32::from(s.brightness));
        Self {
            mode: LinkMode::Offset,
            offsets: states
                .iter()
                .map(|(id, state)| (id.clone(), i32::from(state.brightness) - reference))
                .collect(),
        }
    }

    /// Rebuild a link from stored parts. The daemon's config layer owns the
    /// serialisation, so that this crate stays dependency-free.
    pub fn from_parts(mode: LinkMode, offsets: BTreeMap<String, i32>) -> Self {
        Self { mode, offsets }
    }

    /// Members and their offsets, for storage.
    pub fn offsets(&self) -> impl Iterator<Item = (&String, i32)> {
        self.offsets.iter().map(|(id, offset)| (id, *offset))
    }

    pub fn members(&self) -> impl Iterator<Item = &String> {
        self.offsets.keys()
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    pub fn contains(&self, member: &str) -> bool {
        self.offsets.contains_key(member)
    }

    /// This member's brightness offset from the link's level.
    pub fn offset_of(&self, member: &str) -> Option<i32> {
        self.offsets.get(member).copied()
    }

    /// Drop a member. The link is meaningless below two, which the caller
    /// should check with [`Link::len`].
    pub fn remove(&mut self, member: &str) {
        self.offsets.remove(member);
    }

    /// What every member becomes when `mover` is set to `next`.
    ///
    /// Returns nothing when `mover` is not a member — a caller applying this
    /// blindly must not silently move a whole gang for an unrelated light.
    pub fn resolve(&self, mover: &str, next: LightState) -> BTreeMap<String, LightState> {
        let Some(mover_offset) = self.offset_of(mover) else {
            return BTreeMap::new();
        };
        let level = i32::from(next.brightness) - mover_offset;

        self.offsets
            .iter()
            .map(|(id, offset)| {
                let brightness = (level + offset).clamp(0, i32::from(MAX_BRIGHTNESS)) as u8;
                // Temperature and power mirror; only brightness carries an offset.
                (id.clone(), LightState { brightness, ..next })
            })
            .collect()
    }

    /// Re-learn one member's offset after it was adjusted on its own.
    ///
    /// This is what alt-drag means: the user moves a single lamp out of step,
    /// and the link adopts the new difference rather than dragging the lamp
    /// back. The level is taken from any *other* member, since that member did
    /// not move.
    ///
    /// A mirror link becomes an offset link, because it now has a difference to
    /// keep — which is also what the panel shows, the wire changing from solid
    /// to dashed.
    pub fn relearn(
        &mut self,
        member: &str,
        brightness: u8,
        others: &BTreeMap<String, LightState>,
    ) -> bool {
        if !self.contains(member) {
            return false;
        }

        // Any unmoved member fixes the level.
        let Some(level) = self
            .offsets
            .iter()
            .filter(|(id, _)| id.as_str() != member)
            .find_map(|(id, offset)| others.get(id).map(|s| i32::from(s.brightness) - offset))
        else {
            return false;
        };

        self.offsets.insert(member.to_string(), i32::from(brightness) - level);
        self.mode = LinkMode::Offset;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(brightness: u8, kelvin: u16) -> LightState {
        LightState { on: true, brightness, kelvin }
    }

    fn states(pairs: &[(&str, u8)]) -> BTreeMap<String, LightState> {
        pairs.iter().map(|(id, b)| ((*id).to_string(), state(*b, 4200))).collect()
    }

    /// The sketch's own example: key left 42%, key right 35%, offset −7.
    fn keys() -> Link {
        Link::learn(&states(&[("left", 42), ("right", 35)]))
    }

    #[test]
    fn learning_captures_the_difference_the_sketch_shows() {
        let link = keys();
        assert_eq!(link.mode, LinkMode::Offset);
        assert_eq!(link.offset_of("left"), Some(0));
        assert_eq!(link.offset_of("right"), Some(-7));
    }

    #[test]
    fn moving_either_lamp_moves_both_and_keeps_the_difference() {
        let link = keys();

        let from_left = link.resolve("left", state(60, 4200));
        assert_eq!(from_left["left"].brightness, 60);
        assert_eq!(from_left["right"].brightness, 53);

        // Symmetric: dragging the follower drags the leader too.
        let from_right = link.resolve("right", state(53, 4200));
        assert_eq!(from_right["left"].brightness, 60);
        assert_eq!(from_right["right"].brightness, 53);
    }

    #[test]
    fn temperature_and_power_mirror_rather_than_offsetting() {
        let link = keys();
        let resolved = link.resolve("left", LightState { on: false, brightness: 50, kelvin: 5600 });

        for member in ["left", "right"] {
            assert_eq!(resolved[member].kelvin, 5600, "{member} should mirror temperature");
            assert!(!resolved[member].on, "{member} should mirror power");
        }
        // …while brightness still differs by the learned offset.
        assert_eq!(resolved["left"].brightness - resolved["right"].brightness, 7);
    }

    #[test]
    fn a_mirror_link_moves_every_member_to_the_same_value() {
        let link = Link::mirror(["left", "right", "mini"]);
        let resolved = link.resolve("mini", state(70, 4200));
        for member in ["left", "right", "mini"] {
            assert_eq!(resolved[member].brightness, 70);
        }
    }

    #[test]
    fn the_gang_compresses_at_the_ceiling_instead_of_losing_the_offset() {
        let link = keys();

        // Pushing the leader to 100 pins the follower there too.
        let at_top = link.resolve("left", state(100, 4200));
        assert_eq!(at_top["left"].brightness, 100);
        assert_eq!(at_top["right"].brightness, 93);

        // Pushing the *follower* to 100 would put the leader past the ceiling,
        // so it clamps — but the stored offset is untouched…
        let over = link.resolve("right", state(100, 4200));
        assert_eq!(over["left"].brightness, 100);
        assert_eq!(over["right"].brightness, 100);
        assert_eq!(link.offset_of("right"), Some(-7), "clamping must not rewrite the link");

        // …so coming back down restores the spacing immediately.
        let back = link.resolve("right", state(80, 4200));
        assert_eq!(back["left"].brightness, 87);
        assert_eq!(back["right"].brightness, 80);
    }

    #[test]
    fn the_gang_compresses_at_the_floor_too() {
        let link = keys();
        let at_bottom = link.resolve("left", state(3, 4200));
        assert_eq!(at_bottom["left"].brightness, 3);
        assert_eq!(at_bottom["right"].brightness, 0);
    }

    #[test]
    fn resolving_is_idempotent_so_a_link_cannot_oscillate() {
        // Feeding a resolved value back in must produce the same result. If it
        // did not, propagation would chase itself between members forever.
        let link = keys();
        let once = link.resolve("left", state(60, 4200));
        let twice = link.resolve("left", once["left"]);
        assert_eq!(once, twice);

        // And re-resolving from the *other* member's resolved value agrees.
        let from_other = link.resolve("right", once["right"]);
        assert_eq!(from_other, once);
    }

    #[test]
    fn a_light_outside_the_link_moves_nothing() {
        assert!(keys().resolve("mini", state(50, 4200)).is_empty());
    }

    #[test]
    fn alt_drag_teaches_the_link_a_new_difference() {
        let mut link = keys();
        // left stays at 42; right is dragged alone to 30.
        assert!(link.relearn("right", 30, &states(&[("left", 42)])));
        assert_eq!(link.offset_of("right"), Some(-12));

        // The pair now rides with the new spacing.
        let resolved = link.resolve("left", state(50, 4200));
        assert_eq!(resolved["right"].brightness, 38);
    }

    #[test]
    fn re_learning_turns_a_mirror_into_an_offset_link() {
        // The panel draws this as the wire changing from solid to dashed.
        let mut link = Link::mirror(["left", "right"]);
        assert!(link.relearn("right", 30, &states(&[("left", 42)])));
        assert_eq!(link.mode, LinkMode::Offset);
        assert_eq!(link.offset_of("right"), Some(-12));
    }

    #[test]
    fn re_learning_needs_an_unmoved_member_to_fix_the_level() {
        let mut link = keys();
        // Nothing known about the other member: refuse rather than guess.
        assert!(!link.relearn("right", 30, &BTreeMap::new()));
        assert_eq!(link.offset_of("right"), Some(-7), "the link must be unchanged");
    }

    #[test]
    fn re_learning_a_light_outside_the_link_is_refused() {
        let mut link = keys();
        assert!(!link.relearn("mini", 30, &states(&[("left", 42)])));
    }

    #[test]
    fn removing_a_member_leaves_the_rest_intact() {
        let mut link = Link::learn(&states(&[("left", 42), ("right", 35), ("mini", 20)]));
        link.remove("right");
        assert_eq!(link.len(), 2);
        assert!(!link.contains("right"));
        assert_eq!(link.offset_of("mini"), Some(-22));
    }

    #[test]
    fn a_three_lamp_gang_keeps_every_spacing() {
        let link = Link::learn(&states(&[("left", 42), ("mini", 20), ("right", 35)]));
        // BTreeMap orders by key, so "left" is the reference at offset 0.
        let resolved = link.resolve("left", state(52, 4200));
        assert_eq!(resolved["left"].brightness, 52);
        assert_eq!(resolved["right"].brightness, 45);
        assert_eq!(resolved["mini"].brightness, 30);
    }
}
