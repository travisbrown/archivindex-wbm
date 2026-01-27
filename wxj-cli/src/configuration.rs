use archivindex_wbm_json::Configuration;
use std::borrow::Cow;

/// Configuration for WXJ data format snapshots.
pub struct WxjDataConfig;

impl Configuration for WxjDataConfig {
    type S<'a> = birdsite::model::wxj::data::TweetSnapshot<'a>;

    fn default_closing_whitespace() -> &'static [char] {
        &['\r', '\r', '\n']
    }

    fn infer_url<'a>(content: &'a Self::S<'a>) -> Option<Cow<'a, str>> {
        content.lookup_user(content.data.author_id).map(|user| {
            format!(
                "https://twitter.com/{}/status/{}",
                user.username, content.data.id
            )
            .into()
        })
    }
}

/// Configuration for WXJ flat format snapshots.
pub struct WxjFlatConfig;

impl Configuration for WxjFlatConfig {
    type S<'a> = birdsite::model::wxj::flat::TweetSnapshot<'a>;

    fn default_closing_whitespace() -> &'static [char] {
        &['\r', '\r', '\n']
    }

    fn infer_url<'a>(content: &'a Self::S<'a>) -> Option<Cow<'a, str>> {
        Some(
            format!(
                "https://twitter.com/{}/status/{}",
                content.user.screen_name, content.id
            )
            .into(),
        )
    }
}
