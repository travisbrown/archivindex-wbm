//! Serializing and inferring trailing JSON whitespace.
//!
//! A snapshot records the whitespace characters that close its raw content so the original bytes
//! round-trip byte-exact. This module deserializes that whitespace from an escaped string and
//! serializes it back, validating that every character is JSON whitespace (carriage return, line
//! feed, space, or tab).

use serde::{
    de::{Deserialize, Deserializer},
    ser::Serializer,
};
use std::borrow::Cow;

pub const fn is_json_whitespace(candidate: char) -> bool {
    candidate == '\r' || candidate == '\n' || candidate == ' ' || candidate == '\t'
}

pub fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<char>>, D::Error> {
    let closing_whitespace_str: Option<Cow<'_, str>> = Deserialize::deserialize(deserializer)?;

    closing_whitespace_str
        .map(|closing_whitespace_str| {
            let closing_whitespace = closing_whitespace_str
                .chars()
                .filter(|whitespace_char| is_json_whitespace(*whitespace_char))
                .collect::<Vec<_>>();

            // Every JSON whitespace character is single-byte (ASCII), so the number of whitespace
            // characters equals the string's byte length exactly when *every* character was
            // whitespace.
            if closing_whitespace.len() == closing_whitespace_str.len() {
                Ok(closing_whitespace)
            } else {
                Err(serde::de::Error::invalid_value(
                    serde::de::Unexpected::Str(&closing_whitespace_str),
                    &"string of whitespace characters (escaped when necessary)",
                ))
            }
        })
        .map_or(Ok(None), |value| value.map(Some))
}

// We are constrained by the requirements for attribute modules, so Clippy is wrong here.
#[allow(clippy::ref_option)]
pub fn serialize<S: Serializer>(
    value: &Option<Vec<char>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => {
            // Push the actual whitespace characters and let the serializer apply JSON escaping, so
            // `['\r', '\n']` becomes the string value `"\r\n"`. Pushing pre-escaped text here would
            // double-escape under `serde_json` (`"\\r\\n"`) and fail to round-trip.
            let mut closing_whitespace_str = String::new();

            for &whitespace_char in value {
                if is_json_whitespace(whitespace_char) {
                    closing_whitespace_str.push(whitespace_char);
                } else {
                    return Err(serde::ser::Error::custom(format!(
                        "unexpected whitespace character: {whitespace_char}"
                    )));
                }
            }

            serializer.serialize_some(&closing_whitespace_str)
        }
        None => serializer.serialize_none(),
    }
}
