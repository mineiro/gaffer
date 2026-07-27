//! The D-Bus surface: gaffer's only client API.
//!
//! ```text
//! io.mineiro.gaffer                       bus name
//!  /io/mineiro/gaffer                     Manager1 + org.freedesktop.DBus.ObjectManager
//!    ├── /lights/00005E005301             Light1
//!    ├── /lights/00005E005302             Light1
//!    └── /lights/all                      Light1  (the group, as a light)
//! ```
//!
//! Two design notes worth keeping in mind when extending this:
//!
//! * **The group implements the same interface as a light.** Clients need no
//!   special case to control every light at once — the trick Photon used, kept.
//! * **Property setters take `&self`, never `&mut self`.** A `&mut self` setter
//!   holds the interface's write lock across the reply, which would deadlock
//!   against the reconciler trying to emit `PropertiesChanged` on that same
//!   object. Setters mutate the world optimistically and return immediately;
//!   the actual hardware I/O happens in the supervisor.

use std::sync::Arc;

use gaffer_core::{LightState, Selector, StatePatch, parse_patch};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};
use zbus::object_server::SignalEmitter;
use zbus::{Connection, fdo, interface};

use crate::world::{Changed, GroupSnapshot, LightId, Target, World};

/// The well-known bus name gaffer owns.
pub const BUS_NAME: &str = "io.mineiro.gaffer";
/// Root object, carrying the manager and the object manager.
pub const ROOT_PATH: &str = "/io/mineiro/gaffer";
/// The "all lights" pseudo-device.
pub const GROUP_PATH: &str = "/io/mineiro/gaffer/lights/all";

/// Work the supervisor must do on the caller's behalf.
#[derive(Clone, Debug)]
pub enum Request {
    /// A client changed desired state. The world has already been mutated
    /// optimistically; the supervisor owes the network push *and* the
    /// `PropertiesChanged` signals describing what moved.
    ///
    /// Emission is deliberately not done by the interface that made the change.
    /// Keeping one writer of the object server means there is a single place
    /// where a change can fail to be announced, which is exactly the bug this
    /// shape was introduced to fix.
    Applied {
        changes: Vec<(LightId, Changed)>,
        group_before: GroupSnapshot,
        group_after: GroupSnapshot,
    },
    /// Re-read every light's state now.
    RefreshAll,
    /// Re-probe the network now.
    Rescan,
    /// Physically blink these lights.
    Identify(Vec<LightId>),
    /// Gangs changed: persist them and emit Manager1.Links.
    ///
    /// Carries no light ids, because ganging moves no lamp by itself. When a
    /// mode change *does* move lamps, those travel as an ordinary `Applied`.
    LinksChanged,
    /// The set of saved scenes changed: persist it and emit Manager1.Scenes.
    ///
    /// Saving or deleting a scene moves no lamp. *Applying* one does, and that
    /// travels as an ordinary `Applied` plus a `LinksChanged` — applying a
    /// scene does not change which scenes exist.
    ScenesChanged,
}

/// Object path for a light, derived from its hardware id.
pub fn light_path(id: &str) -> String {
    let element: String =
        id.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    format!("{ROOT_PATH}/lights/{element}")
}

/// Longest scene name accepted. Generous for anything a person would type,
/// and short enough that a client cannot quietly grow the config file.
const MAX_SCENE_NAME: usize = 64;

/// Validate and normalise a scene name at the one place names enter gaffer.
///
/// Control characters are refused rather than escaped at each display site.
/// Scene names are echoed back by every client that lists them, including
/// terminal ones, and a name carrying an ANSI escape would let whoever set it
/// rewrite that output. Sanitising on the way out means every future client has
/// to remember to; refusing on the way in means none of them do.
///
/// Deliberately *rejects* where [`gaffer_core::text`] cleans. Names arriving
/// from mDNS are attacker-supplied with nobody to complain to, so they are
/// stripped and kept. A scene name was typed by the user, who can be told — and
/// silently storing something other than what they typed is its own bug.
fn scene_name(raw: &str) -> fdo::Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(fdo::Error::InvalidArgs("a scene needs a name".into()));
    }
    if name.chars().count() > MAX_SCENE_NAME {
        return Err(fdo::Error::InvalidArgs(format!(
            "scene names are limited to {MAX_SCENE_NAME} characters"
        )));
    }
    if let Some(bad) = name.chars().find(|c| c.is_control()) {
        return Err(fdo::Error::InvalidArgs(format!(
            "scene names cannot contain control characters (found U+{:04X})",
            bad as u32
        )));
    }
    Ok(name.to_string())
}

