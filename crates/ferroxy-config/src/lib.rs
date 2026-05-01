//! ferroxy-config — parse and validate the proxy configuration.
//!
//! Layer 6 (control) under the engineering charter. This crate has **no**
//! upward project dependencies and runs on the cold/startup path: it is
//! permitted to allocate freely.
//!
//! # Public surface
//!
//! - [`Config`] is the validated configuration the rest of the system consumes.
//!   The only constructor is [`parse`] / [`load`]; [`Config`] implements
//!   [`serde::Deserialize`] but external callers must go through these
//!   entry points so semantic validation is not skipped.
//! - [`ConfigError`] is the typed error returned by both entry points.

#![deny(missing_docs)]

mod duration;
mod error;
mod schema;
mod validate;
mod workers;

pub use duration::DurationStr;
pub use error::ConfigError;
pub use schema::*;
pub use workers::Workers;

use std::path::Path;

/// Read a configuration file from disk, parse it as TOML, and validate it.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&text)
}

/// Parse a configuration from an in-memory TOML string and validate it.
///
/// Equivalent to [`load`] without the filesystem read. Useful for tests and
/// for callers that read the configuration through a non-filesystem source
/// (admin endpoint, etc.).
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let cfg: Config = toml::from_str(text)?;
    validate::check(&cfg)?;
    Ok(cfg)
}
