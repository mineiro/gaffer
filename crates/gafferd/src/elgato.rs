//! The Elgato Key Light HTTP protocol.
//!
//! Plain HTTP on port 9123, no authentication, no TLS. Four endpoints:
//!
//! | Method | Path | Purpose |
//! |--------|------|---------|
//! | `GET`  | `/elgato/lights` | read current state |
//! | `PUT`  | `/elgato/lights` | set state; the response echoes the result |
//! | `GET`  | `/elgato/accessory-info` | display name, model, firmware |
//! | `POST` | `/elgato/identify` | physically blink the unit |
//!
//! Brightness is 0–100. Temperature is **mireds, not Kelvin** — the conversion
//! lives in `gaffer_core::color` and is the single most easily-broken detail in
//! the whole protocol.

use std::time::Duration;

use anyhow::{Context, Result};
use gaffer_core::{LightState, kelvin_to_mired, mired_to_kelvin};
use serde::{Deserialize, Serialize};

/// Per-request timeout, matching the value both earlier implementations used.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// State read back from a light. Fields are optional because firmware omits
/// what it does not know, and treating a missing `on` as `false` would silently
/// turn lights off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PartialState {
    pub on: Option<bool>,
    pub brightness: Option<u8>,
    pub kelvin: Option<u16>,
}

impl PartialState {
    /// Overlay whatever the device reported onto a known-complete state.
    pub fn merge_into(self, base: LightState) -> LightState {
        LightState {
            on: self.on.unwrap_or(base.on),
            brightness: self.brightness.unwrap_or(base.brightness),
            kelvin: self.kelvin.unwrap_or(base.kelvin),
        }
        .clamped()
    }
}

/// Identity and firmware details.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessoryInfo {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub product_name: String,
    #[serde(default)]
    pub firmware_version: String,
}

#[derive(Debug, Deserialize)]
struct LightsEnvelope {
    #[serde(default)]
    lights: Vec<LightEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct LightEntry {
    on: Option<u8>,
    brightness: Option<u8>,
    temperature: Option<u16>,
}

impl From<LightEntry> for PartialState {
    fn from(entry: LightEntry) -> Self {
        Self {
            on: entry.on.map(|value| value != 0),
            brightness: entry.brightness,
            kelvin: entry.temperature.map(mired_to_kelvin),
        }
    }
}

#[derive(Debug, Serialize)]
struct PutEnvelope {
    #[serde(rename = "numberOfLights")]
    number_of_lights: u32,
    lights: [PutEntry; 1],
}

#[derive(Debug, Serialize)]
struct PutEntry {
    on: u8,
    brightness: u8,
    temperature: u16,
}

impl PutEnvelope {
    fn single(state: LightState) -> Self {
        Self {
            number_of_lights: 1,
            lights: [PutEntry {
                on: u8::from(state.on),
                brightness: state.brightness,
                temperature: kelvin_to_mired(state.kelvin),
            }],
        }
    }
}

/// Build the HTTP client the daemon shares across every light.
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("gafferd/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the HTTP client")
}

/// Read a light's current state.
pub async fn get_state(client: &reqwest::Client, base_url: &str) -> Result<PartialState> {
    let body: LightsEnvelope = client
        .get(format!("{base_url}/elgato/lights"))
        .send()
        .await
        .context("requesting light state")?
        .error_for_status()
        .context("light rejected the state request")?
        .json()
        .await
        .context("parsing light state")?;

    Ok(first_light(body))
}

/// Push a complete state, returning whatever the light echoes back.
pub async fn put_state(
    client: &reqwest::Client,
    base_url: &str,
    state: LightState,
) -> Result<PartialState> {
    let body: LightsEnvelope = client
        .put(format!("{base_url}/elgato/lights"))
        .json(&PutEnvelope::single(state))
        .send()
        .await
        .context("pushing light state")?
        .error_for_status()
        .context("light rejected the state push")?
        .json()
        .await
        .context("parsing the echoed state")?;

    Ok(first_light(body))
}

/// Read identity and firmware details. Best-effort; failure is not fatal.
pub async fn get_info(client: &reqwest::Client, base_url: &str) -> Result<AccessoryInfo> {
    client
        .get(format!("{base_url}/elgato/accessory-info"))
        .send()
        .await
        .context("requesting accessory info")?
        .error_for_status()
        .context("light rejected the accessory-info request")?
        .json()
        .await
        .context("parsing accessory info")
}

