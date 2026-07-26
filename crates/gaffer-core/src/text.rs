//! Making untrusted device text safe to display.
//!
//! Names, models and firmware strings come from two places an attacker on the
//! local link controls completely: the `displayName` field of an HTTP response,
//! and mDNS record labels. They then travel to a terminal via the CLI, to a
//! status bar, to the journal, and to any D-Bus client — none of which expect
//! to receive control characters.
//!
//! Sanitising here rather than in the CLI's renderer is deliberate: the daemon
//! is the single point every one of those sinks reads through, so one filter
//! covers all of them, including sinks that do not exist yet.

/// Longest device string worth keeping. Real names are a few words; anything
/// longer is either a mistake or an attempt to disrupt a display.
pub const MAX_DISPLAY_LEN: usize = 128;

/// Strip control characters and clamp the length of untrusted device text.
///
/// Control characters are *removed* rather than escaped, because every consumer
/// wants something printable and none of them want to render an escape. That
/// includes:
///
/// * `ESC`, which would otherwise let a device paint arbitrary ANSI sequences
///   onto the terminal of anyone running `gaffer list` or `gaffer watch` —
///   erasing lines, forging prompts, or writing the clipboard via OSC 52.
/// * `NUL`, which is not representable in a D-Bus string. Serialising one would
///   produce a malformed message and get the daemon disconnected from the bus.
/// * `\r` and `\n`, which would let a device forge extra lines in the journal
///   or in the CLI's table.
pub fn sanitize(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(MAX_DISPLAY_LEN)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Reduce a device id to its canonical form: alphanumerics only, upper-cased.
///
/// Ids are hardware MACs, which devices punctuate inconsistently. Canonicalising
/// once, at discovery, means `00:00:5E:00:53:01` and `00-00-5e-00-53-01` are
/// recognised as the same hardware and become one record — rather than two
/// records that then collide on a single D-Bus object path.
///
/// That collision was a real defect: two mDNS packets, an announcement using a
/// punctuation variant of a real light's id followed by a goodbye, would
/// unregister the *genuine* light's object permanently, because the two records
/// never merged and so the survivor was never republished. Canonicalising
/// reduces that to ordinary mDNS impersonation, which self-corrects when the
/// real light re-announces.
///
/// Returns `None` when nothing usable remains, which also keeps an id of pure
/// punctuation from producing the empty, invalid object path `…/lights/`.
pub fn normalize_id(raw: &str) -> Option<String> {
    let id: String =
        raw.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punctuation_variants_of_one_mac_canonicalise_together() {
        let canonical = normalize_id("00:00:5E:00:53:01");
        assert_eq!(canonical.as_deref(), Some("00005E005301"));
        assert_eq!(normalize_id("00-00-5e-00-53-01"), canonical);
        assert_eq!(normalize_id("00005e005301"), canonical);
    }

    #[test]
    fn distinct_hardware_stays_distinct() {
        assert_ne!(normalize_id("00:00:5E:00:53:01"), normalize_id("00:00:5E:00:53:02"));
    }

    #[test]
    fn an_id_with_nothing_usable_is_rejected() {
        // Would otherwise yield the invalid object path `…/lights/`.
        assert_eq!(normalize_id("::::"), None);
        assert_eq!(normalize_id(""), None);
        assert_eq!(normalize_id("  -- "), None);
    }

    #[test]
    fn the_canonical_form_is_a_valid_dbus_path_element() {
        let id = normalize_id("00:00:5E:00:53:01").unwrap();
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(!id.is_empty());
    }

    #[test]
    fn ordinary_names_are_untouched() {
        assert_eq!(sanitize("Elgato Key Light Left"), "Elgato Key Light Left");
        assert_eq!(sanitize("Ring Light — desk"), "Ring Light — desk");
    }

    #[test]
    fn ansi_escapes_cannot_reach_a_terminal() {
        // The reported attack: erase the line and forge a sudo prompt.
        let hostile = "Key Light\u{1b}[2K\r  \u{1b}[32m[sudo] password:\u{1b}[0m";
        let clean = sanitize(hostile);
        assert!(!clean.contains('\u{1b}'), "ESC must not survive: {clean:?}");
        assert!(!clean.contains('\r'));
        assert_eq!(clean, "Key Light[2K  [32m[sudo] password:[0m");
    }

    #[test]
    fn nul_is_removed_so_dbus_serialisation_cannot_fail() {
        // A D-Bus string cannot contain NUL; emitting one would produce a
        // malformed message and disconnect the daemon from the bus.
        assert_eq!(sanitize("a\0b"), "ab");
        assert!(!sanitize("\0\0\0").contains('\0'));
    }

    #[test]
    fn newlines_cannot_forge_extra_log_or_table_lines() {
        assert_eq!(sanitize("Left\nFAKE  on  100%"), "LeftFAKE  on  100%");
    }

    #[test]
    fn absurdly_long_names_are_clamped() {
        let long = "x".repeat(10_000);
        assert_eq!(sanitize(&long).len(), MAX_DISPLAY_LEN);
    }

    #[test]
    fn clamping_counts_characters_not_bytes() {
        // Truncating by bytes would split a multi-byte character; `take` on
        // chars cannot, so this must not panic and must stay valid UTF-8.
        let long = "é".repeat(10_000);
        assert_eq!(sanitize(&long).chars().count(), MAX_DISPLAY_LEN);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(sanitize("  Left  "), "Left");
    }

    #[test]
    fn a_string_of_only_control_characters_becomes_empty() {
        // Callers treat empty as "no name reported" and keep what they had.
        assert!(sanitize("\u{1b}\r\n\0").is_empty());
    }
}
