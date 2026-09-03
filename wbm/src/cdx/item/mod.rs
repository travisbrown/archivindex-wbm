//! The standard seven-field CDX index record, with deserialization from the JSON array rows that
//! the Wayback Machine CDX API returns.
use std::borrow::Cow;

use serde::de::{Deserialize, Deserializer, IgnoredAny, SeqAccess, Unexpected, Visitor};

use crate::cdx::mime_type::MimeType;
use crate::cdx::status_code::StatusCode;
use crate::de::BorrowableCow;
use crate::digest::Digest;
use crate::surt::Surt;
use crate::timestamp::Timestamp;

pub mod extended;

/// The row count of a full CDX list page, used to cap pre-allocation of item storage.
const EXPECTED_ITEM_LIST_LEN: usize = 10_000;
/// The item storage pre-allocation used when the deserializer provides no size hint.
const DEFAULT_ITEM_LIST_CAPACITY: usize = 256;
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

/// The standard seven-field CDX index record.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Item<'a> {
    /// The `urlkey` field: the capture's SURT.
    pub key: Surt<'a>,
    /// The capture timestamp.
    pub timestamp: Timestamp,
    /// The original URL of the capture.
    pub original: Cow<'a, str>,
    /// The MIME type reported for the capture.
    pub mime_type: MimeType<'a>,
    /// The HTTP status code reported for the capture.
    pub status_code: StatusCode,
    /// The capture's content digest.
    pub digest: Digest<'a>,
    /// The reported length (absent when given as `-`).
    ///
    /// In some cases the length may be negative, for unknown reasons.
    pub length: Option<i64>,
}

/// The fields between `urlkey` and the format-specific tail that the standard and extended rows
/// share: `timestamp`, `original`, `mimetype`, `statuscode`, and `digest`.
type SharedFields<'a> = (
    Timestamp,
    Cow<'a, str>,
    MimeType<'a>,
    StatusCode,
    Digest<'a>,
);

/// Read the shared fields following an already-read `urlkey`.
fn read_shared_fields<'de, V: SeqAccess<'de>>(
    seq: &mut V,
    invalid_length_message: &'static str,
) -> Result<SharedFields<'de>, V::Error> {
    let timestamp = seq
        .next_element()?
        .ok_or_else(|| serde::de::Error::invalid_length(1, &invalid_length_message))?;
    let BorrowableCow(original) = seq
        .next_element()?
        .ok_or_else(|| serde::de::Error::invalid_length(2, &invalid_length_message))?;
    let mime_type = seq
        .next_element()?
        .ok_or_else(|| serde::de::Error::invalid_length(3, &invalid_length_message))?;
    let status_code = seq
        .next_element()?
        .ok_or_else(|| serde::de::Error::invalid_length(4, &invalid_length_message))?;
    let digest = seq
        .next_element()?
        .ok_or_else(|| serde::de::Error::invalid_length(5, &invalid_length_message))?;

    Ok((timestamp, original, mime_type, status_code, digest))
}

/// A row in a CDX list document: an item, or the empty row that precedes a trailing resume key.
trait ListRow<'de>: Deserialize<'de> {
    /// The item type produced for non-empty rows.
    type Row;

    /// The row's item, or `None` for the empty row.
    fn into_item(self) -> Option<Self::Row>;
}

/// A CDX list document's contents: its items and an optional trailing resume key.
type ListParts<'a, T> = (Vec<T>, Option<Cow<'a, str>>);

