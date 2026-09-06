# archivindex-wbm

![GitHub last commit][last-commit-badge]
[![build][build-badge]][build]
[![codecov][codecov-badge]][codecov]
[![license][license-badge]][agpl-3.0]
[![crates.io][crates-version-badge]][crates]
[![crates.io][crates-downloads-badge]][crates]
[![API Docs][docs-badge]][docs]

This repository contains Rust libraries for working with web archive data, primarily from the
[Internet Archive][internet-archive]'s [Wayback Machine][wayback-machine]. An earlier
iteration of this project was developed with [support][archivindex-prototype-fund] from [the
Prototype Fund][prototype-fund].

## The JSON format

The snapshot JSON format stores large collections of archived API responses together with the
metadata needed to identify and verify them.

API snapshots such as tweets and Truth Social posts often contain JSON with no internal line
breaks. The `archivindex-wbm-json` crate stores these snapshots as newline-delimited JSON (JSONL),
with one snapshot per line.

This format reduces the overhead of reading many small files, keeps metadata alongside content,
and preserves the bytes needed to verify [CDX][cdx] digests. The files can be read with standard
JSON and JSONL tools.

The basic idea is that each line of the JSONL file represents a single snapshot from the Wayback
Machine, in the following format (formatted here for readability):

```json
{
  "digest": "J52QGGGXF27X4B67T54WNBT3O77SSVN5",
  "timestamp": "20250506174711",
  "url": "https://twitter.com/elonmusk/status/1724908287471272299",
  "content": "..."
}
```

The `content` field holds the archived JSON value, shown as `"..."` in this example. For an
uncompressed UTF-8 snapshot, the content value preserves the original bytes except for trailing
JSON whitespace (spaces, tabs, carriage returns, and line feeds), which is stored separately.
Compressed snapshots also need a codec to reproduce the original bytes, as described below.

The `digest` field is the uppercase Base32-encoded SHA-1 of the original bytes. Files are sorted
by the decoded digest bytes, with no duplicate digests; sorting the Base32 strings gives a
different order.

Each JSONL file is interpreted under a context that supplies the default trailing whitespace.
The bundled Twitter context uses `\r\r\n`; the Truth Social context uses `\n`.

When trailing whitespace differs from the context default, the line includes a top-level `format`
field whose value is, for example:

```json
{"closing_whitespace":"\r\n"}
```

In some more complicated cases, the `format` object may contain additional information that is
needed to recover the original bytes. For example, the Wayback Machine snapshots for Truth Social
API posts are often served as gzip-compressed bytes, even though the MIME type is given as
`application/json`. The snapshot stores the decompressed JSON so standard JSON tools can read it.
The `archivindex-wbm-json-gzip` crate reproduces supported gzip encodings byte for byte, using the
compression implementation, level, headers, and flush parameters recorded in the `format` object.
Parameter detection succeeds only when recompression exactly matches the original bytes.

There are two other details of the format that are relevant in a small percentage of real-world
cases. In some rare cases, the SHA-1 digest in the Wayback Machine's CDX results does not match the
bytes that the Wayback Machine serves. I've spent some time investigating these mismatches, and have
[published an explanation for some of them][wbm-invalid-digests], together with examples. In these
cases, the `digest` field will be the actual computed digest, and the line will have an additional
`expected_digest` field that indicates the digest in the CDX results.

The `url` field can be omitted when the context can recover the exact source URL from the content.
For posts from the Truth Social API, for example, the URL will be
`"https://truthsocial.com/api/v1/statuses/{id}"`, where `id` is the value of the `id` field inside
`content`. For a JSONL file in this format capturing Truth Social posts, we may therefore choose not
to include the `url` field.

## Contents

The library crates, in `crates/`:

- `archivindex-wbm-cdx-client`: Queries the Internet Archive CDX server and records the HTTP
  requests and responses in WARC files.
- `archivindex-wbm`: Core Wayback Machine data types for parsing and modeling archived web captures,
  including digests, timestamps, SURT (Sort-friendly URI Reordering Transform) keys, redirect pages,
  and CDX index records.
- `archivindex-wbm-json`: Representation and byte-exact parsing of archived JSON snapshots, stored
  as JSONL, together with the context that is needed to verify their digests.
- `archivindex-wbm-json-gzip`: Byte-exact gzip reproduction for `archivindex-wbm-json` snapshots, so
  a compressed capture can be re-encoded to the bytes its digest was computed from.
- `archivindex-wbm-json-processing`: I/O and batch processing for JSON snapshot files.
- `archivindex-wbm-invalid-log`: SQLite log of issues found in CDX data (digest mismatches and URLs
  withheld by the Internet Archive).
- `archivindex-wbm-cas`: Content-addressed storage for snapshot bytes, indexed by SHA-1 digest, with
  a `Store` trait covering saving, lookup, digest verification, and copying across backends.
- `archivindex-wbm-cdx-index`: On-disk index of CDX items and capture metadata, backed by redb.
- `archivindex-wbm-downloader`: HTTP downloader for Wayback Machine snapshots.

There are also several command-line tools in `tools/` (not published):

- `archivindex-wbm-cdx-client-cli`: Reads CDX request parameters as CSV and archives the query
  responses in a WARC file.
