//! A simplified Sort-friendly URI Reordering Transform key, providing the sort-friendly URL
//! representation and domain-part access needed for Wayback Machine CDX results.
use std::borrow::Cow;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use serde::de::{Deserialize, Deserializer, Unexpected, Visitor};
use serde::ser::{Serialize, Serializer};

/// An error encountered while building a [`Surt`], either from a SURT string or from a URL.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// The input was not a well-formed SURT.
    ///
    /// This covers a missing `)` terminator, an empty domain name part (as in `com,)/x` or
    /// `,com)/x`), a domain name part longer than [`u8::MAX`], and any character before the
    /// terminator that is not alphanumeric, `-`, `_`, `:`, or `,`.
    #[error("invalid SURT: {0}")]
    InvalidSurt(String),
    /// A label of the URL's host was longer than [`u8::MAX`], so its length could not be recorded.
    #[error("invalid domain part: {0}")]
    InvalidDomainPart(String),
    /// The input could not be parsed as a URL at all.
    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    /// The input parsed as a URL that has no supported SURT form.
    ///
    /// This covers schemes other than `http` and `https`, URLs with a non-default port, hosts that
    /// are IP addresses rather than domain names, and hosts that leave no labels once `www` and
    /// empty labels are dropped (as in `https://www/path`).
    #[error("unexpected URL: {0}")]
    UnexpectedUrl(String),
}

/// Represents a simplified Sort-friendly URI Reordering Transform.
///
/// Currently only implements features necessary to handle Wayback Machine CDX results.
///
/// By construction there will always be at least one domain name part.
#[derive(Clone, bounded_static::ToStatic)]
pub struct Surt<'a> {
    representation: Representation<'a>,
    domain_name_part_lens: Vec<u8>,
}