/// Shared deserialization for the two CDX list document shapes: a header row, item rows, and an
/// optional trailing resume key preceded by an empty row.
///
/// The item storage is pre-allocated using the sequence's size hint when one is available (capped
/// at the row count of a full page), and a modest default otherwise.
fn visit_item_list<'de, V: SeqAccess<'de>, R: ListRow<'de>>(
    mut seq: V,
    expected_header: &[&str],
) -> Result<ListParts<'de, R::Row>, V::Error> {
    match seq.next_element::<Vec<&str>>()? {
        Some(header) => {
            if header == expected_header {
                // The hint counts the remaining rows (at most two of which are resume-key
                // bookkeeping rather than items), so it is a close upper bound on the item count;
                // capping it keeps an oversized hint from over-allocating.
                let capacity = seq.size_hint().map_or(DEFAULT_ITEM_LIST_CAPACITY, |hint| {
                    hint.min(EXPECTED_ITEM_LIST_LEN)
                });
                let mut values = Vec::with_capacity(capacity);
                let mut expect_resume_key = false;

                while let Some(next) = seq.next_element::<R>()? {
                    if let Some(item) = next.into_item() {
                        values.push(item);
                    } else {
                        expect_resume_key = true;
                        break;
                    }
                }

                let resume_key = if expect_resume_key {
                    let (BorrowableCow(resume_key),) = seq
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::invalid_length(0, &"a resume key row"))?;

                    Some(resume_key)
                } else {
                    None
                };

                Ok((values, resume_key))
            } else {
                Err(serde::de::Error::invalid_value(
                    Unexpected::Seq,
                    &"CDX item list header",
                ))
            }
        }
        None => Ok((Vec::new(), None)),
    }
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
                        let (timestamp, original, mime_type, status_code, digest) =
                            read_shared_fields(&mut seq, INVALID_LENGTH_MESSAGE)?;

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

impl<'de> ListRow<'de> for ItemOrEmpty<'de> {
    type Row = Item<'de>;

    fn into_item(self) -> Option<Self::Row> {
        match self {
            Self::Item(item) => Some(item),
            Self::Empty => None,
        }
    }
}

/// A CDX list document: the standard items and an optional resume key for paging.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, bounded_static::ToStatic)]
pub struct ItemList<'a> {
    /// The document's items, in document order.
    pub values: Vec<Item<'a>>,
    /// The resume key for fetching the next page, if one was present.
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

            fn visit_seq<V: SeqAccess<'de>>(self, seq: V) -> Result<Self::Value, V::Error> {
                let (values, resume_key) =
                    visit_item_list::<_, ItemOrEmpty<'de>>(seq, &ITEM_LIST_HEADER)?;

                Ok(ItemList { values, resume_key })
            }
        }

        deserializer.deserialize_seq(EntryListVisitor)
    }
}

impl bounded_static::ToBoundedStatic for Item<'_> {
    type Static = Item<'static>;

    fn to_static(&self) -> Self::Static {
        Item {
            key: self.key.to_static(),
            timestamp: self.timestamp,
            original: self.original.to_static(),
            mime_type: self.mime_type.to_static(),
            status_code: self.status_code,
            digest: self.digest.to_static(),
            length: self.length,
        }
    }
}

impl bounded_static::IntoBoundedStatic for Item<'_> {
    type Static = Item<'static>;

    fn into_static(self) -> Self::Static {
        Item {
            key: self.key.into_static(),
            timestamp: self.timestamp,
            original: self.original.into_static(),
            mime_type: self.mime_type.into_static(),
            status_code: self.status_code,
            digest: self.digest.into_static(),
            length: self.length,
        }
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
        let contents = include_str!("../../../tests/data/cdx/1706619334645856.json");
        let items = serde_json::from_str::<super::ItemList<'_>>(contents).unwrap();

        assert_eq!(items.values.len(), 29);
        // The `original` field must borrow from the input rather than allocating.
        assert!(matches!(
            items.values[0].original,
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn deserialize_with_resume_key() {
        let contents = include_str!("../../../tests/data/cdx/1740396642000000.json");
        let items = serde_json::from_str::<super::ItemList<'_>>(contents).unwrap();

        let expected_resume_key = "eJwNxzEOgCAMAMCvuJqYtKViy3MIdGAgGqj6fb3tytk3f5u7jRVKvrw9VoflbkNgevZ7Amkilj0xBoqKCVWWgCH-FTyYWD9RQxSp";

        assert_eq!(items.values.len(), 100);
        assert_eq!(items.resume_key, Some(expected_resume_key.into()));
    }
}
