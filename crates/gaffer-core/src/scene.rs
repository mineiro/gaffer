//! Named looks you return to.
//!
//! A scene stores **topology, not per-lamp values** — the gangs, their modes
//! and offsets, and each gang's level. That is not a stylistic choice: replaying
//! absolute brightnesses into a live gang is ill-defined, because every write
//! fans out. Setting A re-derives the level and drags B; setting B then does it
//! again. The result is last-write-wins with the others sitting at
//! `level + offset`, not at the values that were stored. Writing each lamp
//! *alone* avoids the fan-out but re-learns the offsets, so the ratio is lost
//! either way — and neither approach can restore membership or mode at all.
//!
//! The two schemes are not rivals on storage: `level + offset_i == brightness_i`
//! carries the same information. The difference is only what apply restores, and
//! topology is the part that cannot be reconstructed.
//!
//! # Scope
//!
//! Applying is bounded to the lamps a scene names, with one unavoidable
//! exception. Gang membership is symmetric, so a named lamp cannot leave a gang
//! without affecting the lamps it leaves behind. The rule:
//!
//! * Named lamps are removed from whatever gang they are in.
//! * The remnant survives at two or more members, and dissolves otherwise.
//! * Unnamed lamps are never re-ganged and never moved.
//!
//! Because offsets are stored against a notional level, dropping a member needs
//! no re-basing and changes nobody's brightness.

use std::collections::{BTreeMap, BTreeSet};

use crate::link::{Link, LinkMode};
use crate::state::LightState;

/// A gang, as a scene remembers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneGang {
    /// Members in order; the first is the reference whose values win a mirror.
    pub members: Vec<String>,
    pub mode: LinkMode,
    /// Each member's brightness offset from the gang's level.
    pub offsets: BTreeMap<String, i32>,
    /// The gang's position as one fader. Deliberately signed and unclamped, so
    /// a gang compressed against the ceiling restores exactly as captured.
    pub level: i32,
    /// Temperature and power mirror across a gang, so one each is enough.
    pub kelvin: u16,
    pub on: bool,
}

/// A lamp that belongs to no gang.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneLamp {
    pub id: String,
    pub brightness: u8,
    pub kelvin: u16,
    pub on: bool,
}

/// A named look.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scene {
    pub gangs: Vec<SceneGang>,
    pub lamps: Vec<SceneLamp>,
}

/// What applying a scene requires, in the order it must happen.
///
/// Topology first, then values — so the level write fans out through the gang
/// that was just re-formed and the offsets land exactly as captured. Doing it
/// the other way round would write values into whatever gang happened to exist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScenePlan {
    /// Lamps to remove from whatever gang they are currently in.
    pub detach: Vec<String>,
    /// Gangs to form. Members are ordered; the first is the reference.
    pub form: Vec<SceneGang>,
    /// Absolute states for lamps the scene names individually.
    pub lamps: BTreeMap<String, LightState>,
}

impl Scene {
    /// Capture the current look of the given lamps.
    ///
    /// `links` supplies whatever gangs exist; a lamp in one is captured as part
    /// of that gang, and only gangs with two or more captured members are kept
    /// — a gang the scene half-covers would be neither reproducible nor honest.
    pub fn capture(states: &BTreeMap<String, LightState>, links: &[Link]) -> Self {
        let mut scene = Scene::default();
        let mut spoken_for: BTreeSet<String> = BTreeSet::new();

        for link in links {
            let members: Vec<String> =
                link.members().filter(|m| states.contains_key(m.as_str())).cloned().collect();
            if members.len() < 2 {
                continue;
            }

            // The reference leads, so the ordering that decides a mirror
            // survives into storage.
            let reference = link.reference().to_string();
            let mut ordered = vec![reference.clone()];
            ordered.extend(members.iter().filter(|m| **m != reference).cloned());

            let Some(level) = link.level(states) else { continue };
            let Some(template) = ordered.first().and_then(|m| states.get(m)).copied() else {
                continue;
            };

            let offsets: BTreeMap<String, i32> =
                ordered.iter().filter_map(|m| link.offset_of(m).map(|o| (m.clone(), o))).collect();
            spoken_for.extend(ordered.iter().cloned());

            scene.gangs.push(SceneGang {
                members: ordered,
                mode: link.mode,
                offsets,
                level,
                kelvin: template.kelvin,
                on: template.on,
            });
        }

        for (id, state) in states {
            if spoken_for.contains(id) {
                continue;
            }
            scene.lamps.push(SceneLamp {
                id: id.clone(),
                brightness: state.brightness,
                kelvin: state.kelvin,
                on: state.on,
            });
        }

        scene
    }

