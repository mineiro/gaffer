//! Output formats.
//!
//! Three consumers, three shapes: a human at a terminal, `jq` in a script, and
//! Waybar's `custom/` module — which reads newline-delimited JSON from a
//! subprocess's stdout. That last one is why `gaffer watch --waybar` exists at
//! all: on Wayland there is no system tray to put a light control in, so the
//! panel integration is a program that prints JSON forever.

use serde_json::{Value, json};

use crate::proxy::LightInfo;

/// Aligned table for a terminal.
pub fn table(lights: &[LightInfo]) -> String {
    if lights.is_empty() {
        return "no lights found\n".to_string();
    }

    let name_width = lights.iter().map(|light| light.name.len()).max().unwrap_or(4).max(4);
    let mut out = format!(
        "{:<name_width$}  {:<7}  {:>6}  {:>6}  {}\n",
        "NAME", "STATE", "BRIGHT", "TEMP", "ADDRESS"
    );

    for light in lights {
        let address = if light.is_group() {
            format!("{} of {} online", light.online_count, lights.len().saturating_sub(1))
        } else if light.last_error.is_empty() {
            light.address.clone()
        } else {
            // A failing light is the case you most want explained in place.
            format!("{}  ({})", light.address, first_line(&light.last_error))
        };

        out.push_str(&format!(
            "{:<name_width$}  {:<7}  {:>5}%  {:>5}K  {}\n",
            light.name,
            light.state_word(),
            light.brightness,
            light.kelvin,
            address
        ));
    }

    out
}

/// Machine-readable form of the whole world.
pub fn json(lights: &[LightInfo]) -> String {
    let payload: Vec<Value> = lights
        .iter()
        .map(|light| {
            json!({
                "id": light.id,
                "name": light.name,
                "model": light.model,
                "firmware": light.firmware,
                "address": light.address,
                "online": light.online,
                "on": light.on,
                "brightness": light.brightness,
                "kelvin": light.kelvin,
                "group": light.is_group(),
                "lastError": light.last_error,
            })
        })
        .collect();

    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "[]".to_string())
}