/// Ids matched by these selectors, in the order the caller named them.
///
/// Union, so both `link left right` and `link key` work, and deduplicated
/// because two selectors may overlap.
///
/// **Order is the contract.** The first lamp is the gang's reference — `link a
/// b` reads "link b onto a" — and it is what a later `SetLinkMode(mirror)`
/// snaps everything else onto. An earlier version sorted here, which made the
/// reference whichever member happened to have the lowest hardware id: `link
/// left right` and `link right left` built the same gang, and mirroring then
/// picked the wrong lamp. `World::link` preserves the order it is given, so
/// this is the only place the intent can be lost.
fn matched_in_order(world: &World, selectors: &[String]) -> Vec<String> {
    let mut matched: Vec<String> = Vec::new();
    for selector in selectors {
        for id in world.select(&Selector::parse(selector)) {
            if !matched.contains(&id) {
                matched.push(id);
            }
        }
    }
    matched
}

/// Which light an interface object is a view of.
#[derive(Clone, Debug)]
enum View {
    One(LightId),
    Group,
}

/// A light — or the group — as seen over D-Bus.
///
/// Holds no state of its own: every getter reads through to the shared world,
/// so there is exactly one source of truth and no cache to invalidate.
#[derive(Debug)]
pub struct Light1 {
    world: Arc<RwLock<World>>,
    view: View,
    requests: mpsc::Sender<Request>,
}

impl Light1 {
    fn new(world: Arc<RwLock<World>>, view: View, requests: mpsc::Sender<Request>) -> Self {
        Self { world, view, requests }
    }

    /// The state this object presents.
    async fn state(&self) -> LightState {
        let world = self.world.read().await;
        match &self.view {
            View::One(id) => world.get(id).map(|light| light.desired).unwrap_or_default(),
            View::Group => world.group_state().unwrap_or_default(),
        }
    }

    /// Read one field of the underlying record, or a default if it is gone.
    async fn field<T: Default>(&self, read: impl Fn(&crate::world::LightRecord) -> T) -> T {
        let world = self.world.read().await;
        match &self.view {
            View::One(id) => world.get(id).map(read).unwrap_or_default(),
            View::Group => T::default(),
        }
    }

    /// Apply a patch optimistically and nudge the supervisor to push it.
    ///
    /// Deliberately does not wait for hardware: the D-Bus reply means "accepted",
    /// and `PropertiesChanged` is what confirms the light actually moved.
    async fn mutate(&self, patch: StatePatch) -> fdo::Result<()> {
        let target = match &self.view {
            View::One(id) => Target::Id(id.clone()),
            View::Group => Target::Select(Selector::All),
        };

        let applied = {
            let mut world = self.world.write().await;
            apply_and_snapshot(&mut world, &target, &patch)
        }; // the write guard must not be held across the send below

        announce(&self.requests, applied).await
    }
}

#[interface(name = "io.mineiro.gaffer.Light1")]
impl Light1 {
    /// Stable hardware id. Empty for the group.
    #[zbus(property)]
    async fn id(&self) -> String {
        match &self.view {
            View::One(id) => id.clone(),
            View::Group => String::new(),
        }
    }

    #[zbus(property)]
    async fn name(&self) -> String {
        match &self.view {
            View::One(_) => self.field(|light| light.name.clone()).await,
            View::Group => "All Lights".to_string(),
        }
    }

    #[zbus(property)]
    async fn model(&self) -> String {
        self.field(|light| light.model.clone()).await
    }

    #[zbus(property)]
    async fn firmware(&self) -> String {
        self.field(|light| light.firmware.clone()).await
    }

    /// `host:port` the daemon is talking to. Empty for the group.
    #[zbus(property)]
    async fn address(&self) -> String {
        self.field(|light| light.endpoint.base_url()).await
    }

    /// Whether the hardware is currently reachable. For the group, whether *any*
    /// member is.
    #[zbus(property)]
    async fn online(&self) -> bool {
        let world = self.world.read().await;
        match &self.view {
            View::One(id) => world.get(id).is_some_and(|light| light.online()),
            View::Group => world.online_count() > 0,
        }
    }

    /// How many members are reachable. Always 1 or 0 for a single light.
    #[zbus(property)]
    async fn online_count(&self) -> u32 {
        let world = self.world.read().await;
        let count = match &self.view {
            View::One(id) => usize::from(world.get(id).is_some_and(|light| light.online())),
            View::Group => world.online_count(),
        };
        count as u32
    }

