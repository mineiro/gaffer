//! Persisting user intent across restarts.
//!
//! Only things the *user* decided live here: which lamps are ganged and how.
//! Discovered facts — addresses, firmware, whether a light answered — are not
//! configuration and are re-learned every run, so nothing in this file can go
//! stale in a way that matters.
//!
//! The daemon writes this file, and it is meant to stay legible enough to edit
//! by hand. Serialisation lives here rather than in `gaffer-core` so that crate
//! keeps its zero-dependency property.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gaffer_core::{Link, LinkMode, Scene, SceneGang, SceneLamp};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// On-disk shape. Kept deliberately close to the TOML a person would write.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default, rename = "link", skip_serializing_if = "Vec::is_empty")]
    links: Vec<StoredLink>,
    #[serde(default, rename = "scene", skip_serializing_if = "Vec::is_empty")]
    scenes: Vec<StoredScene>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredLink {
    /// `offset` (members keep their difference) or `mirror` (identical values).
    mode: String,
    /// The member whose values win when mirrored — the lamp named first when
    /// the gang was made. Optional so older files still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
    /// Member id → brightness offset from the gang's level.
    offsets: BTreeMap<String, i32>,
}

/// A saved scene: what the desk looked like, as topology plus values.
///
/// Deliberately not a list of per-lamp brightnesses. Storing the gang and its
/// level means a scene restores an *instrument* — reload it after alt-dragging
/// the spacing and the spacing comes back too.
#[derive(Debug, Deserialize, Serialize)]
struct StoredScene {
    name: String,
    #[serde(default, rename = "gang", skip_serializing_if = "Vec::is_empty")]
    gangs: Vec<StoredSceneGang>,
    #[serde(default, rename = "lamp", skip_serializing_if = "Vec::is_empty")]
    lamps: Vec<StoredSceneLamp>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredSceneGang {
    /// Ordered; the first is the gang's reference.
    members: Vec<String>,
    mode: String,
    /// Signed and unclamped — the gang's position as one fader. A level below
    /// zero or above maximum is normal and load-bearing: it is what lets a gang
    /// compress against the end of its travel and come back out again.
    level: i32,
    kelvin: u16,
    on: bool,
    offsets: BTreeMap<String, i32>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredSceneLamp {
    id: String,
    brightness: u8,
    kelvin: u16,
    on: bool,
}

impl Config {
    /// `$XDG_CONFIG_HOME/gaffer/config.toml`, falling back to `~/.config`.
    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("gaffer").join("config.toml"))
    }