    /// Every lamp this scene names.
    pub fn names(&self) -> BTreeSet<String> {
        self.gangs
            .iter()
            .flat_map(|gang| gang.members.iter().cloned())
            .chain(self.lamps.iter().map(|lamp| lamp.id.clone()))
            .collect()
    }

    /// Work out what applying this scene requires.
    ///
    /// `present` is the set of lamps that actually exist right now. A missing
    /// member is simply left out of the gang that gets formed, while its stored
    /// offset is preserved in the scene — so the gang re-forms correctly when
    /// the lamp comes back.
    pub fn plan(&self, present: &BTreeSet<String>) -> ScenePlan {
        // Every named lamp leaves its current gang first. Whether it was in one
        // is the caller's business; detaching a lamp that is not ganged is a
        // no-op, and asking here would need world state this type does not have.
        let mut plan = ScenePlan {
            detach: self.names().into_iter().filter(|id| present.contains(id)).collect(),
            ..ScenePlan::default()
        };

        for gang in &self.gangs {
            let members: Vec<String> =
                gang.members.iter().filter(|m| present.contains(*m)).cloned().collect();
            if members.len() < 2 {
                // Not enough of the gang is here to form it. The remaining lamp
                // still takes the scene's values, below.
                if let Some(only) = members.first() {
                    let offset = gang.offsets.get(only).copied().unwrap_or(0);
                    plan.lamps.insert(
                        only.clone(),
                        LightState {
                            on: gang.on,
                            brightness: clamp_u8(gang.level + offset),
                            kelvin: gang.kelvin,
                        },
                    );
                }
                continue;
            }
            plan.form.push(SceneGang { members, ..gang.clone() });
        }

        for lamp in &self.lamps {
            if !present.contains(&lamp.id) {
                continue;
            }
            plan.lamps.insert(
                lamp.id.clone(),
                LightState { on: lamp.on, brightness: lamp.brightness, kelvin: lamp.kelvin },
            );
        }

        plan
    }
}

impl SceneGang {
    /// The link this gang describes, ready to be installed.
    pub fn to_link(&self) -> Link {
        Link::from_parts(
            self.mode,
            self.members.first().cloned(),
            self.members
                .iter()
                .filter_map(|m| self.offsets.get(m).map(|o| (m.clone(), *o)))
                .collect(),
        )
    }

    /// The state to drive the formed gang to, as a template for its level.
    pub fn template(&self) -> LightState {
        LightState { on: self.on, brightness: clamp_u8(self.level), kelvin: self.kelvin }
    }
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, i32::from(crate::state::MAX_BRIGHTNESS)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(brightness: u8) -> LightState {
        LightState { on: true, brightness, kelvin: 4200 }
    }

    fn states(pairs: &[(&str, u8)]) -> BTreeMap<String, LightState> {
        pairs.iter().map(|(id, b)| ((*id).to_string(), state(*b))).collect()
    }