- `archivindex-wbm-json-cli`: Verifies digests, exports, compacts, and merges snapshot JSONL,
  resolving digests to CDX metadata and reproducing original content bytes as needed.
- `archivindex-wbm-cdx-index-cli`: Fills the CDX index from JSON files, reports statistics, lists
  items whose digest is absent from given snapshot or digest files, and builds the digest-keyed
  capture metadata database.
- `archivindex-wbm-downloader-cli`: Downloads the captures listed as CSV on standard input, verifies
  a content-addressed store, and manages the invalid-digest log.

The repository also includes:

- `crates/test-data/`: A download cache for archive snapshots used in tests.
- `tools/hacks/`: An unpublished workspace member used for scratch and one-off processing
  tasks.
- `minimal/`: A standalone Python verifier for the snapshot format. The `validate_snapshots.py`
  script checks that the `digest` field of each line of a plain or Zstandard-compressed JSONL file
  matches the Base32-encoded SHA-1 hash of the content (including the closing whitespace), for both
  the old and new snapshot layouts. It only handles the default UTF-8 format; snapshots whose
  `format` contains a `type` key (including an explicit `"utf8"`) are reported and skipped.

## Building and installation

The workspace requires Rust 1.91 or later:

```bash
cargo build --release
```

The command-line tools are not published to [crates.io][crates], but they can be installed from a
checkout of this repository:

```bash
cargo install --path tools/cdx-client-cli  # Installs the `archivindex-wbm-cdx-client` binary
cargo install --path tools/json-cli        # Installs the `archivindex-wbm-json` binary
cargo install --path tools/cdx-index-cli   # Installs the `archivindex-wbm-cdx-index` binary
cargo install --path tools/downloader-cli  # Installs the `archivindex-wbm-downloader` binary
```

## Usage

The CDX client's `archive` command reads CSV rows from standard input in
`URL,matchType,fastLatest,limit` order, without a header. `matchType` accepts `exact`, `prefix`,
`host`, or `domain`; a negative `limit` requests the last N results. The `limit` field is optional
and may be omitted or left empty. Pass `--resume` to follow CDX resumption keys until every query
is exhausted. Transient failures get up to ten attempts, including the initial request, by default;
use `--retry-attempts` to change that limit. `--request-delay` accepts a duration such as `250ms`,
`2s`, or `1m` and waits that long between session requests:

```bash
printf '%s\n' \
  'example.org,exact,true,-5' \
  'example.com/docs/,prefix,false,' |
  archivindex-wbm-cdx-client archive --resume --request-delay 1s --output queries.warc
```

The `extract` command reads one or more plain or gzip-compressed WARC files produced by the client
and prints headerless CSV rows containing original URL, timestamp, digest, MIME type, and status:

```bash
archivindex-wbm-cdx-client extract --input queries.warc > captures.csv
```

As an example, the `archivindex-wbm-json` tool can verify the digest and ordering of every snapshot
in a set of snapshot JSONL Zstandard files:

```bash
archivindex-wbm-json verify --input snapshots/flat.jsonl.zst --input snapshots/data.jsonl.zst
```

It can also write the original content bytes for selected digests into a directory, with each file
named by its uppercase Base32 digest:

```bash
archivindex-wbm-json export --input snapshots/flat.jsonl.zst \
    --digest J52QGGGXF27X4B67T54WNBT3O77SSVN5 --output exported/
```

Each tool provides a `--help` listing of its subcommands and options.

## License

This project is licensed under the [GNU Affero General Public License, version 3
only](https://www.gnu.org/licenses/agpl-3.0.html). See [LICENSE](LICENSE) for the full text.

[agpl-3.0]: https://www.gnu.org/licenses/agpl-3.0.html
[archivindex-prototype-fund]: https://www.prototypefund.de/en/projects/archivindex-builder
[build]: https://github.com/travisbrown/archivindex-wbm/actions/workflows/ci.yml
[build-badge]: https://github.com/travisbrown/archivindex-wbm/actions/workflows/ci.yml/badge.svg
[cdx]: https://github.com/internetarchive/wayback/tree/master/wayback-cdx-server
[codecov]: https://codecov.io/gh/travisbrown/archivindex-wbm
[codecov-badge]: https://codecov.io/gh/travisbrown/archivindex-wbm/branch/main/graph/badge.svg
[crates]: https://crates.io/crates/archivindex-wbm/
[crates-downloads-badge]: https://img.shields.io/crates/d/archivindex-wbm
[crates-version-badge]: https://img.shields.io/crates/v/archivindex-wbm.svg
[docs]: https://docs.rs/archivindex-wbm/
[docs-badge]: https://docs.rs/archivindex-wbm/badge.svg
[internet-archive]: https://archive.org
[last-commit-badge]: https://img.shields.io/github/last-commit/travisbrown/archivindex-wbm
[license-badge]: https://img.shields.io/badge/license-AGPL--v3-blue
[prototype-fund]: https://www.prototypefund.de/en/
[wayback-machine]: https://web.archive.org
[wbm-invalid-digests]: https://github.com/travisbrown/wbm-invalid-digests
