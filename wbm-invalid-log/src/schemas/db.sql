CREATE TABLE IF NOT EXISTS invalid_digest(
    id INTEGER PRIMARY KEY NOT NULL,
    timestamp INTEGER NOT NULL,
    url TEXT NOT NULL,
    archive_timestamp INTEGER NOT NULL,
    expected_digest TEXT NOT NULL,
    actual_digest BLOB NOT NULL CHECK(LENGTH(actual_digest) = 20),
    UNIQUE(url, timestamp, expected_digest, actual_digest)
);

CREATE INDEX IF NOT EXISTS invalid_digest_url_timestamp ON invalid_digest(url, timestamp);

CREATE TABLE IF NOT EXISTS withheld_url(
    id INTEGER PRIMARY KEY NOT NULL,
    timestamp INTEGER NOT NULL,
    url TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS withheld_url_url ON withheld_url(url);