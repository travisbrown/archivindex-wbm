use serde::{
    de::{Deserialize, Deserializer, Unexpected, Visitor},
    ser::{Serialize, Serializer},
};
use std::borrow::Cow;
use std::fmt::Display;
use std::str::FromStr;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Invalid SURT")]
    InvalidSurt(String),
    #[error("Invalid domain part")]
    InvalidDomainPart(String),
    #[error("Invalid URL")]
    InvalidUrl(#[from] url::ParseError),
    #[error("Unexpected URL")]
    UnexpectedUrl(String),
}

/// Simplified Sort-friendly URI Reordering Transform representation.
///
/// Currently only implements features necessary to handle Wayback Machine CDX results.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Surt<'a> {
    source: Cow<'a, str>,
    domain_name_part_lens: Vec<u8>,
}

impl<'a> Surt<'a> {
    #[must_use]
    pub fn as_str(&'a self) -> &'a str {
        &self.source
    }

    fn path_start(&'a self) -> usize {
        self.domain_name_part_lens.len() + self.domain_name_part_lens.iter().sum::<u8>() as usize
    }

    #[must_use]
    pub fn domain_name_parts(&'a self) -> DomainNamePartIter<'a> {
        DomainNamePartIter {
            source: &self.source[0..self.path_start() - 1],
            domain_name_part_lens: self.domain_name_part_lens.iter(),
        }
    }

    #[must_use]
    pub fn path(&'a self) -> &'a str {
        &self.source[self.path_start()..]
    }

    pub fn parse_str(input: &'a str) -> Result<Self, Error> {
        let mut domain_name_part_lens = Vec::with_capacity(2);
        let mut len = 0;

        for ch in input.chars() {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                len += 1;
            } else if ch == ',' {
                domain_name_part_lens.push(len);

                len = 0;
            } else if ch == ')' {
                domain_name_part_lens.push(len);

                break;
            } else {
                return Err(Error::InvalidSurt(input.to_string()));
            }
        }

        Ok(Self {
            source: input.into(),
            domain_name_part_lens,
        })
    }

    #[must_use]
    pub fn into_owned(self) -> Surt<'static> {
        Surt {
            source: self.source.into_owned().into(),
            domain_name_part_lens: self.domain_name_part_lens,
        }
    }

    #[must_use]
    pub const fn canonical_url(&'a self) -> SurtCanonicalUrl<'a> {
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

                source.pop();
                source.push(')');
                source.push_str(&Self::decode_path(url.path()));

                if source.ends_with('/') {
                    source.pop();
                }

                let mut query_pairs = url.query_pairs().collect::<Vec<_>>();

                if !query_pairs.is_empty() {
                    query_pairs.sort_by_key(|(key, _)| key.clone());

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
        Surt::parse_str(s).map(Surt::into_owned)
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

            self.source = &self.source[len..];

            part
        })
    }
}

impl DoubleEndedIterator for DomainNamePartIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.domain_name_part_lens.next_back().map(|len| {
            let len = *len as usize;
            let part = &self.source[self.source.len() - len..];

            self.source = &self.source[0..self.source.len()];

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
        let contents = include_str!("../../examples/cdx/1706619334645856.json");
        let items = serde_json::from_str::<crate::cdx::item::ItemList<'_>>(contents).unwrap();

        for item in items.values {
            let from_url = Surt::from_url(&item.original).unwrap();

            assert_eq!(item.key, from_url);
        }
    }
}
