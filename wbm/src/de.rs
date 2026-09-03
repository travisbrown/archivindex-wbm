//! Shared `serde` deserialization helpers.
//!
//! The types in this crate are all serialized as plain strings, so their `Deserialize`
//! implementations are written by hand rather than derived. This module holds the pieces more
//! than one of them needs: [`from_str`] for values validated by [`FromStr`], and
//! [`BorrowableCow`] for string fields that should borrow from the input where the format allows
//! it.
use std::borrow::Cow;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::de::{Deserialize, Deserializer, Unexpected, Visitor};

/// Deserialize a value from a string using its [`FromStr`] implementation.
///
/// # Arguments
///
/// * `deserializer` - The deserializer to read a string from
/// * `expecting` - A description of the expected value, used as the "expected" half of the error
///   message when parsing fails (for example `"struct Timestamp"`)
///
/// # Returns
///
/// The parsed value
///
/// # Errors
///
/// Returns the deserializer's own error if the input is not a string, or an
/// [`invalid_value`](serde::de::Error::invalid_value) error if [`FromStr`] rejects it.
pub fn from_str<'de, T: FromStr, D: Deserializer<'de>>(
    deserializer: D,
    expecting: &'static str,
) -> Result<T, D::Error> {
    deserializer.deserialize_str(FromStrVisitor {
        expecting,
        value: PhantomData,
    })
}

/// Visits a string and parses it with [`FromStr`], discarding the parse error in favour of
/// `serde`'s own "invalid value" message.
struct FromStrVisitor<T> {
    expecting: &'static str,
    /// `PhantomData` records that this visitor produces a `T` without storing one; a type
    /// parameter that appears in no field would otherwise be rejected by the compiler.
    value: PhantomData<T>,
}

impl<T: FromStr> Visitor<'_> for FromStrVisitor<T> {
    type Value = T;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.expecting)
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        v.parse()
            .map_err(|_| serde::de::Error::invalid_value(Unexpected::Str(v), &self))
    }
}

/// A `Cow<str>` that borrows from the deserializer input when possible.
///
/// Serde's stock `Cow` deserialization always produces `Cow::Owned`; this wrapper implements the
/// zero-copy path for borrowed input (the common case when parsing a response held in memory).
///
/// The wrapped value is public because unwrapping it, usually by destructuring, is the only thing
/// callers ever do with one.
pub struct BorrowableCow<'a>(pub Cow<'a, str>);

impl<'de> Deserialize<'de> for BorrowableCow<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BorrowableCowVisitor;

        impl<'de> Visitor<'de> for BorrowableCowVisitor {
            type Value = BorrowableCow<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                v: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(BorrowableCow(Cow::Borrowed(v)))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(BorrowableCow(Cow::Owned(v.to_string())))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(BorrowableCow(Cow::Owned(v)))
            }
        }

        deserializer.deserialize_str(BorrowableCowVisitor)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    #[test]
    fn a_string_without_escapes_is_borrowed_from_the_input() {
        // Arrange.
        let input = "\"borrowed\"";

        // Act.
        let super::BorrowableCow(value) =
            serde_json::from_str::<super::BorrowableCow<'_>>(input).unwrap();

        // Assert.
        assert!(matches!(value, Cow::Borrowed("borrowed")));
    }

    #[test]
    fn a_string_with_an_escape_is_owned() {
        // Arrange: an escape forces the parser to build a new string, so it cannot lend a slice.
        let input = "\"an \\\"escape\\\"\"";

        // Act.
        let super::BorrowableCow(value) =
            serde_json::from_str::<super::BorrowableCow<'_>>(input).unwrap();

        // Assert.
        assert_eq!(value, Cow::<'_, str>::Owned("an \"escape\"".to_string()));
        assert!(matches!(value, Cow::Owned(_)));
    }
}
