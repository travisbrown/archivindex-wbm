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

        /// Returns the canonical [`Context`] for the WXJ format, without URL inference.
        #[must_use]
        pub const fn context() -> Context {
            Context::from_static(WXJ_CLOSING_WHITESPACE)
        }

        pub mod data {
            use super::super::Context;
            use super::WXJ_CLOSING_WHITESPACE;

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
            use super::super::Context;
            use super::WXJ_CLOSING_WHITESPACE;

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
