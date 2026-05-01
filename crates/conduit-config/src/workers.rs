//! Worker count: either an explicit positive integer or the string `"auto"`.

use std::fmt;
use std::num::NonZeroUsize;

use serde::de::{self, Deserializer, Visitor};
use serde::Deserialize;

/// How many worker tasks the runtime should run.
///
/// `Auto` means: pick at startup based on available cores. `Count(n)` is an
/// explicit count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Workers {
    /// Pick at startup from `std::thread::available_parallelism`.
    #[default]
    Auto,
    /// Explicit count, must be ≥ 1.
    Count(NonZeroUsize),
}

impl<'de> Deserialize<'de> for Workers {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Workers;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"a positive integer or the string "auto""#)
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<Workers, E> {
                if s == "auto" {
                    Ok(Workers::Auto)
                } else {
                    Err(de::Error::invalid_value(de::Unexpected::Str(s), &self))
                }
            }

            fn visit_i64<E: de::Error>(self, n: i64) -> Result<Workers, E> {
                if n <= 0 {
                    return Err(de::Error::invalid_value(de::Unexpected::Signed(n), &self));
                }
                let n_usize = usize::try_from(n)
                    .map_err(|_| de::Error::invalid_value(de::Unexpected::Signed(n), &self))?;
                NonZeroUsize::new(n_usize)
                    .map(Workers::Count)
                    .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Signed(n), &self))
            }

            fn visit_u64<E: de::Error>(self, n: u64) -> Result<Workers, E> {
                let n_usize = usize::try_from(n)
                    .map_err(|_| de::Error::invalid_value(de::Unexpected::Unsigned(n), &self))?;
                NonZeroUsize::new(n_usize)
                    .map(Workers::Count)
                    .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Unsigned(n), &self))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Wrap {
        w: Workers,
    }

    #[test]
    fn auto_string() {
        let w: Wrap = toml::from_str(r#"w = "auto""#).unwrap();
        assert_eq!(w.w, Workers::Auto);
    }

    #[test]
    fn integer_count() {
        let w: Wrap = toml::from_str("w = 4").unwrap();
        assert_eq!(w.w, Workers::Count(NonZeroUsize::new(4).unwrap()));
    }

    #[test]
    fn rejects_zero() {
        let r: Result<Wrap, _> = toml::from_str("w = 0");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_negative() {
        let r: Result<Wrap, _> = toml::from_str("w = -1");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_unknown_string() {
        let r: Result<Wrap, _> = toml::from_str(r#"w = "many""#);
        assert!(r.is_err());
    }

    #[test]
    fn default_is_auto() {
        assert_eq!(Workers::default(), Workers::Auto);
    }
}
