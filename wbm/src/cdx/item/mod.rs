use crate::cdx::{mime_type::MimeType, status_code::StatusCode};
use crate::{digest::Digest, surt::Surt, timestamp::Timestamp};
use serde::de::{Deserialize, Deserializer, IgnoredAny, SeqAccess, Unexpected, Visitor};
use std::borrow::Cow;

pub mod extended;

const EXPECTED_ITEM_LIST_LEN: usize = 10_000;
const INVALID_LENGTH_MESSAGE: &str = "expected 7 elements";
const ITEM_LIST_HEADER: [&str; 7] = [
    "urlkey",
    "timestamp",
    "original",
    "mimetype",
    "statuscode",
    "digest",
    "length",
];

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("JSON decoding error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid MIME type")]
    InvalidMimeType(#[from] crate::cdx::mime_type::Error),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, bounded_static_derive_more::ToStatic)]
pub struct Item<'a> {
    pub key: Surt<'a>,
    pub timestamp: Timestamp,
    pub original: Cow<'a, str>,
    pub mime_type: MimeType<'a>,
    pub status_code: StatusCode,
    pub digest: Digest<'a>,
    /// In some cases the length may be negative, for unknown reasons.
    pub length: Option<i64>,
}

// This is an internal representation that we need because of the way resumption keys are given.
enum ItemOrEmpty<'a> {
    Item(Item<'a>),
    Empty,
}

impl<'a, 'de: 'a> Deserialize<'de> for ItemOrEmpty<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ItemOrEmptyVisitor;

        impl<'de> Visitor<'de> for ItemOrEmptyVisitor {
            type Value = ItemOrEmpty<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("enum ItemOrEmpty")
            }

            fn visit_seq<V: SeqAccess<'de>>(self, mut seq: V) -> Result<Self::Value, V::Error> {
                match seq.next_element::<Surt<'_>>()? {
                    None => Ok(Self::Value::Empty),
                    Some(key) => {
                        let timestamp = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(1, &INVALID_LENGTH_MESSAGE)
                        })?;
                        let original = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(2, &INVALID_LENGTH_MESSAGE)
                        })?;
                        let mime_type = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(3, &INVALID_LENGTH_MESSAGE)
                        })?;
                        let status_code = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(4, &INVALID_LENGTH_MESSAGE)
                        })?;
                        let digest = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(5, &INVALID_LENGTH_MESSAGE)
                        })?;
                        let length_str: &str = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(6, &INVALID_LENGTH_MESSAGE)
                        })?;

                        let length = parse_length(length_str).ok_or_else(|| {
                            serde::de::Error::invalid_value(Unexpected::Str(length_str), &"length")
                        })?;

                        let end: Option<IgnoredAny> = seq.next_element()?;

                        match end {
                            None => Ok(Self::Value::Item(Item {
                                key,
                                timestamp,
                                original,
                                mime_type,
                                status_code,
                                digest,
                                length,
                            })),
                            Some(_) => {
                                Err(serde::de::Error::invalid_length(8, &INVALID_LENGTH_MESSAGE))
                            }
                        }
                    }
                }
            }
        }

        deserializer.deserialize_seq(ItemOrEmptyVisitor)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, bounded_static_derive_more::ToStatic)]
pub struct ItemList<'a> {
    pub values: Vec<Item<'a>>,
    pub resume_key: Option<Cow<'a, str>>,
}

impl<'a, 'de: 'a> Deserialize<'de> for ItemList<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntryListVisitor;

        impl<'de> Visitor<'de> for EntryListVisitor {
            type Value = ItemList<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct ItemList")
            }

            fn visit_seq<V: SeqAccess<'de>>(self, mut seq: V) -> Result<Self::Value, V::Error> {
                match seq.next_element::<Vec<&str>>()? {
                    Some(header) => {
                        if header == ITEM_LIST_HEADER {
                            let mut values = Vec::with_capacity(EXPECTED_ITEM_LIST_LEN);

                            let mut expect_resume_key = false;

                            while let Some(next) = seq.next_element::<ItemOrEmpty<'_>>()? {
                                match next {
                                    ItemOrEmpty::Item(item) => {
                                        values.push(item);
                                    }
                                    ItemOrEmpty::Empty => {
                                        expect_resume_key = true;
                                        break;
                                    }
                                }
                            }

                            let resume_key = if expect_resume_key {
                                let (resume_key,) = seq
                                    .next_element()?
                                    .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;

                                Some(resume_key)
                            } else {
                                None
                            };

                            Ok(ItemList { values, resume_key })
                        } else {
                            Err(serde::de::Error::invalid_value(
                                Unexpected::Seq,
                                &"CDX item list header",
                            ))
                        }
                    }
                    None => Ok(ItemList {
                        values: vec![],
                        resume_key: None,
                    }),
                }
            }
        }

        deserializer.deserialize_seq(EntryListVisitor)
    }
}

// Simple internal function, so we don't care what Clippy says.
#[allow(clippy::option_option)]
fn parse_length(input: &str) -> Option<Option<i64>> {
    if input == "-" {
        Some(None)
    } else {
        input.parse::<i64>().ok().map(Some)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn deserialize_empty() {
        let contents = "[]";
        let items = serde_json::from_str::<super::ItemList<'_>>(contents).unwrap();

        assert_eq!(items.values.len(), 0);
    }

    #[test]
    fn deserialize() {
        let contents = include_str!("../../../../examples/wbm/cdx/1706619334645856.json");
        let items = serde_json::from_str::<super::ItemList<'_>>(contents).unwrap();

        assert_eq!(items.values.len(), 37647);
    }

    #[test]
    fn deserialize_with_resume_key() {
        let contents = include_str!("../../../../examples/wbm/cdx/1740396642000000.json");
        let items = serde_json::from_str::<super::ItemList<'_>>(contents).unwrap();

        let expected_resume_key = "eJwNxzEOgCAMAMCvuJqYtKViy3MIdGAgGqj6fb3tytk3f5u7jRVKvrw9VoflbkNgevZ7Amkilj0xBoqKCVWWgCH-FTyYWD9RQxSp";

        assert_eq!(items.values.len(), 100);
        assert_eq!(items.resume_key, Some(expected_resume_key.into()));
    }
}
