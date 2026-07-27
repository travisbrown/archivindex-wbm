CREATE TABLE IF NOT EXISTS invalid_digest(
    id INTEGER PRIMARY KEY NOT NULL,
    timestamp INTEGER NOT NULL,
    url TEXT NOT NULL,
    archive_timestamp INTEGER NOT NULL,
    expected_digest TEXT NOT NULL,
    actual_digest BLOB NOT NULL CHECK(LENGTH(actual_digest) = 20),
    -- A digest-mismatch record is identified by its archived URL, capture time, and digest pair;
    -- the `timestamp` column records when the invalidity was *observed* and is not part of the
    -- identity.
    UNIQUE(url, archive_timestamp, expected_digest, actual_digest)
);

-- Serves the observation-time range queries (`WHERE timestamp >= ? ORDER BY timestamp`). The
-- deduplication lookup on the identity columns is served by the implicit `UNIQUE` index.
CREATE INDEX IF NOT EXISTS invalid_digest_timestamp ON invalid_digest(timestamp);

CREATE TABLE IF NOT EXISTS withheld_url(
    id INTEGER PRIMARY KEY NOT NULL,
    timestamp INTEGER NOT NULL,
    url TEXT NOT NULL,
    -- A withheld status may be observed repeatedly.
    UNIQUE(url, timestamp)
);

-- Serves the observation-time range queries. URL-prefix lookups use the implicit `UNIQUE` index.
CREATE INDEX IF NOT EXISTS withheld_url_timestamp ON withheld_url(timestamp);