    /// Last transport error, or empty when healthy.
    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.field(|light| light.last_error.clone()).await
    }

    #[zbus(property)]
    async fn on(&self) -> bool {
        self.state().await.on
    }

    #[zbus(property)]
    async fn set_on(&self, value: bool) -> fdo::Result<()> {
        self.mutate(StatePatch::power(if value {
            gaffer_core::Power::On
        } else {
            gaffer_core::Power::Off
        }))
        .await
    }

    #[zbus(property)]
    async fn brightness(&self) -> u8 {
        self.state().await.brightness
    }

    #[zbus(property)]
    async fn set_brightness(&self, value: u8) -> fdo::Result<()> {
        self.mutate(StatePatch {
            brightness: Some(gaffer_core::Adjust::Set(i32::from(value))),
            ..StatePatch::EMPTY
        })
        .await
    }

    #[zbus(property)]
    async fn kelvin(&self) -> u16 {
        self.state().await.kelvin
    }

    #[zbus(property)]
    async fn set_kelvin(&self, value: u16) -> fdo::Result<()> {
        self.mutate(StatePatch {
            kelvin: Some(gaffer_core::Adjust::Set(i32::from(value))),
            ..StatePatch::EMPTY
        })
        .await
    }

    #[zbus(property)]
    async fn min_kelvin(&self) -> u16 {
        gaffer_core::MIN_KELVIN
    }

    #[zbus(property)]
    async fn max_kelvin(&self) -> u16 {
        gaffer_core::MAX_KELVIN
    }

    /// Apply suffix-typed value tokens such as `["on", "42%", "4200k"]`.
    ///
    /// Tokens are parsed daemon-side so relative adjustments resolve against
    /// live state without a read-then-write race.
    async fn apply(&self, tokens: Vec<String>) -> fdo::Result<()> {
        let patch =
            parse_patch(&tokens).map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        if patch.is_empty() {
            return Err(fdo::Error::InvalidArgs("no values given".into()));
        }
        self.mutate(patch).await
    }

    /// Set this light's brightness *without* dragging its gang, and teach the
    /// gang the new difference.
    ///
    /// This is alt-drag. An ordinary brightness write moves the whole
    /// instrument; this one moves a single lamp and re-learns the spacing, so
    /// the ratio the user just dialled in becomes the one the gang keeps.
    async fn set_brightness_alone(&self, value: u8) -> fdo::Result<()> {
        let View::One(id) = &self.view else {
            return Err(fdo::Error::InvalidArgs(
                "the group has no gang of its own to re-learn".into(),
            ));
        };

        let applied = {
            let mut world = self.world.write().await;
            let group_before = GroupSnapshot::of(&world);

            let changes = match world.get_mut(id) {
                Some(light) => {
                    let next = LightState { brightness: value, ..light.desired };
                    light.set_desired_alone(next).map(|c| vec![(id.clone(), c)]).unwrap_or_default()
                }
                None => Vec::new(),
            };
            world.relearn(id, value);

            let group_after = GroupSnapshot::of(&world);
            Request::Applied { changes, group_before, group_after }
        };

        announce(&self.requests, applied).await?;
        send(&self.requests, Request::LinksChanged).await
    }

    /// Physically blink the light.
    async fn identify(&self) -> fdo::Result<()> {
        let ids = {
            let world = self.world.read().await;
            match &self.view {
                View::One(id) => vec![id.clone()],
                View::Group => world.ids(),
            }
        };
        send(&self.requests, Request::Identify(ids)).await
    }

    /// Re-read this light's state from hardware now.
    async fn refresh(&self) -> fdo::Result<()> {
        send(&self.requests, Request::RefreshAll).await
    }
}

/// The daemon itself.
#[derive(Debug)]
pub struct Manager1 {
    world: Arc<RwLock<World>>,
    requests: mpsc::Sender<Request>,
}

#[interface(name = "io.mineiro.gaffer.Manager1")]
impl Manager1 {
    /// Apply value tokens to whatever a selector matches.
    ///
    /// Returns the ids of the lights that matched, so a caller can tell the
    /// difference between "done" and "that selector matched nothing".
    async fn set_state(&self, selector: String, tokens: Vec<String>) -> fdo::Result<Vec<String>> {
        let patch =
            parse_patch(&tokens).map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        if patch.is_empty() {
            return Err(fdo::Error::InvalidArgs("no values given".into()));
        }

        let selector = Selector::parse(&selector);
        let (matched, applied) = {
            let mut world = self.world.write().await;
            let matched = world.select(&selector);
            let applied = apply_and_snapshot(&mut world, &Target::Select(selector), &patch);
            (matched, applied)
        };

        announce(&self.requests, applied).await?;
        Ok(matched)
    }

