//! Duration strings: `"100ms"`, `"5s"`, `"10m"`, `"2h"`.
//!
//! Stored as [`std::time::Duration`] post-parse so consumers do not re-parse.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Deserializer, Visitor};
use serde::Deserialize;

/// A [`Duration`] deserialized from a human-readable string.
///
/// Accepted units: `ms`, `s`, `m`, `h`. The number must be a non-negative
/// integer. Whitespace is permitted at the boundaries and between number and
/// unit. Anything else is a hard error at config-load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationStr(Duration);

impl DurationStr {
    /// Construct from a [`Duration`].
    pub const fn new(d: Duration) -> Self {
        Self(d)
    }

    /// Construct from a whole-second count.
    pub const fn from_secs(s: u64) -> Self {
        Self(Duration::from_secs(s))
    }

    /// The wrapped [`Duration`].
    pub const fn get(self) -> Duration {
        self.0
    }
}

impl From<DurationStr> for Duration {
    fn from(d: DurationStr) -> Self {
        d.0
    }
}

impl<'de> Deserialize<'de> for DurationStr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = DurationStr;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"a duration string like "5s", "100ms", "10m", "1h""#)
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<DurationStr, E> {
                parse(s).map(DurationStr).map_err(de::Error::custom)
            }
        }
        d.deserialize_str(V)
    }
}

fn parse(s: &str) -> Result<Duration, ParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }

    // Split at the first non-digit; everything before is the number, everything
    // after (possibly with trimmed whitespace) is the unit.
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or(ParseError::MissingUnit)?;
    if split == 0 {
        return Err(ParseError::MissingNumber);
    }
    let (num_str, unit) = s.split_at(split);
    let n: u64 = num_str.parse().map_err(|_| ParseError::Overflow)?;

    let unit = unit.trim();
    let secs_mul: u64 = match unit {
        "ms" => return Ok(Duration::from_millis(n)),
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        other => return Err(ParseError::UnknownUnit(other.to_string())),
    };
    let secs = n.checked_mul(secs_mul).ok_or(ParseError::Overflow)?;
    Ok(Duration::from_secs(secs))
}

#[derive(Debug, thiserror::Error)]
enum ParseError {
    #[error("duration string is empty")]
    Empty,
    #[error("duration is missing a unit (expected ms, s, m, or h)")]
    MissingUnit,
    #[error("duration is missing a number")]
    MissingNumber,
    #[error("duration value overflows")]
    Overflow,
    #[error("unknown duration unit `{0}` (expected ms, s, m, or h)")]
    UnknownUnit(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ms() {
        assert_eq!(parse("100ms").unwrap(), Duration::from_millis(100));
    }

    #[test]
    fn seconds() {
        assert_eq!(parse("5s").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn minutes() {
        assert_eq!(parse("10m").unwrap(), Duration::from_secs(600));
    }

    #[test]
    fn hours() {
        assert_eq!(parse("2h").unwrap(), Duration::from_secs(7_200));
    }

    #[test]
    fn whitespace_tolerated() {
        assert_eq!(parse(" 5 s ").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(parse(""), Err(ParseError::Empty)));
    }

    #[test]
    fn rejects_no_unit() {
        assert!(matches!(parse("5"), Err(ParseError::MissingUnit)));
    }

    #[test]
    fn rejects_no_number() {
        assert!(matches!(parse("s"), Err(ParseError::MissingNumber)));
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(matches!(parse("5x"), Err(ParseError::UnknownUnit(_))));
    }

    #[test]
    fn rejects_overflow() {
        // u64::MAX hours saturates immediately
        assert!(matches!(
            parse("18446744073709551615h"),
            Err(ParseError::Overflow)
        ));
    }

    #[test]
    fn deserialize_via_toml() {
        #[derive(serde::Deserialize)]
        struct Wrap {
            d: DurationStr,
        }
        let w: Wrap = toml::from_str(r#"d = "250ms""#).unwrap();
        assert_eq!(w.d.get(), Duration::from_millis(250));
    }
}
