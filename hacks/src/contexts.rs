//! Loads the canonical snapshot contexts from the bundled TOML configuration files in
//! `examples/contexts/`, embedded at compile time.

use archivindex_wbm_json::context::{Context, ContextConfig};

fn load(toml_source: &str, name: &str) -> Context {
    let config: ContextConfig = toml::from_str(toml_source)
        .unwrap_or_else(|error| panic!("invalid {name} context configuration: {error}"));
    Context::from_config(config)
        .unwrap_or_else(|error| panic!("invalid {name} context URL query: {error:?}"))
}

pub mod wxj {
    /// The canonical context for Twitter (WXJ) tweet snapshots.
    #[must_use]
    pub fn context() -> super::Context {
        super::load(
            include_str!("../../examples/contexts/twitter.toml"),
            "Twitter",
        )
    }
}

pub mod wts {
    /// The canonical context for Truth Social post snapshots.
    #[must_use]
    pub fn context() -> super::Context {
        super::load(
            include_str!("../../examples/contexts/truthsocial.toml"),
            "Truth Social",
        )
    }
}