/// Most keys use the shared representation. The fallback preserves the legacy parser's acceptance
/// of non-numeric ports and whitespace or control characters after the host terminator.
#[derive(Clone, Debug, bounded_static::ToStatic)]
enum Representation<'a> {
    Shared(archivindex_surt::Surt<'a>),
    Legacy(Cow<'a, str>),
}

impl<'a> Surt<'a> {
    /// Borrows the whole SURT, in the form it takes in the `urlkey` field of a CDX record.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match &self.representation {
            Representation::Shared(surt) => surt.as_str(),
            Representation::Legacy(source) => source,
        }
    }

    fn path_start(&self) -> usize {
        self.domain_name_part_lens.len()
            + self
                .domain_name_part_lens
                .iter()
                .map(|len| usize::from(*len))
                .sum::<usize>()
    }

    /// Iterates over the comma-separated domain name parts, in SURT order.
    ///
    /// SURT order is the reverse of the order the labels take in a host name, so
    /// `com,twitter,api)/x` yields `com`, then `twitter`, then `api`. There is always at least one
    /// part. The last part yielded corresponds to the leftmost label of the host and is the one
    /// that carries a port, if any (as in `com,example:8080)`).
    ///
    /// The returned iterator is double-ended, so [`rev`](Iterator::rev) recovers host order.
    #[must_use]
    pub fn domain_name_parts(&self) -> DomainNamePartIter<'_> {
        DomainNamePartIter {
            source: &self.as_str()[..self.path_start() - 1],
            domain_name_part_lens: self.domain_name_part_lens.iter(),
        }
    }

    /// Borrows everything after the `)` that terminates the domain name parts.
    ///
    /// This is the canonicalized path together with the canonicalized, key-sorted query string, if
    /// there is one. It is empty for a SURT that names only a host, such as `com,example)`.
    /// [`from_url`](Surt::from_url) removes the path's trailing slash. [`parse_str`](Self::parse_str)
    /// preserves the supplied text, including trailing slashes.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.as_str()[self.path_start()..]
    }

    /// Parses a SURT string, borrowing from the input.
    ///
    /// The input is validated and indexed but not otherwise transformed: it is assumed to be
    /// already canonicalized, as the `urlkey` field of a CDX record is. Use
    /// [`from_url`](Surt::from_url) to canonicalize an ordinary URL instead.
    pub fn parse_str(input: &'a str) -> Result<Self, Error> {
        let mut domain_name_part_lens = Vec::with_capacity(2);
        let mut len = 0;

        for ch in input.chars() {
            // Underscores appear in real (non-standard) hostnames, and a colon marks a port, which
            // the Wayback Machine keeps in the `urlkey` field (e.g. `com,example:8080)/`).
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == ':' {
                if len == u8::MAX {
                    return Err(Error::InvalidSurt(input.to_string()));
                }
                len += 1;
            } else if ch == ',' {
                // An empty domain part (e.g. `com,)` or `,com)`) would render as an invalid
                // canonical URL, so it is rejected here to preserve the type's invariants.
                if len == 0 {
                    return Err(Error::InvalidSurt(input.to_string()));
                }

                domain_name_part_lens.push(len);
                len = 0;
            } else if ch == ')' {
                if len == 0 {
                    return Err(Error::InvalidSurt(input.to_string()));
                }

                domain_name_part_lens.push(len);

                return Ok(Self::from_validated(
                    Cow::Borrowed(input),
                    domain_name_part_lens,
                ));
            } else {
                return Err(Error::InvalidSurt(input.to_string()));
            }
        }

        // The domain name list terminator was never seen.
        Err(Error::InvalidSurt(input.to_string()))
    }

    fn from_validated(source: Cow<'a, str>, domain_name_part_lens: Vec<u8>) -> Self {
        let representation = match source {
            Cow::Borrowed(source) => archivindex_surt::Surt::parse(source).map_or_else(
                |_| Representation::Legacy(Cow::Borrowed(source)),
                Representation::Shared,
            ),
            Cow::Owned(source) => source
                .parse::<archivindex_surt::Surt<'static>>()
                .map_or_else(
                    |_| Representation::Legacy(Cow::Owned(source)),
                    Representation::Shared,
                ),
        };

        Self {
            representation,
            domain_name_part_lens,
        }
    }

    /// Views the SURT as the `https` URL it was derived from.
    ///
    /// The result is a display adapter rather than a string, so no allocation happens until it is
    /// formatted. The reconstruction is lossy in the ways the SURT transform itself is lossy: the
    /// scheme is always `https`, a dropped `www` label is not restored, and query parameters remain
    /// sorted by key.
    #[must_use]
    pub const fn canonical_url(&self) -> SurtCanonicalUrl<'_> {
        SurtCanonicalUrl { source: self }
    }
}

