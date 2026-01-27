use serde::{
    de::{Deserialize, Deserializer},
    ser::Serializer,
};
use std::borrow::Cow;

const fn is_json_whitespace(candidate: char) -> bool {
    candidate == '\r' || candidate == '\n' || candidate == ' ' || candidate == '\t'
}

pub fn check_closing_whitespace(
    default_closing_whitespace: &[char],
    line: &str,
) -> Option<Vec<char>> {
    let mut is_match = true;
    let mut chars_read = 0;
    let mut reversed_chars = line.chars().rev();

    for whitespace_char in default_closing_whitespace {
        if let Some(next_char) = reversed_chars.next() {
            if *whitespace_char == next_char {
                chars_read += 1;
            } else {
                if is_json_whitespace(next_char) {
                    chars_read += 1;
                }

                is_match = false;
                break;
            }
        } else {
            is_match = false;
            break;
        }
    }

    for next_char in reversed_chars {
        if is_json_whitespace(next_char) {
            chars_read += 1;
        } else {
            break;
        }
    }

    if is_match && chars_read == default_closing_whitespace.len() {
        None
    } else {
        let mut closing_whitespace = line.chars().rev().take(chars_read).collect::<Vec<_>>();
        closing_whitespace.reverse();

        Some(closing_whitespace)
    }
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
            let mut closing_whitespace_str = String::new();

            for whitespace_char in value {
                match whitespace_char {
                    '\r' => closing_whitespace_str.push_str("\\r"),
                    '\n' => closing_whitespace_str.push_str("\\n"),
                    ' ' => closing_whitespace_str.push(' '),
                    '\t' => closing_whitespace_str.push_str("\\t"),
                    other => {
                        return Err(serde::ser::Error::custom(format!(
                            "unexpected whitespace character: {other}"
                        )));
                    }
                }
            }

            serializer.serialize_some(&closing_whitespace_str)
        }
        None => serializer.serialize_none(),
    }
}
