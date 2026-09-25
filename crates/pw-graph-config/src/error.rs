//! TOML configuration compatible with the state surface described by qpwgraph.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config: {0}")]
    Read(#[source] std::io::Error),
    #[error("could not write config: {0}")]
    Write(#[source] std::io::Error),
    #[error("invalid config TOML: {0}")]
    Format(#[from] toml::de::Error),
    #[error("could not serialize config TOML: {0}")]
    Serialize(#[from] toml::ser::Error),
}