impl Surt<'static> {
    /// Computes the SURT for a URL, applying the Wayback Machine's canonicalization.
    ///
    /// The transform lowercases the whole URL, reverses the host's labels and joins them with
    /// commas, drops a `www` label and any empty label left by a trailing dot, and terminates the
    /// host with `)`. The path then has runs of slashes collapsed, a small set of percent-escapes
    /// that the Wayback Machine leaves literal decoded, and any trailing slash removed. Query
    /// parameters are sorted by key and re-encoded, and a parameter with an empty value keeps its
    /// `=`.
    ///
    /// Only `http` and `https` URLs with a domain name host are accepted. Non-default ports are
    /// rejected; an explicit default port is normalized away by the URL parser.
    pub fn from_url(input: &str) -> Result<Self, Error> {
        let url: url::Url = input.to_lowercase().parse()?;

        match (url.scheme(), url.domain()) {
            ("http" | "https", Some(domain_name)) if url.port().is_none() => {
                let mut source = String::new();
                let mut domain_name_part_lens = Vec::with_capacity(2);

                for domain_name_part in domain_name.split('.').rev() {
                    // A fully-qualified host like `example.com.` splits into an empty final part;
                    // the Wayback Machine's canonicalization strips such trailing dots, so empty
                    // parts are skipped here rather than producing keys like `,com,example)/`.
                    if domain_name_part != "www" && !domain_name_part.is_empty() {
                        source.push_str(domain_name_part);
                        source.push(',');

                        domain_name_part_lens.push(
                            domain_name_part.len().try_into().map_err(|_| {
                                Error::InvalidDomainPart(domain_name_part.to_string())
                            })?,
                        );
                    }
                }

                if domain_name_part_lens.is_empty() {
                    Err(Error::UnexpectedUrl(input.to_string()))
                } else {
                    source.pop();
                    source.push(')');
                    source.push_str(&Self::decode_path(url.path()));

                    if source.ends_with('/') {
                        source.pop();
                    }

                    let mut query_pairs = url.query_pairs().collect::<Vec<_>>();

                    if !query_pairs.is_empty() {
                        query_pairs.sort_by(|(a, _), (b, _)| a.cmp(b));

                        source.push('?');

                        let mut first = true;

                        for (key, value) in query_pairs {
                            if first {
                                first = false;
                            } else {
                                source.push('&');
                            }

                            source.push_str(&key);
                            source.push('=');

                            if !value.is_empty() {
                                source.push_str(&Self::decode_query_value(&value));
                            }
                        }
                    }

                    Ok(Self::from_validated(
                        Cow::Owned(source),
                        domain_name_part_lens,
                    ))
                }
            }
            _ => Err(Error::UnexpectedUrl(input.to_string())),
        }
    }

    /// Decode the percent-escapes that the Wayback Machine's canonicalization leaves literal, and
    /// collapse runs of slashes, in a single pass over the (still percent-encoded) path.
    ///
    /// The URL was lowercased before parsing, but the `url` crate re-encodes reserved path
    /// characters with uppercase hex digits, so escapes are matched case-insensitively and any that
    /// stay encoded are emitted with lowercase hex digits to keep the key all-lowercase.
    fn decode_path(value: &str) -> String {
        let mut decoded = Vec::with_capacity(value.len());
        let bytes = value.as_bytes();
        let mut i = 0;

        while i < bytes.len() {
            let byte = bytes[i];

            if byte == b'%'
                && let Some(escape) = bytes.get(i + 1..i + 3)
                && escape.iter().all(u8::is_ascii_hexdigit)
            {
                let escape = [
                    escape[0].to_ascii_lowercase(),
                    escape[1].to_ascii_lowercase(),
                ];

                let replacement = match &escape {
                    b"22" => Some(b'"'),
                    b"27" => Some(b'\''),
                    b"2a" => Some(b'*'),
                    b"3c" => Some(b'<'),
                    b"3e" => Some(b'>'),
                    b"5c" => Some(b'\\'),
                    b"7b" => Some(b'{'),
                    b"7d" => Some(b'}'),
                    _ => None,
                };

                if let Some(replacement) = replacement {
                    decoded.push(replacement);
                } else {
                    decoded.push(b'%');
                    decoded.extend_from_slice(&escape);
                }

                i += 3;
                continue;
            }

            // Runs of slashes collapse to a single slash (a non-ASCII byte can never equal `/`, so
            // copying multi-byte characters byte by byte is safe here).
            if byte != b'/' || decoded.last() != Some(&b'/') {
                decoded.push(byte);
            }
            i += 1;
        }

        String::from_utf8(decoded).expect("ASCII-only substitutions preserve UTF-8")
    }

    /// Re-encode a percent-decoded query value the way the Wayback Machine's canonicalization does,
    /// in a single pass.
    fn decode_query_value(value: &str) -> String {
        let mut decoded = Vec::with_capacity(value.len());
        let bytes = value.as_bytes();
        let mut i = 0;

        while i < bytes.len() {
            match bytes[i] {
                b'+' => decoded.extend_from_slice(b"%20"),
                b' ' => decoded.push(b'+'),
                b'\n' => decoded.extend_from_slice(b"%0a"),
                // These are structural characters, so `query_pairs` only yields them decoded when
                // the source had them percent-encoded; emitting them raw would corrupt the key's
                // `key=value&...` structure, and the Wayback Machine's canonicalization keeps them
                // encoded (with the lowercase hex digits the key format uses).
                b'&' => decoded.extend_from_slice(b"%26"),
                b'#' => decoded.extend_from_slice(b"%23"),
                b'%' if bytes.get(i + 1..i + 3) == Some(b"5e") => {
                    decoded.push(b'^');
                    i += 3;
                    continue;
                }
                byte => decoded.push(byte),
            }
            i += 1;
        }

        String::from_utf8(decoded).expect("ASCII-only substitutions preserve UTF-8")
    }
}

