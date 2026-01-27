use archivindex_wbm_json::configuration::Configuration;
use std::borrow::Cow;

/// Configuration for WXJ data format (v2 API format).
pub struct WxjDataConfig;

impl Configuration for WxjDataConfig {
    type S<'a> = birdsite::model::wxj::data::TweetSnapshot<'a>;

    fn default_closing_whitespace() -> &'static [char] {
        &['\r', '\r', '\n']
    }

    fn infer_url<'a>(content: &Self::S<'a>) -> Option<Cow<'a, str>> {
        content.lookup_user(content.data.author_id).map(|user| {
            format!(
                "https://twitter.com/{}/status/{}",
                user.username, content.data.id
            )
            .into()
        })
    }
}