    /// Blink whatever a selector matches.
    async fn identify(&self, selector: String) -> fdo::Result<Vec<String>> {
        let matched = {
            let world = self.world.read().await;
            world.select(&Selector::parse(&selector))
        };
        send(&self.requests, Request::Identify(matched.clone())).await?;
        Ok(matched)
    }

    /// Re-probe the network and re-read every light.
    async fn rescan(&self) -> fdo::Result<()> {
        send(&self.requests, Request::Rescan).await
    }

    /// Gang the lights a selector matches, learning their current differences.
    ///
    /// Offset is the only way in, deliberately: a pair that already matches
    /// learns an offset of zero and behaves exactly like a mirror, so the
    /// friendly default is also the non-destructive one. `Mirror` is a separate,
    /// explicit act because it overwrites one lamp's values with another's.
    async fn link(&self, selectors: Vec<String>) -> fdo::Result<Vec<String>> {
        let members = {
            let mut world = self.world.write().await;
            let matched = matched_in_order(&world, &selectors);
            world.link(&matched)
        };

        if members.len() < 2 {
            return Err(fdo::Error::InvalidArgs(format!(
                "{:?} matched {} light(s); a gang needs at least two",
                selectors,
                members.len()
            )));
        }

        send(&self.requests, Request::LinksChanged).await?;
        Ok(members)
    }

    /// Break the gang a light belongs to. Returns every light affected.
    async fn unlink(&self, selector: String) -> fdo::Result<Vec<String>> {
        let affected = {
            let mut world = self.world.write().await;
            let matched = world.select(&Selector::parse(&selector));
            let mut affected: Vec<String> = Vec::new();
            for id in matched {
                affected.extend(world.unlink(&id));
            }
            affected.sort();
            affected.dedup();
            affected
        };

        send(&self.requests, Request::LinksChanged).await?;
        Ok(affected)
    }

    /// Change how a gang tracks: `offset` or `mirror`.
    ///
    /// Mirroring is destructive — every member snaps onto the gang's reference,
    /// the lamp named first when it was made — so it is a deliberate verb
    /// rather than an argument on `Link`. It also matches the interaction: the
    /// editor changes mode on a gang that already exists.
    async fn set_link_mode(&self, selector: String, mode: String) -> fdo::Result<()> {
        let mode = match mode.as_str() {
            "mirror" => gaffer_core::LinkMode::Mirror,
            "offset" => gaffer_core::LinkMode::Offset,
            other => {
                return Err(fdo::Error::InvalidArgs(format!(
                    "unknown link mode `{other}`; expected `offset` or `mirror`"
                )));
            }
        };

        let applied = {
            let mut world = self.world.write().await;
            let group_before = GroupSnapshot::of(&world);

            let matched = world.select(&Selector::parse(&selector));
            let Some(id) = matched.first().cloned() else {
                return Err(fdo::Error::InvalidArgs(format!("`{selector}` matched no light")));
            };
            if world.link_of(&id).is_none() {
                return Err(fdo::Error::InvalidArgs(format!("`{selector}` is not in a gang")));
            }

            let changes = world.set_link_mode(&id, mode);
            let group_after = GroupSnapshot::of(&world);
            Request::Applied { changes, group_before, group_after }
        };

        announce(&self.requests, applied).await?;
        send(&self.requests, Request::LinksChanged).await
    }

    /// Set a gang's level — its position as one fader.
    ///
    /// Prefer this to writing a member's `Brightness` when driving a gang from
    /// a single control. Going through a member requires knowing its offset,
    /// and after alt-drags there may be no member at the level at all, so a
    /// fader driven that way lands elsewhere and springs back.
    ///
    /// The level may fall outside 0–100: members clamp individually, so a gang
    /// compresses at the ends and recovers its spacing coming back.
    async fn set_link_level(&self, selector: String, level: i32) -> fdo::Result<()> {
        let applied = {
            let mut world = self.world.write().await;
            let group_before = GroupSnapshot::of(&world);

            let matched = world.select(&Selector::parse(&selector));
            let Some(id) = matched.first().cloned() else {
                return Err(fdo::Error::InvalidArgs(format!("`{selector}` matched no light")));
            };
            if world.link_of(&id).is_none() {
                return Err(fdo::Error::InvalidArgs(format!("`{selector}` is not in a gang")));
            }

            let changes = world.set_link_level(&id, level);
            let group_after = GroupSnapshot::of(&world);
            Request::Applied { changes, group_before, group_after }
        };

        announce(&self.requests, applied).await
    }

