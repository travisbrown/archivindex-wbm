use archivindex_wbm::{digest::Sha1Digest, timestamp::Timestamp};
use sha1::{Digest, Sha1};
use std::borrow::Cow;

pub mod io;

const DEFAULT_CLOSING_WHITESPACE: [u8; 3] = [b'\r', b'\r', b'\n'];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Invalid line")]
    InvalidLine,
    #[error("Invalid closing whitespace")]
    InvalidClosingWhitespace(String),
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Snapshot<'a, S> {
    pub digest: Sha1Digest,
    pub expected_digest: Option<Sha1Digest>,
    #[serde(
        with = "closing_whitespace",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub closing_whitespace: Option<Vec<char>>,
    pub timestamp: Option<Timestamp>,
    pub url: Option<Cow<'a, str>>,
    pub content: S,
}

impl<'a, S> Snapshot<'a, S> {
    pub fn map_content<T, F: FnOnce(S) -> T>(self, f: F) -> Snapshot<'a, T> {
        Snapshot {
            digest: self.digest,
            expected_digest: self.expected_digest,
            closing_whitespace: self.closing_whitespace,
            timestamp: self.timestamp,
            url: self.url,
            content: f(self.content),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotLine<'a> {
    pub digest: Sha1Digest,
    pub expected_digest: Option<Sha1Digest>,
    pub closing_whitespace: Option<Vec<char>>,
    pub timestamp: Option<Timestamp>,
    pub url: Option<Cow<'a, str>>,
    pub content: Cow<'a, str>,
}

impl std::fmt::Display for SnapshotLine<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{\"{}\":\"{}\",", DIGEST_KEY, self.digest)?;

        if let Some(expected_digest) = self.expected_digest {
            write!(f, "\"{EXPECTED_DIGEST_KEY}\":\"{expected_digest}\",")?;
        }

        if let Some(closing_whitespace) = &self.closing_whitespace {
            write!(f, "\"{CLOSING_WHITESPACE_KEY}\":\"")?;

            for whitespace in closing_whitespace {
                match whitespace {
                    '\n' => f.write_str("\\n")?,
                    '\r' => f.write_str("\\r")?,
                    _ => {}
                }
            }

            f.write_str("\",")?;
        }

        if let Some(timestamp) = self.timestamp {
            write!(f, "\"{TIMESTAMP_KEY}\":\"{timestamp}\",")?;
        }

        if let Some(url) = &self.url {
            write!(f, "\"{URL_KEY}\":\"{url}\",")?;
        }

        write!(f, "\"content\":{}}}", self.content)
    }
}

const DIGEST_LEN: usize = 32;
const TIMESTAMP_LEN: usize = 14;

const DIGEST_KEY: &str = "digest";
const DIGEST_KEY_LEN: usize = DIGEST_KEY.len();
const EXPECTED_DIGEST_KEY: &str = "expected_digest";
const EXPECTED_DIGEST_KEY_LEN: usize = EXPECTED_DIGEST_KEY.len();
const CLOSING_WHITESPACE_KEY: &str = "closing_whitespace";
const CLOSING_WHITESPACE_KEY_LEN: usize = CLOSING_WHITESPACE_KEY.len();
const TIMESTAMP_KEY: &str = "timestamp";
const TIMESTAMP_KEY_LEN: usize = TIMESTAMP_KEY.len();
const URL_KEY: &str = "url";
const URL_KEY_LEN: usize = URL_KEY.len();
const CONTENT_KEY: &str = "content";
const CONTENT_KEY_LEN: usize = CONTENT_KEY.len();

impl<'a> SnapshotLine<'a> {
    #[must_use]
    pub fn new(digest: Sha1Digest, content: &'a str) -> Self {
        let bytes = content.as_bytes();

        let closing_whitespace = if bytes[bytes.len() - 3..] == DEFAULT_CLOSING_WHITESPACE
            && bytes[bytes.len() - 4] != b'\r'
            && bytes[bytes.len() - 4] != b'\n'
        {
            None
        } else {
            let mut closing_whitespace = content
                .chars()
                .rev()
                .take_while(|ch| *ch == '\r' || *ch == '\n')
                .collect::<Vec<_>>();

            closing_whitespace.reverse();

            Some(closing_whitespace)
        };

        let content = &content[0..content.len()
            - closing_whitespace
                .as_ref()
                .map_or(DEFAULT_CLOSING_WHITESPACE.len(), std::vec::Vec::len)];

        Self {
            digest,
            expected_digest: None,
            closing_whitespace,
            timestamp: None,
            url: None,
            content: content.into(),
        }
    }