    /// Read the config, or a default if it does not exist.
    ///
    /// A malformed file is reported and then ignored rather than being fatal: a
    /// daemon that refuses to start because one gang was mistyped is worse than
    /// one that starts without it and says so.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "no config yet");
                return Self::default();
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "could not read config; continuing without it");
                return Self::default();
            }
        };

        match toml::from_str(&text) {
            Ok(config) => config,
            Err(error) => {
                warn!(path = %path.display(), %error, "config is malformed; continuing without it");
                Self::default()
            }
        }
    }

    /// Write the config atomically.
    ///
    /// Via a temporary file and a rename, so a crash mid-write cannot leave a
    /// half-written file that fails to parse on the next start.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }

        let body = format!(
            "# gaffer — written by the daemon, safe to edit by hand.\n\
             # Gangs: which lamps move together, and the brightness difference\n\
             # each keeps from the gang's level. Scenes: whole-desk snapshots,\n\
             # stored as that same topology rather than as flat brightnesses.\n\n{}",
            toml::to_string_pretty(self).context("serialising config")?
        );

        let temp = path.with_extension("toml.new");
        std::fs::write(&temp, body).with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Links as the daemon understands them.
    ///
    /// Entries with fewer than two members are dropped — a gang of one is not a
    /// gang, and a hand-edited file is the likely source of one.
    pub fn links(&self) -> Vec<Link> {
        self.links
            .iter()
            .filter(|stored| stored.offsets.len() >= 2)
            .map(|stored| {
                let mode = mode_from(&stored.mode);
                Link::from_parts(mode, stored.reference.clone(), stored.offsets.clone())
            })
            .collect()
    }

    /// Replace the stored links.
    pub fn set_links<'a>(&mut self, links: impl Iterator<Item = &'a Link>) {
        self.links = links
            .map(|link| StoredLink {
                mode: link.mode.as_str().to_string(),
                reference: Some(link.reference().to_string()),
                offsets: link.offsets().map(|(id, offset)| (id.clone(), offset)).collect(),
            })
            .collect();
    }

    /// Saved scenes, by name.
    ///
    /// Unnamed entries are dropped — a scene nobody can ask for is only a way
    /// for a hand-edited file to grow silent clutter.
    pub fn scenes(&self) -> BTreeMap<String, Scene> {
        self.scenes
            .iter()
            .filter(|stored| !stored.name.trim().is_empty())
            .map(|stored| {
                let scene = Scene {
                    gangs: stored
                        .gangs
                        .iter()
                        // Same rule as links: a gang of one is not a gang.
                        .filter(|gang| gang.members.len() >= 2)
                        .map(|gang| SceneGang {
                            members: gang.members.clone(),
                            mode: mode_from(&gang.mode),
                            offsets: gang.offsets.clone(),
                            level: gang.level,
                            kelvin: gang.kelvin,
                            on: gang.on,
                        })
                        .collect(),
                    lamps: stored
                        .lamps
                        .iter()
                        .map(|lamp| SceneLamp {
                            id: lamp.id.clone(),
                            brightness: lamp.brightness,
                            kelvin: lamp.kelvin,
                            on: lamp.on,
                        })
                        .collect(),
                };
                (stored.name.clone(), scene)
            })
            .collect()
    }

    /// Replace the stored scenes.
    pub fn set_scenes(&mut self, scenes: &BTreeMap<String, Scene>) {
        self.scenes = scenes
            .iter()
            .map(|(name, scene)| StoredScene {
                name: name.clone(),
                gangs: scene
                    .gangs
                    .iter()
                    .map(|gang| StoredSceneGang {
                        members: gang.members.clone(),
                        mode: gang.mode.as_str().to_string(),
                        level: gang.level,
                        kelvin: gang.kelvin,
                        on: gang.on,
                        offsets: gang.offsets.clone(),
                    })
                    .collect(),
                lamps: scene
                    .lamps
                    .iter()
                    .map(|lamp| StoredSceneLamp {
                        id: lamp.id.clone(),
                        brightness: lamp.brightness,
                        kelvin: lamp.kelvin,
                        on: lamp.on,
                    })
                    .collect(),
            })
            .collect();
    }
}