/// One line of Waybar's `custom/` module protocol.
pub fn waybar(lights: &[LightInfo]) -> String {
    let group = lights.iter().find(|light| light.is_group());
    let members: Vec<&LightInfo> = lights.iter().filter(|light| !light.is_group()).collect();

    let (text, class, percentage) = match group {
        None => ("no lights".to_string(), "empty", 0),
        Some(group) if group.online_count == 0 => ("offline".to_string(), "offline", 0),
        Some(group) if !group.on => ("off".to_string(), "off", 0),
        Some(group) => (format!("{}%", group.brightness), "on", u32::from(group.brightness)),
    };

    let tooltip = if members.is_empty() {
        "no lights discovered".to_string()
    } else {
        members
            .iter()
            .map(|light| {
                if light.online {
                    format!(
                        "{}: {} {}% {}K",
                        light.name,
                        light.state_word(),
                        light.brightness,
                        light.kelvin
                    )
                } else {
                    format!("{}: offline", light.name)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    json!({ "text": text, "class": class, "percentage": percentage, "tooltip": tooltip })
        .to_string()
}

/// Compact single line for `gaffer watch` at a terminal.
pub fn line(lights: &[LightInfo]) -> String {
    let mut parts: Vec<String> = lights
        .iter()
        .filter(|light| !light.is_group())
        .map(|light| {
            if light.online {
                format!(
                    "{} {} {}% {}K",
                    light.name,
                    light.state_word(),
                    light.brightness,
                    light.kelvin
                )
            } else {
                format!("{} offline", light.name)
            }
        })
        .collect();

    if parts.is_empty() {
        parts.push("no lights".to_string());
    }
    parts.join("  |  ")
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light(name: &str, on: bool, online: bool) -> LightInfo {
        LightInfo {
            id: "00:00:5E:00:53:01".into(),
            name: name.into(),
            address: "http://192.168.1.63:9123".into(),
            online,
            on,
            brightness: 42,
            kelvin: 4200,
            ..Default::default()
        }
    }

    fn group(on: bool, online_count: u32) -> LightInfo {
        LightInfo {
            id: String::new(),
            name: "All Lights".into(),
            online: online_count > 0,
            online_count,
            on,
            brightness: 42,
            kelvin: 4200,
            ..Default::default()
        }
    }

    #[test]
    fn an_empty_world_still_renders() {
        assert_eq!(table(&[]), "no lights found\n");
        assert_eq!(json(&[]), "[]");
    }

    #[test]
    fn the_table_has_one_row_per_light_plus_a_header() {
        let rendered = table(&[light("Left", true, true), group(true, 1)]);
        assert_eq!(rendered.lines().count(), 3);
        assert!(rendered.starts_with("NAME"));
        assert!(rendered.contains("Left"));
        assert!(rendered.contains("42%"));
        assert!(rendered.contains("4200K"));
    }

    #[test]
    fn an_offline_light_reads_as_offline_not_off() {
        let rendered = table(&[light("Left", true, false)]);
        assert!(rendered.contains("offline"));
    }

    #[test]
    fn a_transport_error_is_shown_next_to_the_light() {
        let mut broken = light("Left", true, false);
        broken.last_error = "connection refused\nbacktrace...".into();
        let rendered = table(&[broken]);
        assert!(rendered.contains("connection refused"));
        assert!(!rendered.contains("backtrace"), "only the first line belongs in a table");
    }

    #[test]
    fn waybar_output_is_a_single_line_of_valid_json() {
        let rendered = waybar(&[light("Left", true, true), group(true, 1)]);
        assert!(!rendered.contains('\n'), "waybar reads one object per line");

        let parsed: Value = serde_json::from_str(&rendered).expect("valid json");
        assert_eq!(parsed["text"], "42%");
        assert_eq!(parsed["class"], "on");
        assert_eq!(parsed["percentage"], 42);
        assert!(parsed["tooltip"].as_str().unwrap().contains("Left"));
    }

    #[test]
    fn waybar_distinguishes_off_from_offline() {
        let off = waybar(&[light("Left", false, true), group(false, 1)]);
        assert_eq!(serde_json::from_str::<Value>(&off).unwrap()["class"], "off");

        let offline = waybar(&[light("Left", false, false), group(false, 0)]);
        assert_eq!(serde_json::from_str::<Value>(&offline).unwrap()["class"], "offline");
    }

    #[test]
    fn waybar_survives_having_no_lights_at_all() {
        let rendered = waybar(&[]);
        let parsed: Value = serde_json::from_str(&rendered).expect("valid json");
        assert_eq!(parsed["class"], "empty");
        assert!(parsed["tooltip"].as_str().unwrap().contains("no lights"));
    }

    #[test]
    fn tooltip_newlines_are_escaped_rather_than_breaking_the_line() {
        let rendered =
            waybar(&[light("Left", true, true), light("Right", true, true), group(true, 2)]);
        assert!(!rendered.contains('\n'));
        let parsed: Value = serde_json::from_str(&rendered).unwrap();
        assert!(
            parsed["tooltip"].as_str().unwrap().contains('\n'),
            "the value itself is multi-line"
        );
    }

    #[test]
    fn the_json_form_omits_the_group_flag_confusion() {
        let rendered = json(&[light("Left", true, true), group(true, 1)]);
        let parsed: Vec<Value> = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed[0]["group"], false);
        assert_eq!(parsed[1]["group"], true);
    }

    #[test]
    fn the_watch_line_excludes_the_group() {
        let rendered = line(&[light("Left", true, true), group(true, 1)]);
        assert!(rendered.contains("Left"));
        assert!(!rendered.contains("All Lights"));
    }
}
