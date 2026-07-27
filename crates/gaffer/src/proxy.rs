//! Typed client bindings for the gaffer D-Bus API.
//!
//! Reading the whole world goes through `org.freedesktop.DBus.ObjectManager`,
//! which returns every light and every property in a single round trip — and,
//! more usefully, tells us about lights appearing and disappearing without any
//! bespoke signal of gaffer's own.

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::stream::SelectAll;
use zbus::fdo::PropertiesChangedStream;
use zbus::zvariant::OwnedValue;

/// The well-known name the daemon owns.
pub const BUS_NAME: &str = "io.mineiro.gaffer";
/// Root object path.
pub const ROOT_PATH: &str = "/io/mineiro/gaffer";
/// Interface every light — and the group — implements.
pub const LIGHT_INTERFACE: &str = "io.mineiro.gaffer.Light1";

#[zbus::proxy(
    interface = "io.mineiro.gaffer.Manager1",
    default_service = "io.mineiro.gaffer",
    default_path = "/io/mineiro/gaffer"
)]
pub trait Manager {
    /// Apply value tokens to whatever the selector matches; returns matched ids.
    fn set_state(&self, selector: &str, tokens: Vec<String>) -> zbus::Result<Vec<String>>;

    /// Blink whatever the selector matches; returns matched ids.
    fn identify(&self, selector: &str) -> zbus::Result<Vec<String>>;

    /// Re-probe the network and re-read every light.
    fn rescan(&self) -> zbus::Result<()>;

    /// Gang the lights a selector matches, learning their current differences.
    fn link(&self, selectors: Vec<String>) -> zbus::Result<Vec<String>>;

    /// Break the gang a light belongs to; returns every light affected.
    fn unlink(&self, selector: &str) -> zbus::Result<Vec<String>>;

    /// Change how a gang tracks: `offset` or `mirror`.
    fn set_link_mode(&self, selector: &str, mode: &str) -> zbus::Result<()>;

    /// Gangs as `(mode, [(light id, brightness offset)])`.
    #[zbus(property)]
    fn links(&self) -> zbus::Result<Vec<(String, Vec<(String, i32)>)>>;

    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;
}

/// A snapshot of one light as the daemon sees it.
#[derive(Clone, Debug, Default)]
pub struct LightInfo {
    /// D-Bus object path, used to subscribe to this light's property changes.
    pub path: String,
    pub id: String,
    pub name: String,
    pub model: String,
    pub firmware: String,
    pub address: String,
    pub online: bool,
    pub online_count: u32,
    pub on: bool,
    pub brightness: u8,
    pub kelvin: u16,
    pub last_error: String,
    /// The gang this light belongs to, if any: mode plus every member's
    /// brightness offset from the gang's level.
    pub gang: Option<Gang>,
}

/// A gang as a client sees it.
#[derive(Clone, Debug)]
pub struct Gang {
    pub mode: String,
    /// The lamp whose values win when the gang is mirrored.
    pub reference: String,
    pub members: Vec<(String, i32)>,
}

impl Gang {
    /// This light's own offset from the gang's level.
    pub fn offset_of(&self, id: &str) -> i32 {
        self.members.iter().find(|(m, _)| m == id).map_or(0, |(_, o)| *o)
    }
}

impl LightInfo {
    /// The group object has no hardware id; that is what distinguishes it.
    pub fn is_group(&self) -> bool {
        self.id.is_empty()
    }

    /// One-word summary for display.
    pub fn state_word(&self) -> &'static str {
        if !self.online {
            "offline"
        } else if self.on {
            "on"
        } else {
            "off"
        }
    }
}

/// Connect to the session bus. D-Bus activation starts the daemon if needed.
pub async fn connect() -> Result<zbus::Connection> {
    zbus::Connection::session().await.context("connecting to the D-Bus session bus")
}

