//! The extended eleven-field CDX index record, adding redirect, robot-flag, and WARC-location
//! fields to the standard item, with deserialization from the JSON array rows.
use std::borrow::Cow;

use archivindex_serde::BorrowableStr;
use serde::de::{Deserialize, Deserializer, IgnoredAny, SeqAccess, Unexpected, Visitor};

use crate::surt::Surt;

const INVALID_LENGTH_MESSAGE: &str = "expected 11 elements";
const ITEM_LIST_HEADER: [&str; 11] = [
    "urlkey",
    "timestamp",
    "original",
    "mimetype",
    "statuscode",
    "digest",
    "redirect",
    "robotflags",
    "length",
    "offset",
    "filename",
];

/// The extended eleven-field CDX index record.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, bounded_static::ToStatic)]
pub struct ExtendedItem<'a> {
    /// The standard seven fields.
    pub item: super::Item<'a>,
    /// The redirect target, if the capture was a redirect (absent when given as `-`).
    pub redirect: Option<Cow<'a, str>>,
    /// The robots.txt flags for the capture (absent when given as `-`).
    pub robot_flags: Option<Cow<'a, str>>,
    /// The capture's byte offset within its WARC file.
    pub offset: u64,
    /// The name of the WARC file containing the capture.
    pub file_name: Cow<'a, str>,
}

// This is an internal representation that we need because of the way resumption keys are given.
enum ItemOrEmpty<'a> {
    Item(Box<ExtendedItem<'a>>),
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
                            super::read_shared_fields(&mut seq, INVALID_LENGTH_MESSAGE)?;

                        let BorrowableStr(redirect_str) = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(6, &INVALID_LENGTH_MESSAGE)
                        })?;

                        let redirect = if redirect_str == "-" {
                            None
                        } else {
                            Some(redirect_str)
                        };

                        let BorrowableStr(robot_flags_str) =
                            seq.next_element()?.ok_or_else(|| {
                                serde::de::Error::invalid_length(7, &INVALID_LENGTH_MESSAGE)
                            })?;

                        let robot_flags = if robot_flags_str == "-" {
                            None
                        } else {
                            Some(robot_flags_str)
                        };

                        let length_str: &str = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(8, &INVALID_LENGTH_MESSAGE)
                        })?;

                        let length = super::parse_length(length_str).ok_or_else(|| {
                            serde::de::Error::invalid_value(Unexpected::Str(length_str), &"length")
                        })?;

                        let offset_str: &str = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(9, &INVALID_LENGTH_MESSAGE)
                        })?;

                        let offset = offset_str.parse().map_err(|_| {
                            serde::de::Error::invalid_value(Unexpected::Str(offset_str), &"offset")
                        })?;

                        let BorrowableStr(file_name) = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(10, &INVALID_LENGTH_MESSAGE)
                        })?;

                        let end: Option<IgnoredAny> = seq.next_element()?;

                        match end {
                            None => Ok(Self::Value::Item(Box::new(ExtendedItem {
                                item: super::Item {
                                    key,
                                    timestamp,
                                    original,
                                    mime_type,
                                    status_code,
                                    digest,
                                    length,
                                },
                                redirect,
                                robot_flags,
                                offset,
                                file_name,
                            }))),
                            Some(_) => Err(serde::de::Error::invalid_length(
                                12,
                                &INVALID_LENGTH_MESSAGE,
                            )),
                        }
                    }
                }
            }
        }

        deserializer.deserialize_seq(ItemOrEmptyVisitor)
    }
}

impl<'de> super::ListRow<'de> for ItemOrEmpty<'de> {
    type Row = ExtendedItem<'de>;

    fn into_item(self) -> Option<Self::Row> {
        match self {
            Self::Item(item) => Some(*item),
            Self::Empty => None,
        }
    }
}

/// A CDX list document: the extended items and an optional resume key for paging.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, bounded_static::ToStatic)]
pub struct ExtendedItemList<'a> {
    /// The document's items, in document order.
    pub values: Vec<ExtendedItem<'a>>,
    /// The resume key for fetching the next page, if one was present.
    pub resume_key: Option<Cow<'a, str>>,
}

impl<'a, 'de: 'a> Deserialize<'de> for ExtendedItemList<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntryListVisitor;

        impl<'de> Visitor<'de> for EntryListVisitor {
            type Value = ExtendedItemList<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct ExtendedItemList")
            }

            fn visit_seq<V: SeqAccess<'de>>(self, seq: V) -> Result<Self::Value, V::Error> {
                let (values, resume_key) =
                    super::visit_item_list::<_, ItemOrEmpty<'de>>(seq, &ITEM_LIST_HEADER)?;

                Ok(ExtendedItemList { values, resume_key })
            }
        }

        deserializer.deserialize_seq(EntryListVisitor)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn deserialize_empty() {
        let contents = "[]";
        let items = serde_json::from_str::<super::ExtendedItemList<'_>>(contents).unwrap();

        assert_eq!(items.values.len(), 0);
    }

    #[test]
    fn deserialize() {
        let contents = include_str!("../../../tests/data/cdx/1702374488385081.json");
        let items = serde_json::from_str::<super::ExtendedItemList<'_>>(contents).unwrap();

        assert_eq!(items.values.len(), 21);
        assert_eq!(items.resume_key, None);
    }

    #[test]
    fn deserialize_with_resume_key() {
        // A paged response ends with an empty row followed by a single-element resume key row.
        let contents = r#"[
            ["urlkey", "timestamp", "original", "mimetype", "statuscode", "digest", "redirect", "robotflags", "length", "offset", "filename"],
            ["com,twitter)/farleftwatch/status/1000004945255522304", "20190622042436", "https://twitter.com/FarLeftWatch/status/1000004945255522304", "text/html", "200", "P4TIU4OJX2CY246KLVUJZGRU3STSJ3TZ", "-", "-", "71613", "1500768660", "example-00005.warc.gz"],
            [],
            ["eJwNxzEOgCAMAMCvuJqYtKViy3MIdGAgGqj6fb3tytk3f5u7jRVKvrw9VoflbkNgevZ7Amkilj0xBoqKCVWWgCH-FTyYWD9RQxSp"]
        ]"#;
        let items = serde_json::from_str::<super::ExtendedItemList<'_>>(contents).unwrap();

        let expected_resume_key = "eJwNxzEOgCAMAMCvuJqYtKViy3MIdGAgGqj6fb3tytk3f5u7jRVKvrw9VoflbkNgevZ7Amkilj0xBoqKCVWWgCH-FTyYWD9RQxSp";

        assert_eq!(items.values.len(), 1);
        assert_eq!(items.values[0].redirect, None);
        assert_eq!(items.values[0].offset, 1_500_768_660);
        assert_eq!(items.values[0].file_name, "example-00005.warc.gz");
        assert_eq!(items.resume_key, Some(expected_resume_key.into()));
    }
}