impl Display for Surt<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Debug for Surt<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surt")
            .field("source", &self.as_str())
            .field("domain_name_part_lens", &self.domain_name_part_lens)
            .finish()
    }
}

impl PartialEq for Surt<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Surt<'_> {}

impl PartialOrd for Surt<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Surt<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for Surt<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
        self.domain_name_part_lens.hash(state);
    }
}

impl FromStr for Surt<'static> {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Surt::parse_str(s).map(bounded_static::IntoBoundedStatic::into_static)
    }
}

impl<'de> Deserialize<'de> for Surt<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SurtVisitor;

        impl<'de> Visitor<'de> for SurtVisitor {
            type Value = Surt<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct Surt")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse()
                    .map_err(|_| serde::de::Error::invalid_value(Unexpected::Str(v), &self))
            }

            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                v: &'de str,
            ) -> Result<Self::Value, E> {
                Self::Value::parse_str(v)
                    .map_err(|_| serde::de::Error::invalid_value(Unexpected::Str(v), &self))
            }
        }

        deserializer.deserialize_str(SurtVisitor)
    }
}

impl Serialize for Surt<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A display adapter that renders a [`Surt`] as an `https` URL.
///
/// This is produced by [`Surt::canonical_url`], and does its work in its [`Display`]
/// implementation: the domain name parts are written in reverse SURT order and dot-separated, a
/// port carried by the leftmost label is moved after the host, and the SURT's path is appended
/// unchanged.
pub struct SurtCanonicalUrl<'a> {
    source: &'a Surt<'a>,
}

impl Display for SurtCanonicalUrl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        /// The final domain part of a SURT may carry a port (e.g. `com,example:8080)`), which
        /// belongs after the whole host in URL form, not inside its label.
        fn split_port(domain_part: &str) -> (&str, Option<&str>) {
            match domain_part.split_once(':') {
                Some((label, port_number)) => (label, Some(port_number)),
                None => (domain_part, None),
            }
        }

        f.write_str("https://")?;

        let mut parts = self.source.domain_name_parts();
        let first = parts.next();
        let mut parts = parts.rev();
        let mut port = None;

        // The parts are written in reverse SURT order (`com,example` renders as `example.com`), so
        // the possibly port-bearing final part is the first one written.
        if let Some(leftmost) = parts.next() {
            let (label, found_port) = split_port(leftmost);
            port = found_port;

            f.write_str(label)?;
            f.write_str(".")?;

            for domain_part in parts {
                f.write_str(domain_part)?;
                f.write_str(".")?;
            }

            if let Some(first_part) = first {
                f.write_str(first_part)?;
            }
        } else if let Some(only_part) = first {
            let (label, found_port) = split_port(only_part);
            port = found_port;

            f.write_str(label)?;
        }

        if let Some(port) = port {
            f.write_str(":")?;
            f.write_str(port)?;
        }

        f.write_str(self.source.path())?;

        Ok(())
    }
}

/// A double-ended iterator over the domain name parts of a [`Surt`], in SURT order.
///
/// This is produced by [`Surt::domain_name_parts`]. It walks the recorded part lengths rather than
/// searching for commas, so it never allocates and yields slices borrowed from the SURT itself.
/// Because the two ends consume from a shared length list, forward and backward iteration can be
/// interleaved and together yield each part exactly once.
pub struct DomainNamePartIter<'a> {
    source: &'a str,
    domain_name_part_lens: std::slice::Iter<'a, u8>,
}

impl<'a> Iterator for DomainNamePartIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.domain_name_part_lens.next().map(|len| {
            let len = usize::from(*len);
            let part = &self.source[..len];

            // Skip past the domain part and the comma separator.
            self.source = if self.source.len() > len {
                &self.source[len + 1..]
            } else {
                &self.source[len..]
            };

            part
        })
    }
}