/// An unrecognised mode reads as offset — the resting state — rather than
/// costing the gang. Shared by links and scenes so a typo behaves the same in
/// both.
fn mode_from(mode: &str) -> LinkMode {
    match mode {
        "mirror" => LinkMode::Mirror,
        _ => LinkMode::Offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link_of(pairs: &[(&str, i32)], mode: LinkMode) -> Link {
        Link::from_parts(
            mode,
            pairs.first().map(|(id, _)| (*id).to_string()),
            pairs.iter().map(|(id, o)| ((*id).to_string(), *o)).collect(),
        )
    }

    #[test]
    fn a_gang_survives_a_round_trip() {
        let mut config = Config::default();
        let link = link_of(&[("left", 0), ("right", -7)], LinkMode::Offset);
        config.set_links([&link].into_iter());

        let text = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&text).unwrap();

        assert_eq!(back.links(), vec![link]);
    }

    #[test]
    fn the_written_form_is_legible() {
        let mut config = Config::default();
        config.set_links([&link_of(&[("left", 0), ("right", -7)], LinkMode::Offset)].into_iter());

        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("mode = \"offset\""), "{text}");
        assert!(text.contains("right = -7"), "{text}");
    }

    #[test]
    fn mirror_mode_round_trips_distinctly_from_offset() {
        let mut config = Config::default();
        config.set_links([&link_of(&[("a", 0), ("b", 0)], LinkMode::Mirror)].into_iter());
        let back: Config = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(back.links()[0].mode, LinkMode::Mirror);
    }

    #[test]
    fn an_unknown_mode_reads_as_offset_rather_than_failing() {
        // Offset is the resting state; a typo should not cost the gang.
        let config: Config =
            toml::from_str("[[link]]\nmode = \"nonsense\"\n[link.offsets]\na = 0\nb = -7\n")
                .unwrap();
        assert_eq!(config.links()[0].mode, LinkMode::Offset);
    }

    #[test]
    fn a_gang_of_one_is_discarded() {
        let config: Config =
            toml::from_str("[[link]]\nmode = \"offset\"\n[link.offsets]\nonly = 0\n").unwrap();
        assert!(config.links().is_empty());
    }

    #[test]
    fn an_empty_config_has_no_links_and_writes_nothing_extra() {
        let config = Config::default();
        assert!(config.links().is_empty());
        assert_eq!(toml::to_string_pretty(&config).unwrap().trim(), "");
    }

    #[test]
    fn a_malformed_file_is_survivable() {
        let dir = std::env::temp_dir().join(format!("gaffer-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not toml {{{").unwrap();

        // Must not panic, and must not take the daemon down.
        assert!(Config::load(&path).links().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saving_then_loading_preserves_the_gang() {
        let dir = std::env::temp_dir().join(format!("gaffer-cfg-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let link = link_of(&[("left", 0), ("right", -7)], LinkMode::Offset);
        let mut config = Config::default();
        config.set_links([&link].into_iter());
        config.save(&path).unwrap();

        assert_eq!(Config::load(&path).links(), vec![link]);
        // The atomic write must not leave its temporary behind.
        assert!(!path.with_extension("toml.new").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn scene_of() -> Scene {
        Scene {
            gangs: vec![SceneGang {
                members: vec!["left".into(), "right".into()],
                mode: LinkMode::Offset,
                offsets: BTreeMap::from([("left".into(), 0), ("right".into(), -7)]),
                level: 42,
                kelvin: 4200,
                on: true,
            }],
            lamps: vec![SceneLamp { id: "mini".into(), brightness: 20, kelvin: 5000, on: false }],
        }
    }

    #[test]
    fn a_scene_survives_a_round_trip() {
        let mut config = Config::default();
        config.set_scenes(&BTreeMap::from([("on camera".to_string(), scene_of())]));

        let back: Config = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(back.scenes(), BTreeMap::from([("on camera".to_string(), scene_of())]));
    }

    #[test]
    fn a_scene_level_outside_the_slider_survives_intact() {
        // A gang compressed against the end of its travel has a level below
        // zero. Clamping it on the way to disk would flatten the spacing on the
        // way back, so the stored level has to stay signed and unclamped.
        let mut scene = scene_of();
        scene.gangs[0].level = -23;
        let mut config = Config::default();
        config.set_scenes(&BTreeMap::from([("dim".to_string(), scene)]));

        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("level = -23"), "{text}");
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.scenes()["dim"].gangs[0].level, -23);
    }

    #[test]
    fn scenes_and_gangs_share_one_file_without_disturbing_each_other() {
        let dir = std::env::temp_dir().join(format!("gaffer-cfg-sc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let link = link_of(&[("left", 0), ("right", -7)], LinkMode::Offset);
        let mut config = Config::default();
        config.set_links([&link].into_iter());
        config.set_scenes(&BTreeMap::from([("on camera".to_string(), scene_of())]));
        config.save(&path).unwrap();

        let back = Config::load(&path);
        assert_eq!(back.links(), vec![link]);
        assert_eq!(back.scenes().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_nameless_scene_is_discarded() {
        let config: Config = toml::from_str("[[scene]]\nname = \"  \"\n").unwrap();
        assert!(config.scenes().is_empty());
    }

    #[test]
    fn an_empty_scene_is_still_a_scene() {
        // Saving a desk with no lights is odd but not wrong, and dropping it
        // would make the name disappear with no explanation.
        let config: Config = toml::from_str("[[scene]]\nname = \"empty\"\n").unwrap();
        assert_eq!(config.scenes().len(), 1);
    }
}
