use crate::surt::Surt;
use serde::de::{Deserialize, Deserializer, IgnoredAny, SeqAccess, Unexpected, Visitor};
use std::borrow::Cow;

const EXPECTED_ITEM_LIST_LEN: usize = 10_000;
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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, bounded_static_derive_more::ToStatic)]
pub struct ExtendedItem<'a> {
    pub item: super::Item<'a>,
    pub redirect: Option<Cow<'a, str>>,
    pub robot_flags: Option<Cow<'a, str>>,
    pub offset: u64,
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

                        let redirect_str: Cow<'_, str> = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(6, &INVALID_LENGTH_MESSAGE)
                        })?;

                        let redirect = if redirect_str == "-" {
                            None
                        } else {
                            Some(redirect_str)
                        };

                        let robot_flags_str: Cow<'_, str> =
                            seq.next_element()?.ok_or_else(|| {
                                serde::de::Error::invalid_length(7, &INVALID_LENGTH_MESSAGE)
                            })?;

                        let robot_flags = if robot_flags_str == "-" {
                            None
                        } else {
                            Some(robot_flags_str)
                        };

                        let length_str: &str = seq
                            .next_element()?
                            .ok_or_else(|| serde::de::Error::invalid_length(8, &self))?;

                        let length = super::parse_length(length_str).ok_or_else(|| {
                            serde::de::Error::invalid_value(Unexpected::Str(length_str), &"length")
                        })?;

                        let offset_str: &str = seq.next_element()?.ok_or_else(|| {
                            serde::de::Error::invalid_length(9, &INVALID_LENGTH_MESSAGE)
                        })?;

                        let offset = offset_str.parse().map_err(|_| {
                            serde::de::Error::invalid_value(Unexpected::Str(offset_str), &"offset")
                        })?;

                        let file_name = seq.next_element()?.ok_or_else(|| {
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

pub struct ExtendedItemList<'a> {
    pub values: Vec<ExtendedItem<'a>>,
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

            fn visit_seq<V: SeqAccess<'de>>(self, mut seq: V) -> Result<Self::Value, V::Error> {
                match seq.next_element::<Vec<&str>>()? {
                    Some(header) => {
                        if header == ITEM_LIST_HEADER {
                            let mut values = Vec::with_capacity(EXPECTED_ITEM_LIST_LEN);

                            let mut expect_resume_key = false;

                            while let Some(next) = seq.next_element::<ItemOrEmpty<'_>>()? {
                                match next {
                                    ItemOrEmpty::Item(item) => {
                                        values.push(*item);
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

                            Ok(ExtendedItemList { values, resume_key })
                        } else {
                            Err(serde::de::Error::invalid_value(
                                Unexpected::Seq,
                                &"CDX item list header",
                            ))
                        }
                    }
                    None => Ok(ExtendedItemList {
                        values: vec![],
                        resume_key: None,
                    }),
                }
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
        let contents = include_str!("../../../../examples/cdx/1702374488385081.json");
        let items = serde_json::from_str::<super::ExtendedItemList<'_>>(contents).unwrap();

        assert_eq!(items.values.len(), 8838);
    }
}
