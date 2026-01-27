pub mod wts {
    use std::borrow::Cow;

    pub type Snapshot<'a> = crate::Snapshot<'a, Configuration, Content<'a>>;

    /// Configuration for Truth Social post snapshots.
    pub struct Configuration;

    impl crate::configuration::Configuration for Configuration {
        type S<'a> = Content<'a>;

        fn default_closing_whitespace() -> &'static [char] {
            &['\r', '\r', '\n']
        }

        fn infer_url<'a>(content: &Self::S<'a>) -> Option<Cow<'a, str>> {
            Some(format!("https://truthsocial.com/api/v1/statuses/{}", content.id).into())
        }
    }

    #[derive(serde::Deserialize)]
    pub struct Content<'a> {
        id: Cow<'a, str>,
    }
}

pub mod wxj {
    pub mod data {
        use std::borrow::Cow;

        pub type Snapshot<'a> = crate::Snapshot<'a, Configuration, Content<'a>>;

        /// Configuration for WXJ data format tweet snapshots.
        pub struct Configuration;

        impl crate::configuration::Configuration for Configuration {
            type S<'a> = Content<'a>;

            fn default_closing_whitespace() -> &'static [char] {
                &['\r', '\r', '\n']
            }

            fn infer_url<'a>(content: &Self::S<'a>) -> Option<Cow<'a, str>> {
                content.lookup_user(&content.data.author_id).map(|user| {
                    format!(
                        "https://twitter.com/{}/status/{}",
                        user.username, content.data.id
                    )
                    .into()
                })
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
            username: Cow<'a, str>,
        }
    }

    pub mod flat {
        use std::borrow::Cow;

        pub type Snapshot<'a> = crate::Snapshot<'a, Configuration, Content<'a>>;

        /// Configuration for WXJ flat format tweet snapshots.
        pub struct Configuration;

        impl crate::configuration::Configuration for Configuration {
            type S<'a> = Content<'a>;

            fn default_closing_whitespace() -> &'static [char] {
                &['\r', '\r', '\n']
            }

            fn infer_url<'a>(content: &Self::S<'a>) -> Option<Cow<'a, str>> {
                Some(
                    format!(
                        "https://twitter.com/{}/status/{}",
                        content.user.screen_name, content.id
                    )
                    .into(),
                )
            }
        }

        #[derive(serde::Deserialize)]
        pub struct Content<'a> {
            id: Cow<'a, str>,
            user: User<'a>,
        }

        #[derive(serde::Deserialize)]
        struct User<'a> {
            screen_name: Cow<'a, str>,
        }
    }
}
