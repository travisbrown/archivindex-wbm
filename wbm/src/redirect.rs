//! Construction and parsing of the small HTML redirect pages that the Wayback Machine stores for a
//! capture listed as a 302 redirect.
use std::sync::LazyLock;

const REDIRECT_HTML_PATTERN: &str =
    r#"^<html><body>You are being <a href="([^"]+)">redirected</a>\.</body></html>$"#;

/// Attempts to guess the contents of a redirect page stored by the Wayback Machine.
///
/// When an item is listed as a 302 redirect in CDX results, the content of the page usually (but
/// not always) has the following format, where the URL is the value of the location header.
#[must_use]
pub fn make_redirect_html(url: &str) -> String {
    format!("<html><body>You are being <a href=\"{url}\">redirected</a>.</body></html>")
}

pub fn parse_redirect_html(content: &str) -> Option<&str> {
    static REDIRECT_HTML_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(REDIRECT_HTML_PATTERN).unwrap());

    REDIRECT_HTML_RE
        .captures(content)
        .and_then(|groups| groups.get(1))
        .map(|m| m.as_str())
}
