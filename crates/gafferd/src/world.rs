//! The daemon's authoritative picture of every known light.
//!
//! Two states per light, deliberately kept apart:
//!
//! * `desired` — what gaffer is trying to make true. Clients read this, so a
//!   slider drag or a keybind feels instant rather than waiting on a round trip.
//! * `reported` — what the hardware last said. `None` means unreachable; that
//!   *is* the definition of offline, rather than a separate flag that can drift
//!   out of sync with reality.
//!
//! The two converging is the reconciler's job. Them being briefly apart is
//! normal; them never converging is how a vanished light is noticed.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Instant;

use gaffer_core::{Adjust, LightState, Power, Selector, StatePatch, group};

/// Stable identity of a light — the `id=` field from its mDNS TXT record.
pub type LightId = String;

/// Where a light's HTTP API lives.
///
/// Deliberately holds a parsed [`IpAddr`] and never a hostname. An earlier
/// version fell back to formatting the mDNS SRV target into the URL when no
/// address had resolved, and that was exploitable: mDNS never validates the
/// characters in a received name, so an advertisement with the target
/// `127.0.0.1:11434/api/pull#` produced
/// `http://127.0.0.1:11434/api/pull#:9123/elgato/lights`, which parses as host
/// `127.0.0.1`, port `11434`, path `/api/pull`. One multicast packet could
/// therefore aim the daemon's requests at an arbitrary path on a loopback
/// service the attacker could not otherwise reach.
///
/// An `IpAddr` cannot express a path, a query or a userinfo section, so the
/// whole class is gone rather than filtered — which matters, because the
/// obvious filter (requiring a `.local.` suffix) does not work: `#` and `?`
/// terminate the authority before any suffix is examined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub addr: IpAddr,
    pub port: u16,
}

impl Endpoint {
    /// Base URL for this light's HTTP API.
    ///
    /// Every component is structurally constrained: a parsed address and a
    /// `u16`. Nothing here originates as an untrusted string.
    pub fn base_url(&self) -> String {
        match self.addr {
            // IPv6 literals need brackets, or the colons read as a port.
            IpAddr::V6(addr) => format!("http://[{addr}]:{}", self.port),
            IpAddr::V4(addr) => format!("http://{addr}:{}", self.port),
        }
    }
}

/// Everything gaffer knows about one light.
#[derive(Clone, Debug)]
pub struct LightRecord {
    pub id: LightId,
    /// mDNS instance fullname, kept so discovery can actively re-probe it.
    pub fullname: String,
    pub name: String,
    pub model: String,
    pub firmware: String,
    pub endpoint: Endpoint,
    pub desired: LightState,
    pub reported: Option<LightState>,
    pub last_error: String,
    /// Set when `desired` has moved and has not yet been pushed to hardware.
    pub dirty_since: Option<Instant>,
}

impl LightRecord {
    /// A light is online exactly when it has told us something recently.
    pub fn online(&self) -> bool {
        self.reported.is_some()
    }

    /// Point `desired` at `next`, returning what clients would see change, or
    /// `None` when no push is owed at all.
    ///
    /// A push is owed whenever the hardware does not already match — not merely
    /// when the desired value changed. That way re-issuing the same command
    /// after someone used the Elgato app corrects the drift instead of being
    /// silently swallowed as a no-op. The returned [`Changed`] can still be
    /// empty in that case: a corrective push nobody can observe emits no signal.
    fn set_desired(&mut self, next: LightState, now: Instant) -> Option<Changed> {
        let owed = self.desired != next || self.reported != Some(next);
        if !owed {
            return None;
        }

        let before = self.desired;
        self.desired = next;
        if self.dirty_since.is_none() {
            self.dirty_since = Some(now);
        }

        Some(Changed {
            on: before.on != next.on,
            brightness: before.brightness != next.brightness,
            kelvin: before.kelvin != next.kelvin,
            ..Changed::default()
        })
    }
}

/// Which lights a mutation addresses.
#[derive(Clone, Debug)]
pub enum Target {
    /// One specific light, by id. Used by the per-light D-Bus objects.
    Id(LightId),
    /// Whatever a selector matches.
    Select(Selector),
}

/// Which client-visible properties of a light changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changed {
    pub meta: bool,
    pub online: bool,
    pub on: bool,
    pub brightness: bool,
    pub kelvin: bool,
    pub error: bool,
}

impl Changed {
    pub const fn any(self) -> bool {
        self.meta || self.online || self.on || self.brightness || self.kelvin || self.error
    }

