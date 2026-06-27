//! Ready-made [`Context`](crate::context::Context) builders for the snapshot sources this workspace
//! handles.
//!
//! The snapshot model itself is generic; these are a convenience so the CLIs and tools don't each
//! re-derive the same closing whitespace and CEL URL queries.

/// Canonical [`Context`](crate::context::Context) instances for specific snapshot formats / sites.
pub mod instances {
    use crate::context::Context;

    pub mod wts {
        use super::Context;

        /// Returns the canonical [`Context`] for Truth Social post snapshots.
        ///
        /// # Panics
        ///
        /// Panics if the built-in CEL URL query fails to compile (a bug).
        #[must_use]
        pub fn context() -> Context {
            Context::from_static(&['\n'])
                .with_url_query("'https://truthsocial.com/api/v1/statuses/' + content.id")
                .expect("valid CEL query")
        }
    }

    pub mod wxj {
        use super::Context;

        const WXJ_CLOSING_WHITESPACE: &[char] = &['\r', '\r', '\n'];

        /// Returns the canonical [`Context`] for WXJ (Twitter) tweet snapshots.
        ///
        /// The URL query is a single conditional CEL expression: it first tries the Twitter API v2
        /// "data" shape and, when that is absent, falls back to the older flat shape.
        ///
        /// # Panics
        ///
        /// Panics if the built-in CEL URL query fails to compile (a bug).
        #[must_use]
        pub fn context() -> Context {
            Context::from_static(WXJ_CLOSING_WHITESPACE)
                .with_url_query(
                    "has(content.data) ? \
                     'https://twitter.com/' + \
                     content.includes.users.filter(u, u.id == content.data.author_id)[0].username + \
                     '/status/' + content.data.id : \
                     'https://twitter.com/' + content.user.screen_name + '/status/' + content.id_str",
                )
                .expect("valid CEL query")
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn wxj_context_infers_data_and_flat_urls() {
        let context = super::instances::wxj::context();

        let data = r#"{"data":{"id":"123","author_id":"42"},"includes":{"users":[{"id":"42","username":"alice"}]}}"#;
        assert_eq!(
            context.infer_url(data).as_deref(),
            Some("https://twitter.com/alice/status/123")
        );

        let flat = r#"{"id_str":"456","user":{"screen_name":"bob"}}"#;
        assert_eq!(
            context.infer_url(flat).as_deref(),
            Some("https://twitter.com/bob/status/456")
        );
    }
}
