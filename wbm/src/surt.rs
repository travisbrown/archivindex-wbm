//! A simplified Sort-friendly URI Reordering Transform key, providing the sort-friendly URL
//! representation and domain-part access needed for Wayback Machine CDX results.
use serde::{
    de::{Deserialize, Deserializer, Unexpected, Visitor},
    ser::{Serialize, Serializer},
};
use std::borrow::Cow;
use std::fmt::Display;
use std::str::FromStr;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Invalid SURT: {0}")]
    InvalidSurt(String),
    #[error("Invalid domain part: {0}")]
    InvalidDomainPart(String),
    #[error("Invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("Unexpected URL: {0}")]
    UnexpectedUrl(String),
}

/// Represents a simplified Sort-friendly URI Reordering Transform.
///
/// Currently only implements features necessary to handle Wayback Machine CDX results.
///
/// By construction there will always be at least one domain name part length.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, bounded_static::ToStatic)]
pub struct Surt<'a> {
    source: Cow<'a, str>,
    domain_name_part_lens: Vec<u8>,
}

impl<'a> Surt<'a> {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.source
    }

    fn path_start(&self) -> usize {
        self.domain_name_part_lens.len()
            + self
                .domain_name_part_lens
                .iter()
                .map(|len| usize::from(*len))
                .sum::<usize>()
    }

    #[must_use]
    pub fn domain_name_parts(&self) -> DomainNamePartIter<'_> {
        DomainNamePartIter {
            source: &self.source[0..self.path_start() - 1],
            domain_name_part_lens: self.domain_name_part_lens.iter(),
        }
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.source[self.path_start()..]
    }

    pub fn parse_str(input: &'a str) -> Result<Self, Error> {
        let mut domain_name_part_lens = Vec::with_capacity(2);
        let mut len = 0;

        for ch in input.chars() {
            // Underscores appear in real (nonstandard) hostnames, and a colon marks a port, which
            // the Wayback Machine keeps in the urlkey (e.g. `com,example:8080)/`).
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == ':' {
                if len == u8::MAX {
                    return Err(Error::InvalidSurt(input.to_string()));
                }
                len += 1;
            } else if ch == ',' {
                domain_name_part_lens.push(len);

                len = 0;
            } else if ch == ')' {
                domain_name_part_lens.push(len);

                return Ok(Self {
                    source: input.into(),
                    domain_name_part_lens,
                });
            } else {
                return Err(Error::InvalidSurt(input.to_string()));
            }
        }

        // The domain name list terminator was never seen.
        Err(Error::InvalidSurt(input.to_string()))
    }

    #[must_use]
    pub const fn canonical_url(&self) -> SurtCanonicalUrl<'_> {
        SurtCanonicalUrl { source: self }
    }
}

impl Surt<'static> {
    pub fn from_url(input: &str) -> Result<Self, Error> {
        let url: url::Url = input.to_lowercase().parse()?;

        match (url.scheme(), url.domain()) {
            ("http" | "https", Some(domain_name)) if url.port().is_none() => {
                let mut source = String::new();
                let mut domain_name_part_lens = Vec::with_capacity(2);

                for domain_name_part in domain_name.split('.').rev() {
                    if domain_name_part != "www" {
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

                    Ok(Self {
                        source: source.into(),
                        domain_name_part_lens,
                    })
                }
            }
            _ => Err(Error::UnexpectedUrl(input.to_string())),
        }
    }

    fn decode_path(value: &str) -> String {
        value
            .replace("%22", "\"")
            .replace("%2a", "*")
            .replace("%5c", "\\")
            .replace("%3c", "<")
            .replace("%3e", ">")
            .replace("%27", "'")
            .replace("%7b", "{")
            .replace("%7d", "}")
            .replace('\n', "%0a")
            .replace("//", "/")
    }

    fn decode_query_value(value: &str) -> String {
        value
            .replace('+', "%20")
            .replace(' ', "+")
            .replace('\n', "%0a")
            .replace("%5e", "^")
    }
}

impl Display for Surt<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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

pub struct SurtCanonicalUrl<'a> {
    source: &'a Surt<'a>,
}