    fn present(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    fn keys_link() -> Link {
        Link::learn(&[("left".to_string(), state(42)), ("right".to_string(), state(35))])
    }

    #[test]
    fn a_gang_is_captured_as_topology_not_as_two_brightnesses() {
        let scene = Scene::capture(&states(&[("left", 42), ("right", 35)]), &[keys_link()]);

        assert_eq!(scene.gangs.len(), 1);
        assert!(scene.lamps.is_empty(), "ganged lamps are not also stored individually");

        let gang = &scene.gangs[0];
        assert_eq!(gang.members, vec!["left".to_string(), "right".to_string()]);
        assert_eq!(gang.level, 42);
        assert_eq!(gang.offsets["right"], -7);
        assert_eq!(gang.mode, LinkMode::Offset);
    }

    #[test]
    fn the_reference_leads_so_a_mirror_stays_reproducible() {
        // `link right left` makes right the reference; capture must preserve it.
        let link =
            Link::learn(&[("right".to_string(), state(35)), ("left".to_string(), state(42))]);
        let scene = Scene::capture(&states(&[("left", 42), ("right", 35)]), &[link]);
        assert_eq!(scene.gangs[0].members.first().unwrap(), "right");
        assert_eq!(scene.gangs[0].to_link().reference(), "right");
    }

    #[test]
    fn an_ungangeed_lamp_is_captured_with_absolute_values() {
        let scene = Scene::capture(&states(&[("mini", 64)]), &[]);
        assert!(scene.gangs.is_empty());
        assert_eq!(
            scene.lamps[0],
            SceneLamp { id: "mini".into(), brightness: 64, kelvin: 4200, on: true }
        );
    }

    #[test]
    fn applying_restores_the_level_and_offsets_exactly() {
        let scene = Scene::capture(&states(&[("left", 42), ("right", 35)]), &[keys_link()]);
        let plan = scene.plan(&present(&["left", "right"]));

        assert_eq!(plan.form.len(), 1);
        let gang = &plan.form[0];
        let resolved = gang.to_link().resolve_from_level(gang.level, gang.template());
        assert_eq!(resolved["left"].brightness, 42);
        assert_eq!(resolved["right"].brightness, 35);
    }

    #[test]
    fn a_level_beyond_the_ceiling_survives_a_round_trip() {
        // A gang compressed against the top keeps its spacing, because the
        // stored level is signed and unclamped.
        let mut link = keys_link();
        link.relearn("right", 100, &states(&[("left", 42)])); // right +58
        let captured = Scene::capture(&states(&[("left", 42), ("right", 100)]), &[link]);
        let gang = &captured.gangs[0];

        let resolved = gang.to_link().resolve_from_level(gang.level, gang.template());
        assert_eq!(resolved["left"].brightness, 42);
        assert_eq!(resolved["right"].brightness, 100);
    }

    #[test]
    fn every_named_lamp_is_detached_before_anything_is_formed() {
        let scene =
            Scene::capture(&states(&[("left", 42), ("right", 35), ("mini", 64)]), &[keys_link()]);
        let plan = scene.plan(&present(&["left", "right", "mini"]));

        assert_eq!(plan.detach, vec!["left".to_string(), "mini".to_string(), "right".to_string()]);
        assert_eq!(scene.names().len(), 3);
    }

    #[test]
    fn a_lamp_the_scene_never_mentions_is_absent_from_the_plan() {
        // The bound that keeps applying a scene about "keys" from touching the
        // rest of the desk.
        let scene = Scene::capture(&states(&[("left", 42), ("right", 35)]), &[keys_link()]);
        let plan = scene.plan(&present(&["left", "right", "stranger"]));

        assert!(!plan.detach.contains(&"stranger".to_string()));
        assert!(!plan.lamps.contains_key("stranger"));
        assert!(plan.form.iter().all(|g| !g.members.contains(&"stranger".to_string())));
    }

    #[test]
    fn a_missing_member_leaves_the_gang_unformed_but_still_lights_the_survivor() {
        let scene = Scene::capture(&states(&[("left", 42), ("right", 35)]), &[keys_link()]);
        let plan = scene.plan(&present(&["left"]));

        assert!(plan.form.is_empty(), "two members are needed to form a gang");
        assert_eq!(plan.lamps["left"].brightness, 42, "the survivor still takes its value");
        // The scene itself is untouched, so the gang re-forms when right returns.
        assert_eq!(scene.gangs[0].offsets["right"], -7);
    }

    #[test]
    fn a_scene_captured_and_replanned_is_stable() {
        let before = states(&[("left", 42), ("right", 35), ("mini", 64)]);
        let scene = Scene::capture(&before, &[keys_link()]);
        let plan = scene.plan(&present(&["left", "right", "mini"]));

        let mut after: BTreeMap<String, LightState> = plan.lamps.clone();
        for gang in &plan.form {
            after.extend(gang.to_link().resolve_from_level(gang.level, gang.template()));
        }
        assert_eq!(after, before, "applying a scene captured from a desk must not move it");
    }

    #[test]
    fn a_half_covered_gang_is_not_captured_as_a_gang() {
        // Only one member is in the capture set, so storing it as a gang would
        // promise a topology the scene cannot reproduce.
        let scene = Scene::capture(&states(&[("left", 42)]), &[keys_link()]);
        assert!(scene.gangs.is_empty());
        assert_eq!(scene.lamps.len(), 1);
        assert_eq!(scene.lamps[0].brightness, 42);
    }

    #[test]
    fn temperature_and_power_are_stored_once_per_gang() {
        let dark = BTreeMap::from([
            ("left".to_string(), LightState { on: false, brightness: 42, kelvin: 2900 }),
            ("right".to_string(), LightState { on: false, brightness: 35, kelvin: 2900 }),
        ]);
        let scene = Scene::capture(&dark, &[keys_link()]);
        assert_eq!(scene.gangs[0].kelvin, 2900);
        assert!(!scene.gangs[0].on);

        let gang = &scene.gangs[0];
        let resolved = gang.to_link().resolve_from_level(gang.level, gang.template());
        assert!(resolved.values().all(|s| !s.on && s.kelvin == 2900));
    }
}