    /// Diff two snapshots of the same light.
    pub fn between(before: &LightRecord, after: &LightRecord) -> Self {
        Self {
            meta: before.name != after.name
                || before.model != after.model
                || before.firmware != after.firmware
                || before.endpoint != after.endpoint,
            online: before.online() != after.online(),
            on: before.desired.on != after.desired.on,
            brightness: before.desired.brightness != after.desired.brightness,
            kelvin: before.desired.kelvin != after.desired.kelvin,
            error: before.last_error != after.last_error,
        }
    }
}

/// Every light gaffer currently knows about.
#[derive(Debug, Default)]
pub struct World {
    lights: BTreeMap<LightId, LightRecord>,
}

impl World {
    pub fn get(&self, id: &str) -> Option<&LightRecord> {
        self.lights.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &LightRecord> {
        self.lights.values()
    }

    pub fn ids(&self) -> Vec<LightId> {
        self.lights.keys().cloned().collect()
    }

    pub fn insert(&mut self, record: LightRecord) -> Option<LightRecord> {
        self.lights.insert(record.id.clone(), record)
    }

    pub fn remove(&mut self, id: &str) -> Option<LightRecord> {
        self.lights.remove(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut LightRecord> {
        self.lights.get_mut(id)
    }

    /// The lights a selector addresses.
    pub fn select(&self, selector: &Selector) -> Vec<LightId> {
        self.lights
            .values()
            .filter(|light| selector.matches(&light.id, &light.name))
            .map(|light| light.id.clone())
            .collect()
    }

    /// The state of the "all lights" pseudo-device.
    ///
    /// Aggregates online members only, so an unreachable light does not drag the
    /// average around. Falls back to every light when none are online, so the
    /// group reads as the last known state rather than going blank.
    pub fn group_state(&self) -> Option<LightState> {
        let online: Vec<LightState> =
            self.lights.values().filter(|l| l.online()).map(|l| l.desired).collect();
        if !online.is_empty() {
            return group::aggregate(&online);
        }
        let all: Vec<LightState> = self.lights.values().map(|l| l.desired).collect();
        group::aggregate(&all)
    }

    /// Number of members currently reachable.
    pub fn online_count(&self) -> usize {
        self.lights.values().filter(|l| l.online()).count()
    }

    /// Apply a patch, returning the lights that now owe a push and what each
    /// one's clients would see change.
    ///
    /// `all` uses **group semantics**: the patch resolves once against the
    /// aggregate state and the resulting absolute state fans out to every
    /// member. That is what makes `gaffer toggle` do the obvious thing when one
    /// light is on and one is off, and it keeps the `all` selector consistent
    /// with the `/lights/all` D-Bus object. A named selector instead resolves
    /// per light, so `gaffer set left +10%` is relative to *that* light.
    pub fn apply(&mut self, target: &Target, patch: &StatePatch) -> Vec<(LightId, Changed)> {
        let now = Instant::now();

        match target {
            Target::Id(id) => {
                let Some(light) = self.lights.get_mut(id) else {
                    return Vec::new();
                };
                let next = patch.apply(light.desired);
                light
                    .set_desired(next, now)
                    .map(|changed| vec![(light.id.clone(), changed)])
                    .unwrap_or_default()
            }
            Target::Select(Selector::All) => {
                let Some(base) = self.group_state() else {
                    return Vec::new();
                };
                let absolute = as_absolute(patch.apply(base));
                self.lights
                    .values_mut()
                    .filter_map(|light| {
                        let next = absolute.apply(light.desired);
                        Some((light.id.clone(), light.set_desired(next, now)?))
                    })
                    .collect()
            }
            Target::Select(selector) => self
                .lights
                .values_mut()
                .filter(|light| selector.matches(&light.id, &light.name))
                .filter_map(|light| {
                    let next = patch.apply(light.desired);
                    Some((light.id.clone(), light.set_desired(next, now)?))
                })
                .collect(),
        }
    }
}

/// What the group looked like at a point in time, for change detection.
///
/// Any per-light change moves the aggregate, so the group object needs its own
/// before/after comparison rather than reusing a member's diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupSnapshot {
    state: Option<LightState>,
    online: usize,
    members: usize,
}

impl GroupSnapshot {
    pub fn of(world: &World) -> Self {
        Self {
            state: world.group_state(),
            online: world.online_count(),
            members: world.lights.len(),
        }
    }

    /// Whether membership or reachability moved, which the manager also exposes.
    pub fn membership_changed(before: Self, after: Self) -> bool {
        before.members != after.members || before.online != after.online
    }

    /// Which of the group object's properties differ between two snapshots.
    pub fn diff(before: Self, after: Self) -> Changed {
        let (b, a) = (before.state.unwrap_or_default(), after.state.unwrap_or_default());
        Changed {
            meta: false,
            online: before.online != after.online,
            on: b.on != a.on,
            brightness: b.brightness != a.brightness,
            kelvin: b.kelvin != a.kelvin,
            error: false,
        }
    }
}

/// Turn a resolved state into a patch that sets every field absolutely.
fn as_absolute(state: LightState) -> StatePatch {
    StatePatch {
        power: Some(if state.on { Power::On } else { Power::Off }),
        brightness: Some(Adjust::Set(i32::from(state.brightness))),
        kelvin: Some(Adjust::Set(i32::from(state.kelvin))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, name: &str, state: LightState, online: bool) -> LightRecord {
        LightRecord {
            id: id.to_string(),
            fullname: format!("{name}._elg._tcp.local."),
            name: name.to_string(),
            model: "Elgato Key Light MK.2".to_string(),
            firmware: String::new(),
            endpoint: Endpoint { addr: IpAddr::from([192, 0, 2, 10]), port: 9123 },
            desired: state,
            reported: online.then_some(state),
            last_error: String::new(),
            dirty_since: None,
        }
    }

    fn ids(touched: &[(LightId, Changed)]) -> Vec<String> {
        touched.iter().map(|(id, _)| id.clone()).collect()
    }

    fn world() -> World {
        let mut world = World::default();
        world.insert(record(
            "a",
            "Key Light Left",
            LightState { on: true, brightness: 40, kelvin: 4000 },
            true,
        ));
        world.insert(record(
            "b",
            "Key Light Right",
            LightState { on: true, brightness: 60, kelvin: 5000 },
            true,
        ));
        world
    }

    #[test]
    fn a_base_url_is_built_only_from_an_address_and_a_port() {
        let v4 = Endpoint { addr: IpAddr::from([192, 0, 2, 10]), port: 9123 };
        assert_eq!(v4.base_url(), "http://192.0.2.10:9123");
    }

    #[test]
    fn ipv6_literals_are_bracketed() {
        // Without brackets the address's own colons would be read as a port.
        let v6 = Endpoint { addr: "2001:db8::1".parse().unwrap(), port: 9123 };
        assert_eq!(v6.base_url(), "http://[2001:db8::1]:9123");
    }

    #[test]
    fn a_url_cannot_carry_a_path_however_hostile_discovery_was() {
        // The endpoint holds a parsed address, so there is no string an mDNS
        // advertisement could supply that adds a path, query or userinfo.
        for addr in ["127.0.0.1", "::1", "192.0.2.10"] {
            let url = Endpoint { addr: addr.parse().unwrap(), port: 11434 }.base_url();
            let after_scheme = url.trim_start_matches("http://");
            assert!(!after_scheme.contains('/'), "{url} gained a path");
            assert!(!after_scheme.contains('#') && !after_scheme.contains('?'), "{url}");
            assert!(!after_scheme.contains('@'), "{url} gained userinfo");
        }
    }

    #[test]
    fn a_named_selector_adjusts_relative_to_that_light() {
        let mut world = world();
        let patch = StatePatch { brightness: Some(Adjust::By(10)), ..StatePatch::EMPTY };
        let touched = world.apply(&Target::Select(Selector::parse("left")), &patch);

        assert_eq!(ids(&touched), vec!["a".to_string()]);
        assert!(touched[0].1.brightness, "the change must be announced to clients");
        assert_eq!(world.get("a").unwrap().desired.brightness, 50);
        assert_eq!(world.get("b").unwrap().desired.brightness, 60, "right must not move");
    }

    #[test]
    fn all_resolves_once_against_the_aggregate_then_fans_out() {
        let mut world = world();
        // Aggregate brightness is 50; +10 makes 60 for *both*, rather than
        // 50 and 70. The group behaves as one device.
        let patch = StatePatch { brightness: Some(Adjust::By(10)), ..StatePatch::EMPTY };
        let touched = world.apply(&Target::Select(Selector::All), &patch);

        assert_eq!(touched.len(), 2);
        assert_eq!(world.get("a").unwrap().desired.brightness, 60);
        assert_eq!(world.get("b").unwrap().desired.brightness, 60);
    }

    #[test]
    fn toggling_a_mixed_group_turns_everything_on() {
        let mut world = World::default();
        world.insert(record("a", "Left", LightState { on: true, ..Default::default() }, true));
        world.insert(record("b", "Right", LightState { on: false, ..Default::default() }, true));

        let touched =
            world.apply(&Target::Select(Selector::All), &StatePatch::power(Power::Toggle));

        assert!(world.get("a").unwrap().desired.on);
        assert!(world.get("b").unwrap().desired.on, "the off light must come on, not flip off");
        // Only "b" actually moved. "a" was already in the target state, so the
        // fan-out owes it no push — a group command must not generate needless
        // HTTP traffic to lights that already agree.
        assert_eq!(ids(&touched), vec!["b".to_string()]);
    }

    #[test]
    fn a_command_matching_nothing_touches_nothing() {
        let mut world = world();
        let touched = world
            .apply(&Target::Select(Selector::parse("nonexistent")), &StatePatch::power(Power::On));
        assert!(touched.is_empty());
    }

    #[test]
    fn re_issuing_a_command_still_pushes_when_hardware_disagrees() {
        let mut world = world();
        // Simulate someone changing the light behind gaffer's back.
        world.get_mut("a").unwrap().reported =
            Some(LightState { on: true, brightness: 5, kelvin: 4000 });

        let patch = StatePatch { brightness: Some(Adjust::Set(40)), ..StatePatch::EMPTY };
        let touched = world.apply(&Target::Id("a".into()), &patch);

        assert_eq!(
            ids(&touched),
            vec!["a".to_string()],
            "desired was already 40 but hardware was not"
        );
        assert!(world.get("a").unwrap().dirty_since.is_some());
        assert!(!touched[0].1.any(), "a corrective push nobody can see emits no signal");
    }

    #[test]
    fn a_genuine_no_op_does_not_mark_the_light_dirty() {
        let mut world = world();
        let patch = StatePatch { brightness: Some(Adjust::Set(40)), ..StatePatch::EMPTY };
        assert!(world.apply(&Target::Id("a".into()), &patch).is_empty());
        assert!(world.get("a").unwrap().dirty_since.is_none());
    }

    #[test]
    fn offline_members_are_excluded_from_the_aggregate() {
        let mut world = world();
        world.get_mut("b").unwrap().reported = None;

        // Only "a" (40%) is online, so the group reads 40 rather than 50.
        assert_eq!(world.group_state().unwrap().brightness, 40);
        assert_eq!(world.online_count(), 1);
    }

    #[test]
    fn the_group_falls_back_to_all_members_when_none_are_online() {
        let mut world = world();
        for id in ["a", "b"] {
            world.get_mut(id).unwrap().reported = None;
        }
        assert_eq!(
            world.group_state().unwrap().brightness,
            50,
            "should show last known, not blank"
        );
        assert_eq!(world.online_count(), 0);
    }

    #[test]
    fn an_empty_world_has_no_group_state() {
        assert_eq!(World::default().group_state(), None);
    }

    #[test]
    fn the_change_diff_reports_only_what_moved() {
        let before =
            record("a", "Left", LightState { on: true, brightness: 40, kelvin: 4000 }, true);
        let mut after = before.clone();
        after.desired.brightness = 50;

        let changed = Changed::between(&before, &after);
        assert!(changed.brightness);
        assert!(!changed.on && !changed.kelvin && !changed.meta && !changed.online);
        assert!(changed.any());

        assert!(!Changed::between(&before, &before).any());
    }

    fn snapshot(state: Option<LightState>, online: usize, members: usize) -> GroupSnapshot {
        GroupSnapshot { state, online, members }
    }

    #[test]
    fn an_unchanged_group_emits_nothing() {
        let before = snapshot(Some(LightState { on: true, brightness: 40, kelvin: 4000 }), 2, 2);
        assert!(!GroupSnapshot::diff(before, before).any());
    }

    #[test]
    fn a_group_brightness_move_is_reported_alone() {
        let before = snapshot(Some(LightState { on: true, brightness: 40, kelvin: 4000 }), 2, 2);
        let after = snapshot(Some(LightState { on: true, brightness: 50, kelvin: 4000 }), 2, 2);

        let changed = GroupSnapshot::diff(before, after);
        assert!(changed.brightness);
        assert!(!changed.on && !changed.kelvin && !changed.online);
    }

    #[test]
    fn a_member_going_offline_changes_the_group() {
        let state = Some(LightState { on: true, brightness: 40, kelvin: 4000 });
        let (before, after) = (snapshot(state, 2, 2), snapshot(state, 1, 2));
        assert!(GroupSnapshot::diff(before, after).online);
        assert!(GroupSnapshot::membership_changed(before, after));
    }

    #[test]
    fn an_empty_group_compares_against_defaults_without_panicking() {
        assert!(!GroupSnapshot::diff(snapshot(None, 0, 0), snapshot(None, 0, 0)).any());

        let appeared =
            GroupSnapshot::diff(snapshot(None, 0, 0), snapshot(Some(LightState::default()), 1, 1));
        assert!(appeared.online);
    }
}
