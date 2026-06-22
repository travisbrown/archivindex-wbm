//! Site-specific [`Context`](crate::context::Context) constructors and content types.
//!
//! Each format under [`instances`] provides a `context()` function — the canonical default closing
//! whitespace plus the URL inference used to omit a redundant `url` field when serializing — along
//! with the deserialized `Content` type and a `Snapshot` type alias for that format. Where the URL
//! can be derived from the content, an `infer_url` function is also exposed for callers that
//! already hold the typed content.

pub mod instances;
