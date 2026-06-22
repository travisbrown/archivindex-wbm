use std::borrow::Cow;

/// Infer the canonical URL for a WXJ data format (v2 API) tweet snapshot from its birdsite content.
#[must_use]
pub fn infer_url<'a>(
    content: &birdsite::model::wxj::data::TweetSnapshot<'a>,
) -> Option<Cow<'a, str>> {
    content.lookup_user(content.data.author_id).map(|user| {
        format!(
            "https://twitter.com/{}/status/{}",
            user.username, content.data.id
        )
        .into()
    })
}
