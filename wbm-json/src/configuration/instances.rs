pub mod wts {
    use crate::context::Context;

    /// The canonical [`Context`] for Truth Social post snapshots.
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
    use crate::context::Context;

    /// The canonical closing whitespace for all WXJ (Twitter) formats.
    const WXJ_CLOSING_WHITESPACE: &[char] = &['\r', '\r', '\n'];

    /// The canonical [`Context`] for the WXJ format, without URL inference.
    ///
    /// Suitable for validation when the content schema (and hence URL inference) is irrelevant.
    #[must_use]
    pub const fn context() -> Context {
        Context::from_static(WXJ_CLOSING_WHITESPACE)
    }

    pub mod data {
        use super::WXJ_CLOSING_WHITESPACE;
        use crate::context::Context;

        /// The canonical [`Context`] for WXJ data format tweet snapshots.
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
        use crate::context::Context;

        /// The canonical [`Context`] for WXJ flat format tweet snapshots.
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