/// Ask the light to physically identify itself.
pub async fn identify(client: &reqwest::Client, base_url: &str) -> Result<()> {
    client
        .post(format!("{base_url}/elgato/identify"))
        .header("content-type", "application/json")
        .body("")
        .send()
        .await
        .context("sending identify")?
        .error_for_status()
        .context("light rejected identify")?;
    Ok(())
}

fn first_light(envelope: LightsEnvelope) -> PartialState {
    envelope.lights.into_iter().next().map(PartialState::from).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> PartialState {
        first_light(serde_json::from_str(json).expect("valid json"))
    }

    #[test]
    fn a_normal_response_parses_and_converts_to_kelvin() {
        let state =
            parse(r#"{"numberOfLights":1,"lights":[{"on":1,"brightness":75,"temperature":213}]}"#);
        assert_eq!(state.on, Some(true));
        assert_eq!(state.brightness, Some(75));
        assert_eq!(state.kelvin, Some(mired_to_kelvin(213)));
    }

    #[test]
    fn zero_means_off_rather_than_missing() {
        let state = parse(r#"{"lights":[{"on":0,"brightness":0,"temperature":344}]}"#);
        assert_eq!(state.on, Some(false));
        assert_eq!(state.brightness, Some(0));
    }

    #[test]
    fn missing_fields_stay_missing_instead_of_defaulting_to_off() {
        // The dangerous case: `{"on": ...}` absent must not read as "turn off".
        let state = parse(r#"{"lights":[{"brightness":50}]}"#);
        assert_eq!(state.on, None);
        assert_eq!(state.brightness, Some(50));
        assert_eq!(state.kelvin, None);

        let base = LightState { on: true, brightness: 10, kelvin: 4000 };
        assert_eq!(state.merge_into(base), LightState { on: true, brightness: 50, kelvin: 4000 });
    }

    #[test]
    fn an_empty_light_list_yields_nothing_to_merge() {
        let state = parse(r#"{"numberOfLights":0,"lights":[]}"#);
        assert_eq!(state, PartialState::default());

        let base = LightState { on: true, brightness: 42, kelvin: 4200 };
        assert_eq!(state.merge_into(base), base, "an empty response must not disturb state");
    }

    #[test]
    fn a_response_without_a_lights_key_does_not_fail() {
        assert_eq!(parse(r#"{"numberOfLights":0}"#), PartialState::default());
    }

    #[test]
    fn only_the_first_light_is_used() {
        // Key Lights report one; a strip reporting several is out of scope, and
        // silently averaging them would be worse than ignoring the extras.
        let state = parse(r#"{"lights":[{"brightness":10},{"brightness":90}]}"#);
        assert_eq!(state.brightness, Some(10));
    }

    #[test]
    fn the_put_body_matches_the_documented_wire_format() {
        let state = LightState { on: true, brightness: 42, kelvin: 4200 };
        let json = serde_json::to_value(PutEnvelope::single(state)).unwrap();

        assert_eq!(json["numberOfLights"], 1);
        assert_eq!(json["lights"][0]["on"], 1);
        assert_eq!(json["lights"][0]["brightness"], 42);
        assert_eq!(json["lights"][0]["temperature"], 238, "must be mireds, not Kelvin");
    }

    #[test]
    fn power_is_serialised_as_an_integer_not_a_bool() {
        let off = LightState { on: false, brightness: 0, kelvin: 4000 };
        let json = serde_json::to_value(PutEnvelope::single(off)).unwrap();
        assert_eq!(json["lights"][0]["on"], 0);
        assert!(json["lights"][0]["on"].is_number());
    }

    #[test]
    fn accessory_info_tolerates_missing_fields() {
        let info: AccessoryInfo = serde_json::from_str(r#"{"displayName":"Left"}"#).unwrap();
        assert_eq!(info.display_name, "Left");
        assert!(info.product_name.is_empty());
        assert!(info.firmware_version.is_empty());
    }

    #[test]
    fn a_state_pushed_then_echoed_round_trips_without_drift() {
        // The reconciler adopts the echoed state as the new desired state, so a
        // value that shifts on every cycle would oscillate forever.
        let mut state = LightState { on: true, brightness: 42, kelvin: 4200 };
        for _ in 0..10 {
            let wire = kelvin_to_mired(state.kelvin);
            let echoed = PartialState {
                on: Some(true),
                brightness: Some(42),
                kelvin: Some(mired_to_kelvin(wire)),
            };
            let next = echoed.merge_into(state);
            if next == state {
                return; // converged
            }
            state = next;
        }
        panic!("state never converged: {state:?}");
    }
}