impl DoubleEndedIterator for DomainNamePartIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.domain_name_part_lens.next_back().map(|len| {
            let len = usize::from(*len);
            let part = &self.source[self.source.len() - len..];

            // Skip back past the domain part and the comma separator.
            let new_len = self.source.len() - len;
            self.source = if new_len > 0 {
                &self.source[..new_len - 1]
            } else {
                &self.source[..new_len]
            };

            part
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let input = "com,twitter)/farleftwatch/status/999825423977639936";
        let parsed = input.parse::<Surt<'_>>().unwrap();

        assert!(matches!(&parsed.representation, Representation::Shared(_)));
        assert_eq!(parsed.domain_name_parts().count(), 2);
        assert_eq!(
            format!("{parsed:?}"),
            "Surt { source: \"com,twitter)/farleftwatch/status/999825423977639936\", domain_name_part_lens: [3, 7] }"
        );

        let printed = parsed.to_string();

        assert_eq!(input, printed);
    }

    #[test]
    fn preserves_legacy_only_keys() {
        let input = "com,example:not-a-port)/a b";
        let parsed = Surt::parse_str(input).unwrap();

        assert!(matches!(&parsed.representation, Representation::Legacy(_)));
        assert_eq!(parsed.as_str(), input);
        assert_eq!(parsed.path(), "/a b");
        assert_eq!(
            parsed.canonical_url().to_string(),
            "https://example.com:not-a-port/a b"
        );
    }

    #[test]
    fn from_url() {
        let input = "https://twitter.com/RichardBSpencer/";
        let surt = Surt::from_url(input).unwrap();
        let expected = "com,twitter)/richardbspencer".parse().unwrap();

        assert_eq!(surt, expected);
    }

    #[test]
    fn canonical_url() {
        let input = "com,twitter)/farleftwatch/status/999825423977639936";

        let parsed = input.parse::<Surt<'_>>().unwrap();
        let expected = "https://twitter.com/farleftwatch/status/999825423977639936";

        assert_eq!(parsed.canonical_url().to_string(), expected);
    }

    #[test]
    fn from_url_examples() {
        let contents = include_str!("../tests/data/cdx/1706619334645856.json");
        let items = serde_json::from_str::<crate::cdx::item::ItemList<'_>>(contents).unwrap();

        for item in items.values {
            let from_url = Surt::from_url(&item.original).unwrap();

            assert_eq!(item.key, from_url);
        }
    }

    #[test]
    fn domain_name_parts_bidirectional_iteration() {
        let input = "com,twitter,api)/v1/endpoint";
        let parsed = input.parse::<Surt<'_>>().unwrap();

        let parts_forward: Vec<_> = parsed.domain_name_parts().collect();
        assert_eq!(parts_forward, vec!["com", "twitter", "api"]);

        let parts_backward: Vec<_> = parsed.domain_name_parts().rev().collect();
        assert_eq!(parts_backward, vec!["api", "twitter", "com"]);

        let mut iter = parsed.domain_name_parts();
        assert_eq!(iter.next(), Some("com"));
        assert_eq!(iter.next_back(), Some("api"));
        assert_eq!(iter.next(), Some("twitter"));
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
    }

    #[test]
    fn domain_name_parts_single_part() {
        let input = "com)/path";
        let parsed = input.parse::<Surt<'_>>().unwrap();

        let parts: Vec<_> = parsed.domain_name_parts().collect();
        assert_eq!(parts, vec!["com"]);

        // Test reverse iteration with single element.
        let parts_rev: Vec<_> = parsed.domain_name_parts().rev().collect();
        assert_eq!(parts_rev, vec!["com"]);
    }

    #[test]
    fn parse_str_with_very_long_domain_part() {
        // Create a domain part longer than `u8::MAX` (255 characters).
        let long_part = "a".repeat(300);
        let input = format!("{long_part})path");

        let result = Surt::parse_str(&input);
        // Should return an error, not panic or overflow.
        assert!(result.is_err());
        assert!(matches!(result, Err(Error::InvalidSurt(_))));
    }

    #[test]
    fn parse_str_exactly_255_chars() {
        // Domain part with exactly `u8::MAX` characters.
        let part = "a".repeat(255);
        let input = format!("{part})path");

        let result = Surt::parse_str(&input);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_str_empty_input() {
        let result = Surt::parse_str("");
        assert!(result.is_err());
        assert!(matches!(result, Err(Error::InvalidSurt(_))));
    }

    #[test]
    fn parse_str_missing_terminator() {
        // Without the `)` terminator the final domain part is never delimited, so the input must be
        // rejected rather than producing a value that violates the type's invariants.
        for input in ["abc", "com,twitter"] {
            let result = Surt::parse_str(input);
            assert!(matches!(result, Err(Error::InvalidSurt(_))), "{input}");
        }
    }

    #[test]
    fn parse_str_empty_domain_part() {
        // Empty domain parts would render as invalid canonical URLs (e.g. `https://.com/x`).
        for input in ["com,)/x", ",com)/x", ")/x", "com,,example)/x"] {
            let result = Surt::parse_str(input);
            assert!(matches!(result, Err(Error::InvalidSurt(_))), "{input}");
        }
    }

    #[test]
    fn from_url_with_trailing_dot_host() {
        // The Wayback Machine's canonicalization strips trailing dots from fully-qualified hosts,
        // so the key must match the one produced for the dotless form.
        let surt = Surt::from_url("https://example.com./x").unwrap();
        let expected = "com,example)/x".parse().unwrap();

        assert_eq!(surt, expected);
    }

    #[test]
    fn from_url_with_no_domain_parts() {
        // URL that results in no domain parts (e.g., after filtering `"www"`).
        let result = Surt::from_url("https://www/path");
        // Should return an error, not panic from underflow.
        assert!(result.is_err());
    }

    #[test]
    fn canonical_url_with_port() {
        // A port in the final domain part (as the Wayback Machine keeps it in `urlkey`) must render
        // after the whole host, not inside its leftmost label.
        let parsed = "com,example:8080)/path".parse::<Surt<'_>>().unwrap();
        assert_eq!(
            parsed.canonical_url().to_string(),
            "https://example.com:8080/path"
        );

        let single = "localhost:8080)/x".parse::<Surt<'_>>().unwrap();
        assert_eq!(
            single.canonical_url().to_string(),
            "https://localhost:8080/x"
        );
    }

    #[test]
    fn from_url_collapses_slash_runs() {
        // Runs of slashes of any length collapse fully (a single replacement pass would only halve
        // them).
        let surt = Surt::from_url("https://example.com/a///b").unwrap();
        let expected = "com,example)/a/b".parse().unwrap();

        assert_eq!(surt, expected);
    }

    #[test]
    fn from_url_path_escapes_are_case_insensitive() {
        // The `url` crate re-encodes reserved path characters with uppercase hex, while a
        // pre-encoded input (lowercased by `from_url`) carries lowercase hex; both forms of the
        // same logical URL must produce the same all-lowercase key.
        let raw = Surt::from_url("https://example.com/a{b}").unwrap();
        let encoded = Surt::from_url("https://example.com/a%7bb%7d").unwrap();
        let expected = "com,example)/a{b}".parse().unwrap();

        assert_eq!(raw, expected);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn from_url_keeps_encoded_query_structural_characters() {
        // A `%26` in a query value must stay percent-encoded: decoding it to a raw `&` would inject
        // a bogus parameter boundary into the key.
        let surt = Surt::from_url("https://example.com/x?a=1%262&b=3").unwrap();
        let expected = "com,example)/x?a=1%262&b=3".parse().unwrap();

        assert_eq!(surt, expected);
    }

    #[test]
    fn from_url_with_domain_part_too_long() {
        // Domain with a part longer than 255 characters.
        let long_part = "a".repeat(300);
        let url = format!("https://{long_part}.com/path");

        let result = Surt::from_url(&url);
        // Should return an error due to domain part length check.
        assert!(result.is_err());
        assert!(matches!(result, Err(Error::InvalidDomainPart(_))));
    }
}
