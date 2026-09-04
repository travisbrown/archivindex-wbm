# archivindex-wbm

![GitHub last commit][last-commit-badge]
[![build][build-badge]][build]
[![codecov][codecov-badge]][codecov]
[![license][license-badge]][agpl-3.0]
[![crates.io][crates-version-badge]][crates]
[![crates.io][crates-downloads-badge]][crates]
[![API Docs][docs-badge]][docs]

This repository contains a set of Rust libraries for working with data from web archives, including
primarily the [Internet Archive][internet-archive]'s [Wayback Machine][wayback-machine]. An earlier
iteration of this project was developed with [support][archivindex-prototype-fund] from [the
Prototype Fund][prototype-fund].

## The JSON format

Apart from some Rust representations of core Wayback Machine types, the most interesting thing in
this repository is probably a JSON format that can be used to make large collections of API
snapshots from the Wayback Machine easier to work with.

The Wayback Machine contains many snapshots that are well-formed JSON with no internal line breaks,
including hundreds of millions of tweets and posts from Truth Social, among many other kinds of
things. It's often practical to store these snapshots as newline-delimited JSON (JSONL) files. The
`archivindex-wbm-json` crate provides a representation for these snapshots that includes both the
archived content and some metadata.

There are several motivations for this serialization format:

1. Efficiency. Reading tens of millions of small files can take a long time.
2. Convenience. It keeps things simple to store metadata alongside snapshot content.
3. Verification. We need to preserve the original bytes so that we can confirm the [CDX][cdx]
   digests.
4. Tools. I've often used formats like Parquet to meet the requirements above in the general case,
   but it's nicer to be able to use standard tools for working with JSON and JSONL files.

The basic idea is that each line of the JSONL file represents a single snapshot from the Wayback
Machine, in the following format (with newlines added here for clarity):

```json
{
  "digest": "J52QGGGXF27X4B67T54WNBT3O77SSVN5",
  "timestamp": "20250506174711",
  "url": "https://twitter.com/elonmusk/status/1724908287471272299",
  "content": "..."
}
```

The value of the `content` field will be the JSON object saved in the Wayback Machine, with some
specific properties that make it possible for us to recover the exact bytes served. Being able to
recover the exact bytes makes it possible to verify the `digest` field, which will generally be the
Base32 encoding of the SHA-1 digest provided by the Wayback Machine's CDX service for the snapshot.

Specifically, the bytes following `"content":` on each line will exactly match the bytes served by
the Wayback Machine, except that any trailing line-break characters will have been trimmed (which is
necessary for the JSONL format). Each JSONL file has an implicit default trailing whitespace
sequence, since this value is generally fairly consistent across snapshots for a platform. For
example, `twitter.com` snapshots typically use `\r\r\n`, with a tiny number of exceptions that have
`\r\n`, while `truthsocial.com` consistently uses simply `\n`.

In cases where the snapshot has a trailing whitespace sequence that does not match the default for
the platform, the line will have an additional top-level `format` field.

```json
    "format": {
        "closing_whitespace": "\r\n"
    }
```

In some more complicated cases, the `format` object may contain additional information that is
needed to recover the original bytes. For example, the Wayback Machine snapshots for Truth Social
API posts are often served as gzip-compressed bytes, even though the MIME type is given as
`application/json`. We want to store the uncompressed JSON here, so that we can use standard JSON
tools, etc., but we also want to be able to recover the original bytes in order to verify them
against the CDX digests. Luckily it's possible to implement byte-exact matching gzip operations for
every such case I've come across in the Wayback Machine, and these are provided in the
`archivindex-wbm-json-gzip` crate. We have observed gzip compression performed by various tools and
with various parameters, and the information that is necessary to characterize these tools and
parameters is captured in the `format` object.

There are two other details of the format that are relevant in a small percentage of real-world
cases. In some rare cases, the SHA-1 digest in the Wayback Machine's CDX results does not match the
bytes that the Wayback Machine serves. I've spent some time investigating these mismatches, and have
[published an explanation for some of them][wbm-invalid-digests], together with examples. In these
cases, the `digest` field will be the actual computed digest, and the line will have an additional
`expected_digest` field that indicates the digest in the CDX results.

The final detail involves the `url` field. The timestamp and source URL uniquely identify a
snapshot, but in many cases we can omit the `url` value, because it can be recovered easily from the
content. For posts from the Truth Social API, for example, the URL will be
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

There are two other directories that are not part of the published API:

- `tools/hacks/`: An unpublished workspace member used for scratch and one-off processing
  tasks.
- `minimal/`: A standalone Python verifier for the snapshot format. The `validate_snapshots.py`
  script checks that the `digest` field of each line of a (possibly compressed) JSONL snapshot file
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
