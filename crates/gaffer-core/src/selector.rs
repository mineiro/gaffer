//! Choosing which lights a command applies to.
//!
//! Selectors are deliberately forgiving: `left` should find "Elgato Key Light
//! Left" without quoting or tab-completion, because the whole point of the CLI
//! is to be bindable to a key in one line of compositor config.
//!
//! A selector may match several lights. That is not an error — the command fans
//! out. Matching *nothing* is the error, and the caller reports it.

/// Which lights a command addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    /// Every known light.
    All,
    /// Case-insensitive match against a light's id or a substring of its name.
    Pattern(String),
}

impl Selector {
    /// Interpret a user-supplied selector string.
    pub fn parse(raw: &str) -> Self {
        let token = raw.trim();
        if token.eq_ignore_ascii_case("all") || token == "*" {
            Selector::All
        } else {
            Selector::Pattern(token.to_string())
        }
    }

    /// Does this selector address the light with the given id and name?
    pub fn matches(&self, id: &str, name: &str) -> bool {
        match self {
            Selector::All => true,
            Selector::Pattern(pattern) => {
                // Ids are MACs; accept them with or without separators so both
                // `00:00:5E:00:53:01` and `00005e005301` work.
                if !pattern.is_empty() && squash(id) == squash(pattern) {
                    return true;
                }
                let pattern = pattern.to_lowercase();
                !pattern.is_empty() && name.to_lowercase().contains(&pattern)
            }
        }
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Selector::All => f.write_str("all"),
            Selector::Pattern(pattern) => f.write_str(pattern),
        }
    }
}

/// Reduce an identifier to comparable form: alphanumerics only, lowercased.
fn squash(value: &str) -> String {
    value.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT_ID: &str = "00:00:5E:00:53:01";
    const LEFT_NAME: &str = "Elgato Key Light Left";
    const RIGHT_ID: &str = "00:00:5E:00:53:02";
    const RIGHT_NAME: &str = "Elgato Key Light Right";

    #[test]
    fn all_is_spelled_several_ways() {
        assert_eq!(Selector::parse("all"), Selector::All);
        assert_eq!(Selector::parse("ALL"), Selector::All);
        assert_eq!(Selector::parse("*"), Selector::All);
        assert_eq!(Selector::parse("  all  "), Selector::All);
        assert!(Selector::parse("all").matches(LEFT_ID, LEFT_NAME));
    }

    #[test]
    fn a_name_substring_matches_case_insensitively() {
        let left = Selector::parse("left");
        assert!(left.matches(LEFT_ID, LEFT_NAME));
        assert!(!left.matches(RIGHT_ID, RIGHT_NAME));

        assert!(Selector::parse("LEFT").matches(LEFT_ID, LEFT_NAME));
        assert!(Selector::parse("key light").matches(LEFT_ID, LEFT_NAME));
    }

    #[test]
    fn a_substring_may_legitimately_match_several_lights() {
        let both = Selector::parse("elgato");
        assert!(both.matches(LEFT_ID, LEFT_NAME));
        assert!(both.matches(RIGHT_ID, RIGHT_NAME));
    }

    #[test]
    fn ids_match_with_or_without_separators() {
        assert!(Selector::parse(LEFT_ID).matches(LEFT_ID, LEFT_NAME));
        assert!(Selector::parse("00005e005301").matches(LEFT_ID, LEFT_NAME));
        assert!(Selector::parse("00005E005301").matches(LEFT_ID, LEFT_NAME));
        assert!(!Selector::parse("00005e005301").matches(RIGHT_ID, RIGHT_NAME));
    }

    #[test]
    fn an_id_must_match_whole_not_as_a_prefix() {
        // Substring matching applies to names only; a partial MAC is a typo,
        // and silently hitting the wrong light would be worse than no match.
        assert!(!Selector::parse("00005e").matches(LEFT_ID, LEFT_NAME));
    }

    #[test]
    fn an_empty_pattern_matches_nothing() {
        assert!(!Selector::parse("").matches(LEFT_ID, LEFT_NAME));
    }
}