    /// A gang's current level.
    ///
    /// The counterpart to `SetLinkLevel`. Offered rather than left to be
    /// reconstructed from `Links` and a member's `Brightness`, because the
    /// obvious reconstruction — read through the member at offset zero — is
    /// wrong once alt-drags have moved every offset off zero, and a write-only
    /// level invites exactly that mistake.
    async fn link_level(&self, selector: String) -> fdo::Result<i32> {
        let world = self.world.read().await;
        let matched = world.select(&Selector::parse(&selector));
        let Some(id) = matched.first() else {
            return Err(fdo::Error::InvalidArgs(format!("`{selector}` matched no light")));
        };
        world
            .link_level(id)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("`{selector}` is not in a gang")))
    }

    /// Photograph the whole desk and save it under `name`.
    ///
    /// Whole-desk on purpose, with no selector: a scene is "how my desk looks",
    /// and the panel has no notion of a subset to pass. An existing scene of
    /// the same name is replaced — re-saving is how you update one.
    ///
    /// What is stored is topology plus values: each gang's members, mode,
    /// offsets and level, then whatever lamps are left over. Restoring
    /// therefore brings back the *instrument*, not just a row of brightnesses.
    async fn save_scene(&self, name: String) -> fdo::Result<()> {
        let name = scene_name(&name)?;
        self.world.write().await.save_scene(&name);
        send(&self.requests, Request::ScenesChanged).await
    }

    /// Restore a saved scene.
    ///
    /// Every lamp the scene names is detached from its current gang, the
    /// scene's gangs are formed, and then values are driven. Lamps the scene
    /// does not name are never re-ganged and never moved — though one ganged to
    /// a lamp the scene *does* name will lose that partner, and its gang
    /// dissolves if fewer than two members remain.
    ///
    /// Missing lamps are not an error. A gang re-forms from whichever members
    /// are present, and the absent one's offset is kept, so plugging it back in
    /// and re-applying restores the gang whole.
    async fn apply_scene(&self, name: String) -> fdo::Result<()> {
        let name = scene_name(&name)?;

        let applied = {
            let mut world = self.world.write().await;
            let Some(scene) = world.scene(&name).cloned() else {
                return Err(fdo::Error::InvalidArgs(format!("no scene named `{name}`")));
            };
            let group_before = GroupSnapshot::of(&world);
            let changes = world.apply_scene(&scene);
            let group_after = GroupSnapshot::of(&world);
            Request::Applied { changes, group_before, group_after }
        };

        announce(&self.requests, applied).await?;
        // Applying rearranges gangs, so `Links` owes an emission — but the set
        // of saved scenes is untouched, so `Scenes` does not.
        send(&self.requests, Request::LinksChanged).await
    }

    /// Forget a saved scene.
    async fn delete_scene(&self, name: String) -> fdo::Result<()> {
        let name = scene_name(&name)?;
        if !self.world.write().await.forget_scene(&name) {
            return Err(fdo::Error::InvalidArgs(format!("no scene named `{name}`")));
        }
        send(&self.requests, Request::ScenesChanged).await
    }

    /// The names of the saved scenes, sorted.
    ///
    /// Names only: a scene's contents are gaffer's business, and a panel that
    /// wants one applies it rather than rendering it.
    #[zbus(property)]
    async fn scenes(&self) -> Vec<String> {
        self.world.read().await.scenes().keys().cloned().collect()
    }

    /// The gangs, as member-id lists with each member's brightness offset.
    ///
    /// Shaped for a panel drawing wires: `(mode, [(id, offset)])`.
    ///
    /// **The first member is the gang's reference** — the lamp whose values win
    /// a mirror, which is the one named first when the gang was made. Carrying
    /// it by position rather than as a separate field keeps this property's
    /// signature stable for clients already reading it, and matches the mental
    /// model: `link a b` reads "link b onto a".
    #[zbus(property)]
    async fn links(&self) -> Vec<(String, Vec<(String, i32)>)> {
        self.world
            .read()
            .await
            .links()
            .map(|link| {
                let reference = link.reference();
                let mut members: Vec<(String, i32)> = Vec::with_capacity(link.len());
                if let Some(offset) = link.offset_of(reference) {
                    members.push((reference.to_string(), offset));
                }
                members.extend(
                    link.offsets()
                        .filter(|(id, _)| id.as_str() != reference)
                        .map(|(id, o)| (id.clone(), o)),
                );
                (link.mode.as_str().to_string(), members)
            })
            .collect()
    }

    #[zbus(property)]
    async fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    #[zbus(property)]
    async fn light_count(&self) -> u32 {
        self.world.read().await.iter().count() as u32
    }

    #[zbus(property)]
    async fn online_count(&self) -> u32 {
        self.world.read().await.online_count() as u32
    }
}