    #[must_use]
    pub fn into_owned(self) -> SnapshotLine<'static> {
        SnapshotLine {
            digest: self.digest,
            expected_digest: self.expected_digest,
            closing_whitespace: self.closing_whitespace,
            timestamp: self.timestamp,
            url: self.url.map(|url| url.into_owned().into()),
            content: self.content.into_owned().into(),
        }
    }

    pub fn validate(&self, hasher: &mut sha1::Sha1) -> Result<(), Sha1Digest> {
        hasher.update(self.content.as_bytes());

        match self.closing_whitespace.as_ref() {
            Some(closing_whitespace) => {
                // We simply ignore any unexpected whitespace characters here.
                let bytes = closing_whitespace
                    .iter()
                    .filter_map(|whitespace| match whitespace {
                        '\n' => Some(b'\n'),
                        '\r' => Some(b'\r'),
                        _ => None,
                    })
                    .collect::<Vec<_>>();

                hasher.update(&bytes);
            }
            None => {
                hasher.update(DEFAULT_CLOSING_WHITESPACE);
            }
        }

        let digest = Sha1Digest(hasher.finalize_reset().into());

        if digest == self.digest {
            Ok(())
        } else {
            Err(digest)
        }
    }

    pub fn parse(line: &'a str) -> Result<Self, Error> {
        let mut index = DIGEST_KEY_LEN + 5;

        let digest = line[index..index + DIGEST_LEN]
            .parse::<Sha1Digest>()
            .map_err(|_| Error::InvalidLine)?;

        index += DIGEST_LEN + 3;

        if line.len() >= index + 2 {
            let expected_digest = if line[index..].starts_with(EXPECTED_DIGEST_KEY) {
                index += EXPECTED_DIGEST_KEY_LEN + 3;

                let expected_digest = line[index..index + DIGEST_LEN]
                    .parse::<Sha1Digest>()
                    .map_err(|_| Error::InvalidLine)?;

                index += DIGEST_LEN + 3;

                Some(expected_digest)
            } else {
                None
            };

            let closing_whitespace = if line[index..].starts_with(CLOSING_WHITESPACE_KEY) {
                let mut closing_whitespace = vec![];

                index += CLOSING_WHITESPACE_KEY_LEN + 3;

                let mut next = &line[index..=index];
                let mut failed = false;
                let mut i = 0;

                while next != "\"" {
                    if i % 2 == 0 && next != "\\" {
                        failed = true;
                    }

                    if i % 2 == 1 {
                        match next {
                            "n" => closing_whitespace.push('\n'),
                            "r" => closing_whitespace.push('\r'),
                            _ => {
                                failed = true;
                            }
                        }
                    }

                    i += 1;
                    next = &line[(index + i)..=(index + i)];
                }

                if failed {
                    Err(Error::InvalidLine)
                } else {
                    index += i + 3;

                    Ok(Some(closing_whitespace))
                }
            } else {
                Ok(None)
            }?;

            let timestamp = if line[index..].starts_with(TIMESTAMP_KEY) {
                index += TIMESTAMP_KEY_LEN + 3;

                let timestamp = line[index..index + TIMESTAMP_LEN]
                    .parse::<Timestamp>()
                    .map_err(|_| Error::InvalidLine)?;

                index += TIMESTAMP_LEN + 3;

                Some(timestamp)
            } else {
                None
            };

            let url = if line[index..].starts_with(URL_KEY) {
                index += URL_KEY_LEN + 3;

                let mut failed = false;
                let mut i = 0;

                while &line[(index + i)..=(index + i)] != "\"" {
                    i += 1;

                    if index + i >= line.len() {
                        failed = true;
                    }
                }

                if failed {
                    Err(Error::InvalidLine)
                } else {
                    let url = line[index..index + i].into();
                    index += i + 3;

                    Ok(Some(url))
                }
            } else {
                Ok(None)
            }?;

            index += CONTENT_KEY_LEN + 2;

            Ok(Self {
                digest,
                expected_digest,
                closing_whitespace,
                timestamp,
                url,
                content: line[index..line.len() - 1].into(),
            })
        } else {
            Err(Error::InvalidLine)
        }
    }

    pub fn validate_lines<R: std::io::Read>(
        lines: std::io::Lines<std::io::BufReader<R>>,
    ) -> Result<SnapshotLineValidation, std::io::Error> {
        let mut validation = SnapshotLineValidation::default();
        let mut hasher = Sha1::default();
        let mut last_digest = Sha1Digest::MIN;

        for (i, line) in lines.enumerate() {
            let line = line?;
            match SnapshotLine::parse(&line) {
                Ok(snapshot_line) => match snapshot_line.validate(&mut hasher) {
                    Ok(()) => {
                        if snapshot_line.digest > last_digest {
                            validation.valid_count += 1;
                            last_digest = snapshot_line.digest;
                        } else {
                            validation.out_of_order.push(snapshot_line.digest);
                        }
                    }
                    Err(actual_digest) => {
                        validation
                            .unexpected_digests
                            .push((snapshot_line.digest, actual_digest));
                    }
                },
                Err(_) => {
                    validation.invalid_lines.push(i + 1);
                }
            }
        }

        Ok(validation)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SnapshotLineValidation {
    pub valid_count: usize,
    pub invalid_lines: Vec<usize>,
    pub unexpected_digests: Vec<(Sha1Digest, Sha1Digest)>,
    pub out_of_order: Vec<Sha1Digest>,
}

impl SnapshotLineValidation {
    #[must_use]
    pub const fn is_successful(&self) -> bool {
        self.invalid_lines.is_empty()
            && self.unexpected_digests.is_empty()
            && self.out_of_order.is_empty()
    }
}

mod closing_whitespace {
    use serde::{
        de::{Deserialize, Deserializer},
        ser::Serializer,
    };

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<char>>, D::Error> {
        let closing_whitespace_str: Option<String> = Deserialize::deserialize(deserializer)?;

        closing_whitespace_str
            .map(|closing_whitespace_str| {
                let closing_whitespace = closing_whitespace_str
                    .chars()
                    .filter(|whitespace| *whitespace == '\n' || *whitespace == '\r')
                    .collect::<Vec<_>>();

                if closing_whitespace.len() == closing_whitespace_str.len() {
                    Ok(closing_whitespace)
                } else {
                    Err(serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(&closing_whitespace_str),
                        &"string of escaped whitespace characters",
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

                for whitespace in value {
                    match whitespace {
                        '\n' => closing_whitespace_str.push_str("\\n"),
                        '\r' => closing_whitespace_str.push_str("\\r"),
                        other => {
                            return Err(serde::ser::Error::custom(format!(
                                "unexpected escaped whitespace character: {other}"
                            )));
                        }
                    }
                }

                serializer.serialize_some(&closing_whitespace_str)
            }
            None => serializer.serialize_none(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufRead;

    use super::*;

    #[test]
    fn parse_inferred_url() -> Result<(), Box<dyn std::error::Error>> {
        let line = include_str!("../../../examples/wxj/inferred-url-01.json").trim();

        let parsed = SnapshotLine::parse(line)?;

        assert_eq!(line, parsed.to_string());

        assert_eq!(parsed.validate(&mut Default::default()), Ok(()));

        Ok(())
    }

    #[test]
    fn parse_examples() -> Result<(), Box<dyn std::error::Error>> {
        let lines = include_str!("../../../examples/wxj/lines-01.ndjson").split("\n");

        for line in lines {
            let parsed = SnapshotLine::parse(line)?;

            assert_eq!(line, parsed.to_string());

            assert_eq!(parsed.validate(&mut Default::default()), Ok(()));
        }

        Ok(())
    }

    #[test]
    fn validate_all_examples() -> Result<(), Box<dyn std::error::Error>> {
        let lines = std::io::BufReader::new(std::io::Cursor::new(
            include_str!("../../../examples/wxj/lines-01.ndjson").as_bytes(),
        ))
        .lines();

        let validation = SnapshotLine::validate_lines(lines)?;

        assert!(validation.is_successful());

        Ok(())
    }

    #[test]
    fn deserialize_examples() -> Result<(), Box<dyn std::error::Error>> {
        let lines = include_str!("../../../examples/wxj/lines-01.ndjson").split("\n");

        for line in lines {
            let _snapshot = serde_json::from_str::<
                Snapshot<'_, birdsite::model::wxj::data::TweetSnapshot<'_>>,
            >(line)?;
        }

        Ok(())
    }

    #[test]
    fn snapshot_line_snapshot_match() -> Result<(), Box<dyn std::error::Error>> {
        let lines = include_str!("../../../examples/wxj/lines-01.ndjson").split("\n");

        for line in lines {
            let snapshot_line = SnapshotLine::parse(line)?;
            let snapshot = serde_json::from_str::<
                Snapshot<'_, birdsite::model::wxj::data::TweetSnapshot<'_>>,
            >(line)?;

            assert_eq!(snapshot_line.digest, snapshot.digest);
            assert_eq!(snapshot_line.expected_digest, snapshot.expected_digest);
            assert_eq!(
                snapshot_line.closing_whitespace,
                snapshot.closing_whitespace
            );
            assert_eq!(snapshot_line.timestamp, snapshot.timestamp);
            assert_eq!(snapshot_line.url, snapshot.url);
        }

        Ok(())
    }
}
