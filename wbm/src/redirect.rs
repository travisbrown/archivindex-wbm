//! Construction and parsing of the small HTML redirect pages that the Wayback Machine stores for a
//! capture listed as a 302 redirect.
use std::sync::LazyLock;

const REDIRECT_HTML_PATTERN: &str =
    r#"^<html><body>You are being <a href="([^"]+)">redirected</a>\.</body></html>$"#;

/// Attempts to guess the contents of a redirect page stored by the Wayback Machine.
///
/// When an item is listed as a 302 redirect in CDX results, the content of the page usually (but
/// not always) has the following format, where the URL is the value of the location header.
///
/// The URL is interpolated verbatim, with no HTML escaping, because the output must reproduce the
/// stored page byte for byte: callers compare its SHA-1 digest against the CDX `digest` field to
/// avoid fetching the body at all. A consequence is that a URL containing `"` produces a page that
/// [`parse_redirect_html`] cannot parse back.
#[must_use]
pub fn make_redirect_html(url: &str) -> String {
    format!("<html><body>You are being <a href=\"{url}\">redirected</a>.</body></html>")
}

/// Extracts the target URL from a redirect page in the format produced by [`make_redirect_html`],
/// or `None` if the content does not have that exact shape.
///
/// Because [`make_redirect_html`] interpolates the URL without escaping, a page built from a URL
/// containing `"` does not round-trip: the quote ends the `href` attribute value early, the page no
/// longer has the expected shape, and `None` is returned rather than a truncated URL.
#[must_use]
pub fn parse_redirect_html(content: &str) -> Option<&str> {
    static REDIRECT_HTML_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(REDIRECT_HTML_PATTERN).expect("redirect page pattern is valid")
    });

    REDIRECT_HTML_RE
        .captures(content)
        .and_then(|groups| groups.get(1))
        .map(|m| m.as_str())
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        // Representative URLs: plain, query string, percent-escapes, and multibyte characters.
        for url in [
            "https://example.com/",
            "https://example.com/path?a=1&b=2",
            "https://example.com/a%20b?q=%26%23",
            "https://example.com/caf\u{e9}/\u{65e5}\u{672c}\u{8a9e}",
        ] {
            let html = super::make_redirect_html(url);

            assert_eq!(super::parse_redirect_html(&html), Some(url), "{url}");
        }
    }

    #[test]
    fn make_redirect_html_matches_stored_page_shape() {
        // The exact bytes are load-bearing: `resolve_redirect_shallow` in the downloader compares
        // this output's SHA-1 digest against the CDX `digest` field, so the shape (including the
        // absence of HTML escaping) must not change.
        assert_eq!(
            super::make_redirect_html("https://example.com/x"),
            r#"<html><body>You are being <a href="https://example.com/x">redirected</a>.</body></html>"#
        );
    }

    #[test]
    fn round_trip_fails_for_url_containing_quote() {
        // The URL cannot be escaped without breaking digest reproduction, so a `"` in the URL ends
        // the `href` attribute value early; the parse regex's `[^"]+` cannot cross the quote, so
        // the malformed page is rejected outright rather than yielding a truncated URL.
        let html = super::make_redirect_html(r#"https://example.com/a"b"#);

        assert_eq!(super::parse_redirect_html(&html), None);
    }
}
