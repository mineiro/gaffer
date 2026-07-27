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

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::time::Instant;

use gaffer_core::{Adjust, LightState, Link, LinkMode, Power, Scene, Selector, StatePatch, group};

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

impl LightRecord {
    /// Move this lamp without touching its gang.
    ///
    /// Only alt-drag uses this: every other path goes through `World::apply`,
    /// which expands gangs. Bypassing propagation is what lets the user set a
    /// new difference instead of dragging the whole instrument.
    pub fn set_desired_alone(&mut self, next: LightState) -> Option<Changed> {
        self.set_desired(next, Instant::now())
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
    /// Gangs. A lamp belongs to at most one, which is what lets the panel draw
    /// a single wire per port and a gang collapse into one card.
    links: Vec<Link>,
    /// Saved scenes, by name. User intent like gangs, so they sit under the
    /// same lock and are persisted by the same path.
    scenes: BTreeMap<String, Scene>,
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

    /// The gang a lamp belongs to, if any.
    pub fn link_of(&self, id: &str) -> Option<&Link> {
        self.links.iter().find(|link| link.contains(id))
    }

    pub fn links(&self) -> impl Iterator<Item = &Link> {
        self.links.iter()
    }

    /// Gang these lamps, learning the brightness differences they have now.
    ///
    /// Any lamp already in a gang leaves it first — a lamp is in at most one,
    /// which is what lets the panel draw one wire per port. Returns the members
    /// actually ganged.
    pub fn link(&mut self, members: &[LightId]) -> Vec<LightId> {
        let present: Vec<LightId> =
            members.iter().filter(|id| self.lights.contains_key(*id)).cloned().collect();
        if present.len() < 2 {
            return Vec::new();
        }

        for id in &present {
            self.unlink(id);
        }

        // Ordered, not a map: the first lamp named is the reference, so
        // `link a b` reads "link b onto a" and a later mirror snaps onto a.
        let members: Vec<(String, LightState)> = present
            .iter()
            .filter_map(|id| Some((id.clone(), self.lights.get(id)?.desired)))
            .collect();
        self.links.push(Link::learn(&members));
        present
    }

    /// Take a lamp out of its gang, returning the members that were in it.
    ///
    /// A gang of one is not a gang, so the remainder is dissolved too.
    pub fn unlink(&mut self, id: &str) -> Vec<LightId> {
        let Some(index) = self.links.iter().position(|link| link.contains(id)) else {
            return Vec::new();
        };
        let mut link = self.links.remove(index);
        let mut affected: Vec<LightId> = link.members().cloned().collect();

        link.remove(id);
        if link.len() >= 2 {
            self.links.push(link);
            affected.retain(|m| m == id || self.link_of(m).is_some());
        }
        affected
    }

    /// Replace a gang's stored links wholesale, for restoring from config.
    pub fn set_links(&mut self, links: Vec<Link>) {
        self.links = links;
    }

    /// Drive a lamp's gang by its level — the gang's position as one fader.
    ///
    /// The honest way to move a gang from a single control. Writing a member's
    /// brightness instead requires knowing that member's offset, and there may
    /// be no member sitting at the level at all once alt-drags have moved the
    /// offsets around.
    pub fn set_link_level(&mut self, id: &str, level: i32) -> Vec<(LightId, Changed)> {
        let states: BTreeMap<String, LightState> =
            self.lights.iter().map(|(id, light)| (id.clone(), light.desired)).collect();

        let Some(link) = self.links.iter().find(|link| link.contains(id)) else {
            return Vec::new();
        };
        // Temperature and power mirror, so any member supplies them.
        let Some(template) = link.members().find_map(|m| states.get(m)).copied() else {
            return Vec::new();
        };

        let now = Instant::now();
        let resolved = link.resolve_from_level(level, template);
        resolved
            .into_iter()
            .filter_map(|(id, next)| {
                let light = self.lights.get_mut(&id)?;
                Some((id, light.set_desired(next, now)?))
            })
            .collect()
    }

    /// A lamp's gang level, if it is ganged.
    pub fn link_level(&self, id: &str) -> Option<i32> {
        let states: BTreeMap<String, LightState> =
            self.lights.iter().map(|(id, light)| (id.clone(), light.desired)).collect();
        self.link_of(id)?.level(&states)
    }

    /// Change how a lamp's gang tracks.
    ///
    /// Mirroring is destructive — every member snaps onto the gang's reference,
    /// the lamp named first when it was made — so the moved lamps are returned
    /// as ordinary changes for the caller to push and announce.
    pub fn set_link_mode(&mut self, id: &str, mode: LinkMode) -> Vec<(LightId, Changed)> {
        let states: BTreeMap<String, LightState> =
            self.lights.iter().map(|(id, light)| (id.clone(), light.desired)).collect();

        let Some(link) = self.links.iter_mut().find(|link| link.contains(id)) else {
            return Vec::new();
        };
        let Some(moved) = link.set_mode(mode, &states) else {
            return Vec::new(); // switching to offset moves nothing
        };

        let now = Instant::now();
        moved
            .into_iter()
            .filter_map(|(id, next)| {
                let light = self.lights.get_mut(&id)?;
                Some((id, light.set_desired(next, now)?))
            })
            .collect()
    }

    /// Teach a lamp's gang a new difference after it moved on its own.
    ///
    /// This is alt-drag: the lamp keeps the value it was just given and the
    /// gang adopts the new spacing, rather than dragging the lamp back.
    pub fn relearn(&mut self, id: &str, brightness: u8) -> bool {
        let others: BTreeMap<String, LightState> = self
            .lights
            .iter()
            .filter(|(other, _)| other.as_str() != id)
            .map(|(other, light)| (other.clone(), light.desired))
            .collect();

        self.links
            .iter_mut()
            .find(|link| link.contains(id))
            .is_some_and(|link| link.relearn(id, brightness, &others))
    }

    /// Saved scenes, by name.
    pub fn scenes(&self) -> &BTreeMap<String, Scene> {
        &self.scenes
    }

    pub fn scene(&self, name: &str) -> Option<&Scene> {
        self.scenes.get(name)
    }

    /// Save the whole desk under a name, replacing any scene already there.
    pub fn save_scene(&mut self, name: &str) {
        let scene = self.capture_scene();
        self.scenes.insert(name.to_string(), scene);
    }

    /// Forget a scene. `false` if there was none by that name.
    pub fn forget_scene(&mut self, name: &str) -> bool {
        self.scenes.remove(name).is_some()
    }

    /// Replace the stored scenes wholesale, for restoring from config.
    pub fn set_scenes(&mut self, scenes: BTreeMap<String, Scene>) {
        self.scenes = scenes;
    }

    /// Photograph the whole desk: every lamp's desired state, and the gangs.
    ///
    /// Desired rather than reported, so a scene taken while a lamp is offline
    /// records what the user asked for and not the last thing the hardware
    /// managed to say.
    pub fn capture_scene(&self) -> Scene {
        let states: BTreeMap<String, LightState> =
            self.lights.iter().map(|(id, light)| (id.clone(), light.desired)).collect();
        Scene::capture(&states, &self.links)
    }

    /// Restore a scene: detach, form, then drive values.
    ///
    /// The order is load-bearing. Detaching every named lamp first means a
    /// gang can be rebuilt from lamps that are currently spread across two
    /// other gangs, without the intermediate states mattering. Values come last
    /// so nothing is pushed through a link that is about to be replaced.
    ///
    /// Lamps the scene does not name are left entirely alone — not re-ganged,
    /// not moved. A lamp *is* touched if it was ganged to one the scene names:
    /// its gang loses that member, and dissolves if fewer than two remain. That
    /// is a topology change, not a value change; the lamp keeps its brightness.
    pub fn apply_scene(&mut self, scene: &Scene) -> Vec<(LightId, Changed)> {
        let present: BTreeSet<String> = self.lights.keys().cloned().collect();
        let plan = scene.plan(&present);

        for id in &plan.detach {
            self.unlink(id);
        }
        for gang in &plan.form {
            self.links.push(gang.to_link());
        }

        // Gangs are driven by level rather than per-member brightness, so the
        // stored offsets are reproduced exactly even where a member's value
        // would clamp. Loose lamps carry absolute values already.
        let now = Instant::now();
        let values = plan
            .form
            .iter()
            .flat_map(|gang| gang.to_link().resolve_from_level(gang.level, gang.template()))
            .chain(plan.lamps.iter().map(|(id, next)| (id.clone(), *next)));

        values
            .filter_map(|(id, next)| {
                let light = self.lights.get_mut(&id)?;
                Some((id, light.set_desired(next, now)?))
            })
            .collect()
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
    ///
    /// Gangs are then expanded over the result: touching any member moves the
    /// whole instrument. Propagation happens here, once, inside a single apply
    /// — it never re-enters, so a symmetric link cannot chase itself.
    pub fn apply(&mut self, target: &Target, patch: &StatePatch) -> Vec<(LightId, Changed)> {
        let now = Instant::now();
        let intended = self.intended(target, patch);
        let expanded = self.expand_through_links(intended);

        expanded
            .into_iter()
            .filter_map(|(id, next)| {
                let light = self.lights.get_mut(&id)?;
                Some((id, light.set_desired(next, now)?))
            })
            .collect()
    }

    /// What the command asks for, before gangs are considered.
    fn intended(&self, target: &Target, patch: &StatePatch) -> BTreeMap<LightId, LightState> {
        match target {
            Target::Id(id) => self
                .lights
                .get(id)
                .map(|light| (id.clone(), patch.apply(light.desired)))
                .into_iter()
                .collect(),
            Target::Select(Selector::All) => {
                let Some(base) = self.group_state() else {
                    return BTreeMap::new();
                };
                let absolute = as_absolute(patch.apply(base));
                self.lights
                    .values()
                    .map(|light| (light.id.clone(), absolute.apply(light.desired)))
                    .collect()
            }
            Target::Select(selector) => self
                .lights
                .values()
                .filter(|light| selector.matches(&light.id, &light.name))
                .map(|light| (light.id.clone(), patch.apply(light.desired)))
                .collect(),
        }
    }

    /// Widen a set of intended states so every gang moves as one instrument.
    ///
    /// When a command touches several members of one gang — `all` does — the
    /// lowest-id member acts as the mover, so the outcome is deterministic
    /// rather than depending on iteration order.
    fn expand_through_links(
        &self,
        intended: BTreeMap<LightId, LightState>,
    ) -> BTreeMap<LightId, LightState> {
        let mut out = intended.clone();

        for link in &self.links {
            let Some(mover) = link.members().find(|member| intended.contains_key(*member)) else {
                continue;
            };
            for (id, state) in link.resolve(mover, intended[mover]) {
                if self.lights.contains_key(&id) {
                    out.insert(id, state);
                }
            }
        }

        out
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

    #[test]
    fn ganging_learns_the_current_difference() {
        let mut world = world(); // a=40%, b=60%
        assert_eq!(world.link(&["a".into(), "b".into()]).len(), 2);

        let link = world.link_of("a").expect("a should be ganged");
        assert_eq!(link.offset_of("a"), Some(0));
        assert_eq!(link.offset_of("b"), Some(20));
    }

    #[test]
    fn moving_either_member_moves_the_whole_gang() {
        let mut world = world();
        world.link(&["a".into(), "b".into()]);

        let patch = StatePatch { brightness: Some(Adjust::Set(50)), ..StatePatch::EMPTY };
        let touched = world.apply(&Target::Id("a".into()), &patch);

        assert_eq!(ids(&touched), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(world.get("a").unwrap().desired.brightness, 50);
        assert_eq!(world.get("b").unwrap().desired.brightness, 70, "offset +20 preserved");
    }

    #[test]
    fn power_gangs_so_the_pair_is_one_instrument() {
        let mut world = world();
        world.link(&["a".into(), "b".into()]);

        world.apply(&Target::Id("a".into()), &StatePatch::power(Power::Off));
        assert!(!world.get("a").unwrap().desired.on);
        assert!(!world.get("b").unwrap().desired.on, "turning the gang off turns both off");
    }

    #[test]
    fn temperature_mirrors_across_the_gang() {
        let mut world = world(); // a=4000K, b=5000K
        world.link(&["a".into(), "b".into()]);

        let patch = StatePatch { kelvin: Some(Adjust::Set(4200)), ..StatePatch::EMPTY };
        world.apply(&Target::Id("a".into()), &patch);

        assert_eq!(world.get("a").unwrap().desired.kelvin, 4200);
        assert_eq!(
            world.get("b").unwrap().desired.kelvin,
            4200,
            "temperature mirrors, never offsets"
        );
    }

    #[test]
    fn applying_the_resolved_state_again_changes_nothing() {
        // The oscillation guard at world level: a gang that has settled must
        // not keep generating work when the same command arrives twice.
        let mut world = world();
        world.link(&["a".into(), "b".into()]);
        let patch = StatePatch { brightness: Some(Adjust::Set(50)), ..StatePatch::EMPTY };

        world.apply(&Target::Id("a".into()), &patch);
        for id in ["a", "b"] {
            let light = world.get_mut(id).unwrap();
            light.reported = Some(light.desired);
            light.dirty_since = None;
        }

        assert!(world.apply(&Target::Id("a".into()), &patch).is_empty());
        assert!(world.apply(&Target::Id("b".into()), &StatePatch::EMPTY).is_empty());
    }

    #[test]
    fn a_lamp_belongs_to_at_most_one_gang() {
        let mut world = world();
        world.insert(record(
            "c",
            "Mini",
            LightState { on: true, brightness: 10, kelvin: 4000 },
            true,
        ));

        world.link(&["a".into(), "b".into()]);
        world.link(&["b".into(), "c".into()]);

        assert!(world.link_of("a").is_none(), "a's gang dissolved when b left it");
        assert!(world.link_of("b").is_some());
        assert!(world.link_of("c").is_some());
        assert_eq!(world.links().count(), 1);
    }

    #[test]
    fn unlinking_dissolves_a_pair_entirely() {
        let mut world = world();
        world.link(&["a".into(), "b".into()]);

        let affected = world.unlink("a");
        assert_eq!(affected.len(), 2, "both lamps are affected when a pair breaks");
        assert!(world.link_of("a").is_none());
        assert!(world.link_of("b").is_none(), "a gang of one is not a gang");
    }

    #[test]
    fn ganging_needs_at_least_two_present_lamps() {
        let mut world = world();
        assert!(world.link(&["a".into()]).is_empty());
        assert!(world.link(&["a".into(), "nonexistent".into()]).is_empty());
        assert!(world.link_of("a").is_none());
    }

    #[test]
    fn alt_drag_relearns_the_spacing() {
        let mut world = world();
        world.link(&["a".into(), "b".into()]); // offset +20

        // b is dragged alone to 45 while a stays at 40.
        world.get_mut("b").unwrap().desired.brightness = 45;
        assert!(world.relearn("b", 45));
        assert_eq!(world.link_of("b").unwrap().offset_of("b"), Some(5));

        let patch = StatePatch { brightness: Some(Adjust::Set(60)), ..StatePatch::EMPTY };
        world.apply(&Target::Id("a".into()), &patch);
        assert_eq!(world.get("b").unwrap().desired.brightness, 65);
    }

    #[test]
    fn all_still_addresses_every_lamp_with_a_gang_present() {
        // `all` touches both members, so the lowest id acts as the mover and
        // the gang keeps its shape rather than being flattened.
        let mut world = world();
        world.link(&["a".into(), "b".into()]); // offset +20

        let patch = StatePatch { brightness: Some(Adjust::Set(50)), ..StatePatch::EMPTY };
        world.apply(&Target::Select(Selector::All), &patch);

        assert_eq!(world.get("a").unwrap().desired.brightness, 50);
        assert_eq!(world.get("b").unwrap().desired.brightness, 70);
    }

    #[test]
    fn leaving_a_gang_moves_no_lamp() {
        // Scene apply leans on this: removing a named lamp from a gang must
        // never disturb anyone's brightness, only the topology.
        let mut world = world();
        world.insert(record(
            "c",
            "Mini",
            LightState { on: true, brightness: 10, kelvin: 4000 },
            true,
        ));
        world.link(&["a".into(), "b".into(), "c".into()]);

        let before: Vec<LightState> =
            ["a", "b", "c"].iter().map(|id| world.get(id).unwrap().desired).collect();

        world.unlink("a");

        let after: Vec<LightState> =
            ["a", "b", "c"].iter().map(|id| world.get(id).unwrap().desired).collect();
        assert_eq!(before, after, "unlinking must not move values");
    }

    #[test]
    fn a_remnant_of_two_or_more_survives_one_member_leaving() {
        let mut world = world();
        world.insert(record(
            "c",
            "Mini",
            LightState { on: true, brightness: 10, kelvin: 4000 },
            true,
        ));
        world.link(&["a".into(), "b".into(), "c".into()]);

        world.unlink("a");

        assert!(world.link_of("a").is_none(), "the named lamp leaves");
        assert!(world.link_of("b").is_some(), "the remnant survives");
        assert!(world.link_of("c").is_some());
        assert_eq!(world.link_of("b").unwrap().len(), 2);
    }

    #[test]
    fn a_gang_survives_a_member_disappearing_and_resumes_when_it_returns() {
        // Scene apply relies on stored offsets outliving an absent lamp.
        let mut world = world();
        world.link(&["a".into(), "b".into()]); // b rides +20

        let vanished = world.remove("b").expect("b was present");
        assert!(world.link_of("a").is_some(), "the gang outlives the missing member");

        world.insert(vanished);
        let patch = StatePatch { brightness: Some(Adjust::Set(50)), ..StatePatch::EMPTY };
        world.apply(&Target::Id("a".into()), &patch);
        assert_eq!(world.get("b").unwrap().desired.brightness, 70, "the offset survived");
    }

    #[test]
    fn a_gang_can_be_driven_by_its_level() {
        let mut world = world(); // a=40, b=60 -> b rides +20
        world.link(&["a".into(), "b".into()]);
        assert_eq!(world.link_level("a"), Some(40));

        world.set_link_level("a", 55);
        assert_eq!(world.get("a").unwrap().desired.brightness, 55);
        assert_eq!(world.get("b").unwrap().desired.brightness, 75);
        assert_eq!(world.link_level("b"), Some(55), "readable from either member");
    }

    #[test]
    fn a_scene_restores_both_the_values_and_the_gang() {
        let mut world = world();
        world.link(&["a".to_string(), "b".to_string()]);
        let scene = world.capture_scene();

        // Wreck it: pull the gang apart and move both lamps.
        world.unlink("a");
        world.apply(
            &Target::Select(Selector::All),
            &StatePatch { brightness: Some(Adjust::Set(5)), ..Default::default() },
        );
        assert!(world.link_of("a").is_none());

        world.apply_scene(&scene);
        assert_eq!(world.get("a").unwrap().desired.brightness, 40);
        assert_eq!(world.get("b").unwrap().desired.brightness, 60);
        assert!(world.link_of("a").is_some(), "the gang is back");
        assert_eq!(world.links().count(), 1, "and there is only one of it");
    }

    #[test]
    fn a_scene_can_rebuild_a_gang_from_lamps_that_are_ganged_elsewhere() {
        // Why detach runs before form: at capture time {a,b} were one gang,
        // but by apply time a is ganged to c instead. Applying has to take a
        // out of that gang before it can go back into this one.
        let mut world = world();
        world.insert(record(
            "c",
            "Mini",
            LightState { on: true, brightness: 20, kelvin: 4000 },
            true,
        ));
        world.link(&["a".to_string(), "b".to_string()]);
        let scene = world.capture_scene();

        world.unlink("a");
        world.link(&["a".to_string(), "c".to_string()]);
        world.apply_scene(&scene);

        let gang = world.link_of("a").expect("a is ganged");
        assert!(gang.contains("b"), "to b, as the scene says");
        assert!(!gang.contains("c"), "and c has been let go");
    }

    #[test]
    fn a_lamp_the_scene_never_names_keeps_its_brightness() {
        // c is not in the scene. Applying may dissolve the gang it happens to
        // be in — that is topology — but must not move it.
        let mut world = world();
        world.insert(record(
            "c",
            "Mini",
            LightState { on: true, brightness: 20, kelvin: 4000 },
            true,
        ));
        let scene = Scene::capture(
            &BTreeMap::from([("a".to_string(), world.get("a").unwrap().desired)]),
            &[],
        );

        world.link(&["a".to_string(), "c".to_string()]);
        let touched = world.apply_scene(&scene);

        assert!(!ids(&touched).contains(&"c".to_string()));
        assert_eq!(world.get("c").unwrap().desired.brightness, 20);
        assert!(world.link_of("c").is_none(), "though its gang is gone");
    }

    #[test]
    fn a_scene_survives_a_lamp_being_absent_and_re_forms_when_it_returns() {
        let mut world = world();
        world.link(&["a".to_string(), "b".to_string()]);
        let scene = world.capture_scene();

        world.remove("b");
        world.apply_scene(&scene);
        assert!(world.link_of("a").is_none(), "one lamp cannot be a gang");
        assert_eq!(world.get("a").unwrap().desired.brightness, 40, "but it still takes its value");

        world.insert(record(
            "b",
            "Key Light Right",
            LightState { on: false, brightness: 1, kelvin: 7000 },
            true,
        ));
        world.apply_scene(&scene);
        assert!(world.link_of("a").is_some_and(|gang| gang.contains("b")), "the gang re-forms");
        assert_eq!(world.get("b").unwrap().desired.brightness, 60);
    }

    #[test]
    fn applying_a_scene_twice_changes_nothing_the_second_time() {
        let mut world = world();
        world.link(&["a".to_string(), "b".to_string()]);
        let scene = world.capture_scene();
        world.apply(
            &Target::Select(Selector::All),
            &StatePatch { brightness: Some(Adjust::Set(5)), ..Default::default() },
        );

        assert!(
            world.apply_scene(&scene).iter().any(|(_, changed)| changed.any()),
            "the first apply moves lamps"
        );
        // The second still owes hardware a corrective push — `reported` only
        // converges once the reconciler runs — but nothing a client can see has
        // moved, so every reported change is empty.
        assert!(
            world.apply_scene(&scene).iter().all(|(_, changed)| !changed.any()),
            "the second changes nothing observable"
        );
        assert_eq!(world.links().count(), 1, "and does not stack a duplicate gang");
    }
}
