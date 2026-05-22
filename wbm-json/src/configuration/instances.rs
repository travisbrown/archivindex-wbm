pub mod wts {
    use std::borrow::Cow;

    pub type WtsSnapshot<'a> = crate::Snapshot<'a, WtsConfiguration, Content<'a>>;

    /// Configuration for Truth Social post snapshots.
    pub struct WtsConfiguration;

    impl crate::configuration::Configuration for WtsConfiguration {
        type Content<'a> = Content<'a>;

        fn default_closing_whitespace() -> &'static [char] {
            &['\r', '\r', '\n']
        }

        fn infer_url<'a>(content: &Self::Content<'a>) -> Option<Cow<'a, str>> {
            Some(format!("https://truthsocial.com/api/v1/statuses/{}", content.id).into())
        }
    }

    #[derive(serde::Deserialize)]
    pub struct Content<'a> {
        id: Cow<'a, str>,
    }
}

pub mod wxj {
    pub type WxjGenericSnapshot<'a> =
        crate::Snapshot<'a, WxjGenericConfiguration, serde::de::IgnoredAny>;

    /// Configuration for WXJ data format tweet snapshots.
    pub struct WxjGenericConfiguration;

    impl crate::configuration::Configuration for WxjGenericConfiguration {
        type Content<'a> = serde::de::IgnoredAny;

        fn default_closing_whitespace() -> &'static [char] {
            &['\r', '\r', '\n']
        }
    }

    pub mod data {
        use std::borrow::Cow;

        pub type WxjDataSnapshot<'a> = crate::Snapshot<'a, WxjDataConfiguration, Content<'a>>;

        /// Configuration for WXJ data format tweet snapshots.
        pub struct WxjDataConfiguration;

        impl crate::configuration::Configuration for WxjDataConfiguration {
            type Content<'a> = Content<'a>;

            fn default_closing_whitespace() -> &'static [char] {
                &['\r', '\r', '\n']
            }

            fn infer_url<'a>(content: &Self::Content<'a>) -> Option<Cow<'a, str>> {
                let user = content.lookup_user(&content.data.author_id)?;
                let username = user.username.as_deref()?;

                Some(
                    format!(
                        "https://twitter.com/{}/status/{}",
                        username, content.data.id
                    )
                    .into(),
                )
            }
        }

        #[derive(serde::Deserialize)]
        pub struct Content<'a> {
            data: Data<'a>,
            includes: Includes<'a>,
        }

        impl<'a> Content<'a> {
            fn lookup_user(&self, id: &'a str) -> Option<&User<'a>> {
                self.includes.users.iter().find(|user| user.id == id)
            }
        }

        #[derive(serde::Deserialize)]
        struct Data<'a> {
            id: Cow<'a, str>,
            author_id: Cow<'a, str>,
        }

        #[derive(serde::Deserialize)]
        struct Includes<'a> {
            users: Vec<User<'a>>,
        }

        #[derive(serde::Deserialize)]
        struct User<'a> {
            id: Cow<'a, str>,
            username: Option<Cow<'a, str>>,
        }
    }

    pub mod flat {
        use std::borrow::Cow;

        pub type WxjFlatSnapshot<'a> = crate::Snapshot<'a, WxjFlatConfiguration, Content<'a>>;

        /// Configuration for WXJ flat format tweet snapshots.
        pub struct WxjFlatConfiguration;

        impl crate::configuration::Configuration for WxjFlatConfiguration {
            type Content<'a> = Content<'a>;

            fn default_closing_whitespace() -> &'static [char] {
                &['\r', '\r', '\n']
            }

            fn infer_url<'a>(content: &Self::Content<'a>) -> Option<Cow<'a, str>> {
                Some(
                    format!(
                        "https://twitter.com/{}/status/{}",
                        content.user.screen_name, content.id_str
                    )
                    .into(),
                )
            }
        }

        #[derive(serde::Deserialize)]
        pub struct Content<'a> {
            id_str: Cow<'a, str>,
            user: User<'a>,
        }

        #[derive(serde::Deserialize)]
        struct User<'a> {
            screen_name: Cow<'a, str>,
        }
    }
}
