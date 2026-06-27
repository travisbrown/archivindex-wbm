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

#[cfg(test)]
mod tests {
    #[test]
    fn example_contexts_load_and_infer() {
        // Twitter: the conditional query handles both the v2 "data" shape and the flat shape.
        let wxj = super::wxj::context();
        let data = r#"{"data":{"id":"123","author_id":"42"},"includes":{"users":[{"id":"42","username":"alice"}]}}"#;
        assert_eq!(
            wxj.infer_url(data).as_deref(),
            Some("https://twitter.com/alice/status/123")
        );
        let flat = r#"{"id_str":"456","user":{"screen_name":"bob"}}"#;
        assert_eq!(
            wxj.infer_url(flat).as_deref(),
            Some("https://twitter.com/bob/status/456")
        );

        // Truth Social.
        let wts = super::wts::context();
        assert_eq!(
            wts.infer_url(r#"{"id":"789"}"#).as_deref(),
            Some("https://truthsocial.com/api/v1/statuses/789")
        );
    }
}
