//! Parsing the CLI's suffix-typed value tokens.
//!
//! `gaffer set left 42% 4200k` reads naturally because the suffix carries the
//! unit. That means the grammar is positional but order-free: `42% 4200k` and
//! `4200k 42%` are the same command, and each field may appear at most once.
//!
//! Bare numbers are accepted where they are unambiguous — brightness tops out
//! at 100 and Kelvin starts at 2900, so the ranges cannot overlap.

use crate::color::{MAX_KELVIN, MIN_KELVIN};
use crate::state::{Adjust, MAX_BRIGHTNESS, Power, StatePatch};

/// One parsed value token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Power(Power),
    Brightness(Adjust),
    Kelvin(Adjust),
}

/// Why a value token could not be understood.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    NotANumber(String),
    /// A bare number that is neither a valid brightness nor a valid Kelvin.
    Ambiguous(String),
    /// A signed value with no unit — `+10` could mean either field.
    RelativeNeedsUnit(String),
    OutOfRange {
        token: String,
        unit: &'static str,
        min: i32,
        max: i32,
    },
    /// The same field was named twice in one command.
    Repeated(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty value"),
            Self::NotANumber(token) => write!(f, "`{token}` is not a number"),
            Self::Ambiguous(token) => write!(
                f,
                "`{token}` is neither a brightness (0-{MAX_BRIGHTNESS}) nor a colour temperature \
                 ({MIN_KELVIN}-{MAX_KELVIN}); add a `%` or `k` suffix"
            ),
            Self::RelativeNeedsUnit(token) => {
                write!(f, "`{token}` needs a unit — did you mean `{token}%` or `{token}k`?")
            }
            Self::OutOfRange { token, unit, min, max } => {
                write!(f, "`{token}` is out of range; {unit} must be {min}-{max}")
            }
            Self::Repeated(field) => write!(f, "{field} was given more than once"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a single value token such as `on`, `42%`, `4200k`, `+10%` or `-200k`.
pub fn parse_token(raw: &str) -> Result<Token, ParseError> {
    let token = raw.trim();
    if token.is_empty() {
        return Err(ParseError::Empty);
    }

    match token.to_ascii_lowercase().as_str() {
        "on" => return Ok(Token::Power(Power::On)),
        "off" => return Ok(Token::Power(Power::Off)),
        "toggle" => return Ok(Token::Power(Power::Toggle)),
        _ => {}
    }

    if let Some(number) = token.strip_suffix('%') {
        let (value, relative) = split_number(token, number)?;
        if !relative {
            check_range(token, "brightness", value, 0, i32::from(MAX_BRIGHTNESS))?;
        }
        return Ok(Token::Brightness(adjust(value, relative)));
    }

    if let Some(number) = token.strip_suffix(['k', 'K']) {
        let (value, relative) = split_number(token, number)?;
        if !relative {
            check_range(
                token,
                "colour temperature",
                value,
                i32::from(MIN_KELVIN),
                i32::from(MAX_KELVIN),
            )?;
        }
        return Ok(Token::Kelvin(adjust(value, relative)));
    }

    // A bare number. Only accept it where the two ranges cannot be confused.
    let (value, relative) = split_number(token, token)?;
    if relative {
        return Err(ParseError::RelativeNeedsUnit(token.to_string()));
    }
    match value {
        0..=100 => Ok(Token::Brightness(Adjust::Set(value))),
        2900..=7000 => Ok(Token::Kelvin(Adjust::Set(value))),
        _ => Err(ParseError::Ambiguous(token.to_string())),
    }
}

/// Fold a sequence of value tokens into one patch.
pub fn parse_patch<I, S>(tokens: I) -> Result<StatePatch, ParseError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut patch = StatePatch::EMPTY;

    for raw in tokens {
        match parse_token(raw.as_ref())? {
            Token::Power(power) => {
                if patch.power.replace(power).is_some() {
                    return Err(ParseError::Repeated("power"));
                }
            }
            Token::Brightness(adjust) => {
                if patch.brightness.replace(adjust).is_some() {
                    return Err(ParseError::Repeated("brightness"));
                }
            }
            Token::Kelvin(adjust) => {
                if patch.kelvin.replace(adjust).is_some() {
                    return Err(ParseError::Repeated("colour temperature"));
                }
            }
        }
    }

    Ok(patch)
}

fn split_number(token: &str, number: &str) -> Result<(i32, bool), ParseError> {
    let relative = number.starts_with('+') || number.starts_with('-');
    let value = number.parse::<i32>().map_err(|_| ParseError::NotANumber(token.to_string()))?;
    Ok((value, relative))
}

const fn adjust(value: i32, relative: bool) -> Adjust {
    if relative { Adjust::By(value) } else { Adjust::Set(value) }
}

fn check_range(
    token: &str,
    unit: &'static str,
    value: i32,
    min: i32,
    max: i32,
) -> Result<(), ParseError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ParseError::OutOfRange { token: token.to_string(), unit, min, max })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_words_are_case_insensitive() {
        assert_eq!(parse_token("on").unwrap(), Token::Power(Power::On));
        assert_eq!(parse_token("OFF").unwrap(), Token::Power(Power::Off));
        assert_eq!(parse_token("Toggle").unwrap(), Token::Power(Power::Toggle));
    }

    #[test]
    fn suffixes_choose_the_field() {
        assert_eq!(parse_token("42%").unwrap(), Token::Brightness(Adjust::Set(42)));
        assert_eq!(parse_token("4200k").unwrap(), Token::Kelvin(Adjust::Set(4200)));
        assert_eq!(parse_token("4200K").unwrap(), Token::Kelvin(Adjust::Set(4200)));
    }

    #[test]
    fn signs_make_an_adjustment_relative() {
        assert_eq!(parse_token("+10%").unwrap(), Token::Brightness(Adjust::By(10)));
        assert_eq!(parse_token("-10%").unwrap(), Token::Brightness(Adjust::By(-10)));
        assert_eq!(parse_token("+200k").unwrap(), Token::Kelvin(Adjust::By(200)));
        assert_eq!(parse_token("-200k").unwrap(), Token::Kelvin(Adjust::By(-200)));
    }

    #[test]
    fn bare_numbers_resolve_by_range() {
        assert_eq!(parse_token("0").unwrap(), Token::Brightness(Adjust::Set(0)));
        assert_eq!(parse_token("100").unwrap(), Token::Brightness(Adjust::Set(100)));
        assert_eq!(parse_token("2900").unwrap(), Token::Kelvin(Adjust::Set(2900)));
        assert_eq!(parse_token("7000").unwrap(), Token::Kelvin(Adjust::Set(7000)));
    }

    #[test]
    fn a_bare_number_in_neither_range_is_rejected() {
        // 500 is too bright to be brightness and too cold to be Kelvin.
        assert!(matches!(parse_token("500"), Err(ParseError::Ambiguous(_))));
        assert!(matches!(parse_token("-5"), Err(ParseError::RelativeNeedsUnit(_))));
    }

    #[test]
    fn absolute_values_outside_the_hardware_range_are_rejected_not_clamped() {
        // Catches the unit mix-up `4200%`, which clamping would silently accept.
        assert!(matches!(parse_token("4200%"), Err(ParseError::OutOfRange { .. })));
        assert!(matches!(parse_token("42k"), Err(ParseError::OutOfRange { .. })));
        assert!(matches!(parse_token("9000k"), Err(ParseError::OutOfRange { .. })));
    }

    #[test]
    fn relative_values_are_not_range_checked() {
        // They clamp against the live value at apply time instead.
        assert_eq!(parse_token("+9000k").unwrap(), Token::Kelvin(Adjust::By(9000)));
        assert_eq!(parse_token("-500%").unwrap(), Token::Brightness(Adjust::By(-500)));
    }

    #[test]
    fn garbage_is_reported_with_the_whole_token() {
        assert_eq!(parse_token("abc"), Err(ParseError::NotANumber("abc".into())));
        assert_eq!(parse_token("12x%"), Err(ParseError::NotANumber("12x%".into())));
        assert_eq!(parse_token("   "), Err(ParseError::Empty));
    }

    #[test]
    fn a_patch_collects_tokens_in_any_order() {
        let forward = parse_patch(["on", "42%", "4200k"]).unwrap();
        let shuffled = parse_patch(["4200k", "on", "42%"]).unwrap();
        assert_eq!(forward, shuffled);
        assert_eq!(forward.power, Some(Power::On));
        assert_eq!(forward.brightness, Some(Adjust::Set(42)));
        assert_eq!(forward.kelvin, Some(Adjust::Set(4200)));
    }

    #[test]
    fn repeating_a_field_is_an_error_rather_than_last_wins() {
        assert_eq!(parse_patch(["42%", "50%"]), Err(ParseError::Repeated("brightness")));
        assert_eq!(parse_patch(["on", "off"]), Err(ParseError::Repeated("power")));
        assert_eq!(
            parse_patch(["4200k", "5000k"]),
            Err(ParseError::Repeated("colour temperature"))
        );
    }

    #[test]
    fn no_tokens_yields_an_empty_patch() {
        let empty: [&str; 0] = [];
        assert!(parse_patch(empty).unwrap().is_empty());
    }
}
