//! Loads the canonical snapshot contexts from the TOML configuration files in this directory,
//! embedded at compile time.

use archivindex_wbm_json::context::{Context, ContextConfig};

fn load(toml_source: &str, name: &str) -> Context {
    let config: ContextConfig = toml::from_str(toml_source)
        .unwrap_or_else(|error| panic!("invalid {name} context configuration: {error}"));
    Context::from_config(config)
}

pub mod wxj {
    /// The canonical context for Twitter (WXJ) tweet snapshots.
    #[must_use]
    pub fn context() -> super::Context {
        super::load(include_str!("twitter.toml"), "Twitter")
    }
}

pub mod wts {
    /// The canonical context for Truth Social post snapshots.
    #[must_use]
    pub fn context() -> super::Context {
        super::load(include_str!("truthsocial.toml"), "Truth Social")
    }
}