impl Display for SurtCanonicalUrl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("https://")?;

        let mut parts = self.source.domain_name_parts();

        let first = parts.next();

        for domain_part in parts.rev() {
            f.write_str(domain_part)?;
            f.write_str(".")?;
        }

        if let Some(first_part) = first {
            f.write_str(first_part)?;
        }

        f.write_str(self.source.path())?;

        Ok(())
    }
}

pub struct DomainNamePartIter<'a> {
    source: &'a str,
    domain_name_part_lens: std::slice::Iter<'a, u8>,
}

impl<'a> Iterator for DomainNamePartIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.domain_name_part_lens.next().map(|len| {
            let len = *len as usize;
            let part = &self.source[0..len];

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
            let len = *len as usize;
            let part = &self.source[self.source.len() - len..];

            // Skip back past the domain part and the comma separator.
            let new_len = self.source.len() - len;
            self.source = if new_len > 0 {
                &self.source[0..new_len - 1]
            } else {
                &self.source[0..new_len]
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

        assert_eq!(parsed.domain_name_parts().count(), 2);

        let printed = parsed.to_string();

        assert_eq!(input, printed);
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
        let contents = include_str!("../../examples/wbm/cdx/1706619334645856.json");
        let items = serde_json::from_str::<crate::cdx::item::ItemList<'_>>(contents).unwrap();

        for item in items.values {
            let from_url = Surt::from_url(&item.original).unwrap();

            assert_eq!(item.key, from_url);
        }
    }

    // Bug #3: Test bidirectional iteration of `DomainNamePartIter`
    #[test]
    fn domain_name_parts_bidirectional_iteration() {
        let input = "com,twitter,api)/v1/endpoint";
        let parsed = input.parse::<Surt<'_>>().unwrap();

        // Test forward iteration
        let parts_forward: Vec<_> = parsed.domain_name_parts().collect();
        assert_eq!(parts_forward, vec!["com", "twitter", "api"]);

        // Test backward iteration
        let parts_backward: Vec<_> = parsed.domain_name_parts().rev().collect();
        assert_eq!(parts_backward, vec!["api", "twitter", "com"]);

        // Test mixed iteration (forward then backward)
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

        // Test reverse iteration with single element
        let parts_rev: Vec<_> = parsed.domain_name_parts().rev().collect();
        assert_eq!(parts_rev, vec!["com"]);
    }

    // Bug #4: Test integer overflow protection in parse_str()
    #[test]
    fn parse_str_with_very_long_domain_part() {
        // Create a domain part longer than u8::MAX (255 characters)
        let long_part = "a".repeat(300);
        let input = format!("{long_part})path");

        let result = Surt::parse_str(&input);
        // Should return an error, not panic or overflow.
        assert!(result.is_err());
        assert!(matches!(result, Err(Error::InvalidSurt(_))));
    }

    #[test]
    fn parse_str_exactly_255_chars() {
        // Domain part with exactly u8::MAX characters
        let part = "a".repeat(255);
        let input = format!("{part})path");

        let result = Surt::parse_str(&input);
        assert!(result.is_ok());
    }

    // Bug #5: Test empty input protection
    #[test]
    fn parse_str_empty_input() {
        let result = Surt::parse_str("");
        assert!(result.is_err());
        assert!(matches!(result, Err(Error::InvalidSurt(_))));
    }

    #[test]
    fn parse_str_missing_terminator() {
        // Without the `)` terminator the final domain part is never delimited, so the input must
        // be rejected rather than producing a value that violates the type's invariants.
        for input in ["abc", "com,twitter"] {
            let result = Surt::parse_str(input);
            assert!(matches!(result, Err(Error::InvalidSurt(_))), "{input}");
        }
    }

    #[test]
    fn from_url_with_no_domain_parts() {
        // URL that results in no domain parts (e.g., after filtering "www")
        let result = Surt::from_url("https://www/path");
        // Should return an error, not panic from underflow.
        assert!(result.is_err());
    }

    #[test]
    fn from_url_with_domain_part_too_long() {
        // Domain with a part longer than 255 characters
        let long_part = "a".repeat(300);
        let url = format!("https://{long_part}.com/path");

        let result = Surt::from_url(&url);
        // Should return an error due to domain part length check.
        assert!(result.is_err());
        assert!(matches!(result, Err(Error::InvalidDomainPart(_))));
    }
}
