//! The user configuration, `config.toml` in the data directory.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths::Paths;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub recognize: Recognize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Recognize {
    pub wrappers: Vec<String>,
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        match std::fs::read_to_string(&paths.config) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("reading {}", paths.config.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", paths.config.display())),
        }
    }
}
