//! Construction and parsing of the small HTML redirect pages that the Wayback Machine stores for a
//! capture listed as a 302 redirect.
const PREFIX: &str = "<html><body>You are being <a href=\"";
const SUFFIX: &str = "\">redirected</a>.</body></html>";

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
    format!("{PREFIX}{url}{SUFFIX}")
}

/// Extracts the target URL from a redirect page in the format produced by [`make_redirect_html`],
/// or `None` if the content does not have that exact shape.
///
/// Because [`make_redirect_html`] interpolates the URL without escaping, a page built from a URL
/// containing `"` does not round-trip: the quote ends the `href` attribute value early, the page no
/// longer has the expected shape, and `None` is returned rather than a truncated URL.
#[must_use]
pub fn parse_redirect_html(content: &str) -> Option<&str> {
    let url = content.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    (!url.is_empty() && !url.contains('"')).then_some(url)
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
        // the `href` attribute value early, so
        // the malformed page is rejected outright rather than yielding a truncated URL.
        let html = super::make_redirect_html(r#"https://example.com/a"b"#);

        assert_eq!(super::parse_redirect_html(&html), None);
    }

    #[test]
    fn only_the_exact_nonempty_page_shape_is_accepted() {
        let page = super::make_redirect_html("https://example.com/");
        for malformed in [
            super::make_redirect_html(""),
            format!("{page}\n"),
            format!(" {page}"),
            page.replace("redirected", "Redirected"),
        ] {
            assert_eq!(super::parse_redirect_html(&malformed), None);
        }
    }
}
