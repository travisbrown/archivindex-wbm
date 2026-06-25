pub mod instances {
    use archivindex_wbm_json::context::Context;

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
        const WXJ_CLOSING_WHITESPACE: &[char] = &['\r', '\r', '\n'];

        pub mod data {
            use super::WXJ_CLOSING_WHITESPACE;
            use super::super::Context;

            /// Returns the canonical [`Context`] for WXJ data format tweet snapshots.
            ///
            /// # Panics
            ///
            /// Panics if the built-in CEL URL query fails to compile (a bug).
            #[must_use]
            pub fn context() -> Context {
                Context::from_static(WXJ_CLOSING_WHITESPACE)
                    .with_url_query(
                        "'https://twitter.com/' + \
                         content.includes.users.filter(u, u.id == content.data.author_id)[0].username + \
                         '/status/' + content.data.id",
                    )
                    .expect("valid CEL query")
            }
        }

        pub mod flat {
            use super::WXJ_CLOSING_WHITESPACE;
            use super::super::Context;

            /// Returns the canonical [`Context`] for WXJ flat format tweet snapshots.
            ///
            /// # Panics
            ///
            /// Panics if the built-in CEL URL query fails to compile (a bug).
            #[must_use]
            pub fn context() -> Context {
                Context::from_static(WXJ_CLOSING_WHITESPACE)
                    .with_url_query(
                        "'https://twitter.com/' + content.user.screen_name + '/status/' + content.id_str",
                    )
                    .expect("valid CEL query")
            }
        }
    }
}