/// Mutate the world and capture the group's before/after state in one go.
///
/// The group aggregate is derived from every member, so it has to be sampled
/// either side of the mutation while the same write guard is held.
fn apply_and_snapshot(world: &mut World, target: &Target, patch: &StatePatch) -> Request {
    let group_before = GroupSnapshot::of(world);
    let changes = world.apply(target, patch);
    let group_after = GroupSnapshot::of(world);
    Request::Applied { changes, group_before, group_after }
}

/// Hand a completed mutation to the supervisor to push and announce.
async fn announce(requests: &mpsc::Sender<Request>, applied: Request) -> fdo::Result<()> {
    if let Request::Applied { changes, group_before, group_after } = &applied
        && changes.is_empty()
        && !GroupSnapshot::diff(*group_before, *group_after).any()
    {
        return Ok(()); // nothing moved; nothing to push or announce
    }
    send(requests, applied).await
}

async fn send(requests: &mpsc::Sender<Request>, request: Request) -> fdo::Result<()> {
    requests
        .send(request)
        .await
        .map_err(|_| fdo::Error::Failed("the gaffer supervisor has stopped".into()))
}

/// Publishes light objects and emits their change signals.
#[derive(Clone, Debug)]
pub struct Publisher {
    connection: Connection,
    world: Arc<RwLock<World>>,
    requests: mpsc::Sender<Request>,
}

impl Publisher {
    /// Connect to the session bus, claim the name, and publish the root objects.
    ///
    /// Everything is registered *before* the name is requested, so a client that
    /// activates gaffer never observes a window where the name exists but the
    /// objects do not.
    pub async fn connect(
        world: Arc<RwLock<World>>,
        requests: mpsc::Sender<Request>,
    ) -> zbus::Result<Self> {
        let manager = Manager1 { world: world.clone(), requests: requests.clone() };
        let group = Light1::new(world.clone(), View::Group, requests.clone());

        let connection = zbus::connection::Builder::session()?
            .serve_at(ROOT_PATH, manager)?
            .serve_at(ROOT_PATH, fdo::ObjectManager)?
            .serve_at(GROUP_PATH, group)?
            .name(BUS_NAME)?
            // zbus defaults to AllowReplacement|ReplaceExisting, which lets a
            // second gafferd silently steal the name and leaves the first one
            // running — two daemons then fight over the same lights. gaffer is
            // a singleton, so a second instance should fail loudly instead.
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .build()
            .await?;

        Ok(Self { connection, world, requests })
    }

    /// The session-bus connection, for callers that need to watch it.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Publish a newly discovered light. `InterfacesAdded` follows automatically.
    pub async fn add_light(&self, id: &LightId) -> zbus::Result<()> {
        let light = Light1::new(self.world.clone(), View::One(id.clone()), self.requests.clone());
        let path = light_path(id);

        // `at` reports an already-occupied path by returning Ok(false), not an
        // error. Discarding that would let one light silently shadow another
        // and log success while doing it — so treat it as the failure it is.
        // Ids are canonicalised at discovery, which should make this
        // unreachable; if it ever fires, the canonical form has a collision.
        if !self.connection.object_server().at(path.as_str(), light).await? {
            warn!(%id, %path, "object path already taken; light not published");
            return Err(zbus::Error::Failure(format!("object path {path} already in use")));
        }

        debug!(%id, %path, "published light on the bus");
        Ok(())
    }

    /// Withdraw a light. `InterfacesRemoved` follows automatically.
    pub async fn remove_light(&self, id: &LightId) -> zbus::Result<()> {
        let path = light_path(id);
        self.connection.object_server().remove::<Light1, _>(path.as_str()).await?;
        debug!(%id, %path, "withdrew light from the bus");
        Ok(())
    }

    /// Emit `PropertiesChanged` for one light.
    pub async fn notify_light(&self, id: &LightId, changed: Changed) {
        self.notify_at(&light_path(id), changed).await;
    }

    /// Emit `PropertiesChanged` for the group object.
    pub async fn notify_group(&self, changed: Changed) {
        self.notify_at(GROUP_PATH, changed).await;
    }

