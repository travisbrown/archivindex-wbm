//! The `invalid-log` subcommand implementations.
//!
//! The database iteration and insertion live in [`archivindex_wbm_invalid_log::Database`]; this
//! module owns only the CSV serialization and the on-disk file layout (the two CSV file names).

use std::path::Path;

use anyhow::Context as _;
use archivindex_wbm::digest::{Digest, Sha1Digest};
use archivindex_wbm::item::{ItemInfo, UrlParts};
use archivindex_wbm::timestamp::Timestamp;
use archivindex_wbm_invalid_log::{Database, Entry, Export, InvalidDigestRecord, WithheldRecord};
use bounded_static::IntoBoundedStatic;
use chrono::{DateTime, Utc};

/// CSV file (within an export directory) holding the `invalid_digest` rows.
const INVALID_DIGESTS_CSV: &str = "invalid-digests.csv";
/// CSV file (within an export directory) holding the `withheld_url` rows.
const WITHHELD_URLS_CSV: &str = "withheld-urls.csv";

/// Export an invalid-digest database to two CSV files in `output` (created if absent).
pub fn export(db: &Path, output: &Path) -> Result<(), anyhow::Error> {
    let database = open(db)?;
    std::fs::create_dir_all(output)
        .with_context(|| format!("failed to create {}", output.display()))?;
    let export = database.export()?;

    let mut invalid_writer = writer(output, INVALID_DIGESTS_CSV)?;
    for record in &export.invalid_digests {
        invalid_writer.serialize(InvalidDigestRow::from(record))?;
    }
    invalid_writer.flush()?;

    let mut withheld_writer = writer(output, WITHHELD_URLS_CSV)?;
    for record in &export.withheld_urls {
        withheld_writer.serialize(WithheldRow::from(record))?;
    }
    withheld_writer.flush()?;

    Ok(())
}

/// Build an invalid-digest database at `db` from the two CSV files in `input`.
pub fn import(input: &Path, db: &Path) -> Result<(), anyhow::Error> {
    let database = open(db)?;
    let mut export = Export::default();

    let mut invalid_reader = reader(input, INVALID_DIGESTS_CSV)?;
    for row in invalid_reader.deserialize() {
        let row: InvalidDigestRow = row?;
        export.invalid_digests.push(row.into_record()?);
    }

    let mut withheld_reader = reader(input, WITHHELD_URLS_CSV)?;
    for row in withheld_reader.deserialize() {
        let row: WithheldRow = row?;
        export.withheld_urls.push(row.into_record()?);
    }

    database.import(&export)?;

    Ok(())
}

/// Print `url,archive_timestamp,expected_digest,actual_digest` CSV (no header) for every invalid
/// digest in `db` to standard output.
pub fn export_invalid_digests(db: &Path) -> Result<(), anyhow::Error> {
    let database = open(db)?;

    // `write_record` never emits a header row.
    let mut writer = csv::Writer::from_writer(std::io::stdout());
    for (_observed, entry) in database.invalid_digests(None)? {
        writer.write_record([
            entry.item_info.url_parts.url.into_owned(),
            entry.item_info.url_parts.timestamp.to_string(),
            entry.item_info.expected_digest.to_string(),
            entry.actual_digest.to_string(),
        ])?;
    }
    writer.flush()?;

    Ok(())
}

/// Open the invalid-digest database at `path`, creating it if absent and including its path in
/// errors.
///
/// # Errors
///
/// Returns an error if the file cannot be created, opened, or read as an invalid-digest database.
pub fn open(path: &Path) -> Result<Database, anyhow::Error> {
    Database::open(path).with_context(|| {
        format!(
            "failed to open the invalid-digest database at {}",
            path.display()
        )
    })
}

/// Open a CSV writer for `name` within the `directory` of an export, naming the file in any
/// failure.
fn writer(directory: &Path, name: &str) -> Result<csv::Writer<std::fs::File>, anyhow::Error> {
    let path = directory.join(name);

    csv::Writer::from_path(&path).with_context(|| format!("failed to write {}", path.display()))
}

/// Open a CSV reader for `name` within the `directory` of an export, naming the file in any
/// failure.
fn reader(directory: &Path, name: &str) -> Result<csv::Reader<std::fs::File>, anyhow::Error> {
    let path = directory.join(name);

    csv::Reader::from_path(&path).with_context(|| format!("failed to read {}", path.display()))
}

/// Convert a Unix-second observation timestamp into a [`DateTime`], the form the database expects.
fn observation_time(seconds: i64) -> Result<DateTime<Utc>, anyhow::Error> {
    DateTime::from_timestamp(seconds, 0)
        .with_context(|| format!("invalid observation timestamp: {seconds}"))
}

/// The CSV form of an [`InvalidDigestRecord`], with an observation time in Unix seconds, a
/// fourteen-digit capture timestamp, and digest text (Base32 for valid SHA-1 values). Field order
/// matches the CSV columns.
#[derive(serde::Serialize, serde::Deserialize)]
struct InvalidDigestRow {
    /// When the invalidity was observed, as Unix seconds.
    observed: i64,
    /// The archived URL.
    url: String,
    /// The Wayback Machine capture timestamp (14 digits).
    archive_timestamp: String,
    /// The expected (CDX) digest.
    expected_digest: String,
    /// The actual digest computed from the downloaded content.
    actual_digest: String,
}

impl From<&InvalidDigestRecord<'_>> for InvalidDigestRow {
    fn from(record: &InvalidDigestRecord<'_>) -> Self {
        Self {
            observed: record.observed.timestamp(),
            url: record.entry.item_info.url_parts.url.to_string(),
            archive_timestamp: record.entry.item_info.url_parts.timestamp.to_string(),
            expected_digest: record.entry.item_info.expected_digest.to_string(),
            actual_digest: record.entry.actual_digest.to_string(),
        }
    }
}

impl InvalidDigestRow {
    fn into_record(self) -> Result<InvalidDigestRecord<'static>, anyhow::Error> {
        let entry = Entry::new(
            ItemInfo::new(
                UrlParts::new(self.url, self.archive_timestamp.parse::<Timestamp>()?),
                Digest::parse_str(&self.expected_digest).into_static(),
            ),
            self.actual_digest.parse::<Sha1Digest>()?,
        );

        Ok(InvalidDigestRecord {
            observed: observation_time(self.observed)?,
            entry,
        })
    }
}

/// The CSV form of a [`WithheldRecord`].
#[derive(serde::Serialize, serde::Deserialize)]
struct WithheldRow {
    /// When the withheld status was observed, as Unix seconds.
    observed: i64,
    /// The withheld URL.
    url: String,
}

impl From<&WithheldRecord> for WithheldRow {
    fn from(record: &WithheldRecord) -> Self {
        Self {
            observed: record.observed.timestamp(),
            url: record.url.clone(),
        }
    }
}

impl WithheldRow {
    fn into_record(self) -> Result<WithheldRecord, anyhow::Error> {
        Ok(WithheldRecord {
            observed: observation_time(self.observed)?,
            url: self.url,
        })
    }
}