/// Fetch every light, group last.
pub async fn lights(connection: &zbus::Connection) -> Result<Vec<LightInfo>> {
    let object_manager = zbus::fdo::ObjectManagerProxy::builder(connection)
        .destination(BUS_NAME)?
        .path(ROOT_PATH)?
        .build()
        .await
        .context("building the object-manager proxy")?;

    let managed = object_manager
        .get_managed_objects()
        .await
        .with_context(|| format!("asking {BUS_NAME} for its lights (is gafferd able to start?)"))?;

    let mut lights: Vec<LightInfo> = managed
        .into_iter()
        .filter_map(|(path, interfaces)| {
            let properties = interfaces.get(LIGHT_INTERFACE)?;
            Some(from_properties(path.as_str(), properties))
        })
        .collect();

    // Gangs live on the manager, not on each light, so stitch them in here —
    // otherwise every consumer of this function would have to make the second
    // call and do the join itself.
    let gangs = ManagerProxy::new(connection).await?.links().await.unwrap_or_default();
    for light in &mut lights {
        light.gang = gangs
            .iter()
            .find(|(_, members)| members.iter().any(|(id, _)| *id == light.id))
            .map(|(mode, members)| Gang {
                mode: mode.clone(),
                // By contract the first member is the gang's reference.
                reference: members.first().map(|(id, _)| id.clone()).unwrap_or_default(),
                members: members.clone(),
            });
    }

    // Group last, then alphabetical: the individual lights are what you scan
    // for, and the summary reads better as a footer.
    lights.sort_by(|a, b| a.is_group().cmp(&b.is_group()).then_with(|| a.name.cmp(&b.name)));
    Ok(lights)
}

fn from_properties(path: &str, properties: &HashMap<String, OwnedValue>) -> LightInfo {
    LightInfo {
        path: path.to_string(),
        id: string(properties, "Id"),
        name: string(properties, "Name"),
        model: string(properties, "Model"),
        firmware: string(properties, "Firmware"),
        address: string(properties, "Address"),
        online: boolean(properties, "Online"),
        online_count: number(properties, "OnlineCount"),
        on: boolean(properties, "On"),
        brightness: number(properties, "Brightness"),
        kelvin: number(properties, "Kelvin"),
        last_error: string(properties, "LastError"),
        // Filled in by `lights()` once the gangs have been fetched.
        gang: None,
    }
}

fn string(properties: &HashMap<String, OwnedValue>, key: &str) -> String {
    properties.get(key).and_then(|value| String::try_from(value.clone()).ok()).unwrap_or_default()
}

fn boolean(properties: &HashMap<String, OwnedValue>, key: &str) -> bool {
    properties.get(key).and_then(|value| bool::try_from(value.clone()).ok()).unwrap_or_default()
}

fn number<T: TryFrom<OwnedValue> + Default>(
    properties: &HashMap<String, OwnedValue>,
    key: &str,
) -> T {
    properties.get(key).and_then(|value| T::try_from(value.clone()).ok()).unwrap_or_default()
}

/// Proxy for the daemon's object manager.
pub async fn object_manager(
    connection: &zbus::Connection,
) -> Result<zbus::fdo::ObjectManagerProxy<'static>> {
    zbus::fdo::ObjectManagerProxy::builder(connection)
        .destination(BUS_NAME)?
        .path(ROOT_PATH)?
        .build()
        .await
        .context("building the object-manager proxy")
}

/// A merged stream of `PropertiesChanged` for every light, group included.
///
/// Built from per-object `PropertiesProxy` streams rather than one raw match
/// rule: zbus routes proxy subscriptions itself, and this keeps the signal
/// plumbing to well-trodden API.
pub async fn property_changes(
    connection: &zbus::Connection,
    lights: &[LightInfo],
) -> Result<SelectAll<PropertiesChangedStream>> {
    let mut merged = SelectAll::new();

    for light in lights {
        let properties = zbus::fdo::PropertiesProxy::builder(connection)
            .destination(BUS_NAME)?
            .path(light.path.clone())?
            .build()
            .await
            .with_context(|| format!("watching {}", light.path))?;
        merged.push(properties.receive_properties_changed().await?);
    }

    Ok(merged)
}