    async fn notify_at(&self, path: &str, changed: Changed) {
        if !changed.any() {
            return;
        }
        if let Err(error) = self.emit(path, changed).await {
            debug!(%path, %error, "could not emit PropertiesChanged");
        }
    }

    async fn emit(&self, path: &str, changed: Changed) -> zbus::Result<()> {
        let iface_ref = self.connection.object_server().interface::<_, Light1>(path).await?;
        let iface = iface_ref.get().await;
        let emitter: &SignalEmitter<'static> = iface_ref.signal_emitter();

        if changed.meta {
            iface.name_changed(emitter).await?;
            iface.model_changed(emitter).await?;
            iface.firmware_changed(emitter).await?;
            iface.address_changed(emitter).await?;
        }
        if changed.online {
            iface.online_changed(emitter).await?;
            iface.online_count_changed(emitter).await?;
        }
        if changed.on {
            iface.on_changed(emitter).await?;
        }
        if changed.brightness {
            iface.brightness_changed(emitter).await?;
        }
        if changed.kelvin {
            iface.kelvin_changed(emitter).await?;
        }
        if changed.error {
            iface.last_error_changed(emitter).await?;
        }
        Ok(())
    }

    /// Emit `PropertiesChanged` for `Manager1.Links`.
    ///
    /// The property is annotated `emits-change`, so a client is entitled to
    /// watch it and never poll. It previously never fired, and clients were
    /// left inferring a gang change from an unrelated burst of identity
    /// properties — which gaffer should not have been emitting either.
    pub async fn notify_links(&self) {
        let Ok(iface_ref) =
            self.connection.object_server().interface::<_, Manager1>(ROOT_PATH).await
        else {
            return;
        };
        let iface = iface_ref.get().await;
        let _ = iface.links_changed(iface_ref.signal_emitter()).await;
    }

    /// Emit `PropertiesChanged` for `Manager1.Scenes`.
    pub async fn notify_scenes(&self) {
        let Ok(iface_ref) =
            self.connection.object_server().interface::<_, Manager1>(ROOT_PATH).await
        else {
            return;
        };
        let iface = iface_ref.get().await;
        let _ = iface.scenes_changed(iface_ref.signal_emitter()).await;
    }

    /// Emit `PropertiesChanged` for the manager's counters.
    pub async fn notify_manager(&self) {
        let Ok(iface_ref) =
            self.connection.object_server().interface::<_, Manager1>(ROOT_PATH).await
        else {
            return;
        };
        let iface = iface_ref.get().await;
        let emitter = iface_ref.signal_emitter();
        let _ = iface.light_count_changed(emitter).await;
        let _ = iface.online_count_changed(emitter).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render an interface's D-Bus contract: names, types and access, with doc
    /// comments stripped.
    ///
    /// Comments are removed deliberately. The point of the snapshot is to catch
    /// a renamed property or a changed signature — things that silently break
    /// every client. If rewording a doc comment also failed the check, the
    /// habit would become "regenerate the snapshot until it passes", which is
    /// exactly the reflex that makes such a test worthless.
    fn contract<I: zbus::object_server::Interface>(iface: &I) -> String {
        let mut xml = String::new();
        iface.introspect_to_writer(&mut xml, 0);

        let mut out = String::new();
        let mut in_comment = false;
        for line in xml.lines() {
            let line = line.trim();
            if line.starts_with("<!--") {
                in_comment = true;
            }
            if in_comment {
                in_comment = !line.ends_with("-->");
                continue;
            }
            if !line.is_empty() {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    // Introspection reads no state and sends no requests, so an empty world and
    // a dangling sender are all these need.
    fn light_iface() -> Light1 {
        let (tx, _) = mpsc::channel(1);
        Light1::new(Arc::new(RwLock::new(World::default())), View::Group, tx)
    }

    fn manager_iface() -> Manager1 {
        let (tx, _) = mpsc::channel(1);
        Manager1 { world: Arc::new(RwLock::new(World::default())), requests: tx }
    }

    /// Rewrites the committed snapshots. Run deliberately, after an intended
    /// API change, and read the resulting diff before committing it:
    ///
    /// ```text
    /// cargo test -p gafferd -- --ignored regenerate_api_snapshots
    /// ```
    #[test]
    #[ignore = "generator: rewrites api/*.xml rather than checking them"]
    fn regenerate_api_snapshots() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("api");
        std::fs::create_dir_all(&dir).expect("create api dir");
        std::fs::write(dir.join("Light1.xml"), contract(&light_iface())).expect("write Light1");
        std::fs::write(dir.join("Manager1.xml"), contract(&manager_iface()))
            .expect("write Manager1");
    }

    /// The D-Bus API is the contract every client binds to — the CLI, a panel
    /// module, anything written against it. Nothing else in the build notices
    /// if a property is renamed or its signature changes, so this does.
    ///
    /// Adding a property or method is free; this only fails on a rename, a
    /// removal, or a type change. If it fails and the change was intended,
    /// regenerate the snapshot and say so in the commit message — a client
    /// somewhere needs updating too.
    #[test]
    fn the_light1_contract_is_unchanged() {
        assert_eq!(
            contract(&light_iface()),
            include_str!("../api/Light1.xml"),
            "io.mineiro.gaffer.Light1 changed shape; see the doc comment on this test"
        );
    }

    #[test]
    fn the_manager1_contract_is_unchanged() {
        assert_eq!(
            contract(&manager_iface()),
            include_str!("../api/Manager1.xml"),
            "io.mineiro.gaffer.Manager1 changed shape; see the doc comment on this test"
        );
    }

    #[test]
    fn object_paths_are_valid_dbus_elements() {
        // MAC separators are not legal in a path element.
        assert_eq!(light_path("00:00:5E:00:53:01"), "/io/mineiro/gaffer/lights/00005E005301");
        assert_eq!(light_path("00005e005301"), "/io/mineiro/gaffer/lights/00005E005301");
    }

    #[test]
    fn paths_are_stable_across_id_formatting() {
        // The same hardware must land on the same path however its id is
        // punctuated, or a client would see it vanish and reappear.
        assert_eq!(light_path("00:00:5E:00:53:01"), light_path("00-00-5e-00-53-01"));
    }

    #[test]
    fn a_light_path_never_collides_with_the_group() {
        assert_ne!(light_path("all"), GROUP_PATH);
    }

    /// A world whose lamp ids sort the *opposite* way to their names, so a
    /// stray sort cannot pass by coincidence. Documentation MACs per RFC 7042.
    fn two_lamps() -> World {
        let mut world = World::default();
        for (id, name) in [("00005E0053FF", "Key Light Left"), ("00005E005301", "Key Light Right")]
        {
            world.insert(crate::world::LightRecord {
                id: id.to_string(),
                fullname: format!("{name}._elg._tcp.local."),
                name: name.to_string(),
                model: String::new(),
                firmware: String::new(),
                endpoint: crate::world::Endpoint {
                    addr: std::net::IpAddr::from([192, 0, 2, 10]),
                    port: 9123,
                },
                desired: LightState { on: true, brightness: 40, kelvin: 4200 },
                reported: None,
                last_error: String::new(),
                dirty_since: None,
            });
        }
        world
    }

    #[test]
    fn the_first_selector_names_the_gangs_reference() {
        // `link left right` reads "link right onto left", so left leads —
        // regardless of how the hardware ids happen to sort. Left's id is the
        // *higher* of the two here, which is what a sort would get wrong.
        let world = two_lamps();
        let matched = matched_in_order(&world, &["left".to_string(), "right".to_string()]);
        assert_eq!(matched, vec!["00005E0053FF".to_string(), "00005E005301".to_string()]);
    }

    #[test]
    fn naming_the_lamps_the_other_way_round_builds_the_other_gang() {
        // The two orders must be distinguishable — this is the whole point.
        let world = two_lamps();
        let matched = matched_in_order(&world, &["right".to_string(), "left".to_string()]);
        assert_eq!(matched, vec!["00005E005301".to_string(), "00005E0053FF".to_string()]);
    }

    #[test]
    fn overlapping_selectors_keep_the_first_mention() {
        let world = two_lamps();
        let matched =
            matched_in_order(&world, &["left".to_string(), "key".to_string(), "left".to_string()]);
        assert_eq!(matched.len(), 2, "no duplicates: {matched:?}");
        assert_eq!(matched[0], "00005E0053FF", "and left still leads");
    }

    #[test]
    fn a_scene_name_cannot_smuggle_a_terminal_escape() {
        assert!(scene_name("studio\u{1b}[2Kfake").is_err());
        assert!(scene_name("studio\ntwo").is_err());
        assert!(scene_name("   ").is_err());
        assert!(scene_name(&"x".repeat(MAX_SCENE_NAME + 1)).is_err());
        // Surrounding whitespace is trimmed, not rejected.
        assert_eq!(scene_name("  on camera  ").unwrap(), "on camera");
        // Ordinary names with punctuation and non-ASCII are fine.
        assert_eq!(scene_name("gravação — 2ª câmera").unwrap(), "gravação — 2ª câmera");
    }
}
