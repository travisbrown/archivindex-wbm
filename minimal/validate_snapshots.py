"""Validate JSONL snapshot files (optionally compressed).

Each line is a JSON wrapper whose ``digest`` field must match the Base32-encoding of the SHA-1 hash
of the content (including the closing whitespace, which by default is "\r\r\n").

Both snapshot layouts are supported:

* old: ``closing_whitespace`` is a top-level string field;
* new: ``closing_whitespace`` lives inside the ``format`` object.

Only the implicit UTF-8 format is checked. A declared ``type`` is reproduced through a codec this
minimal tool does not implement, so any snapshot whose ``format`` contains a ``type`` key is
reported and skipped, including an explicit ``"utf8"`` type. Input must use the compact field order
described by ``extract_raw_content``.

Usage::

    python validate_snapshots.py <file> [--closing-whitespace '\\r\\r\\n']
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import sys
from typing import Optional

DIGEST_LEN = 32
DIGEST_PREFIX_LEN = len('{"digest":"')

# JSON whitespace characters and their byte values.
_WHITESPACE_BYTES = {"\r": b"\r", "\n": b"\n", "\t": b"\t", " ": b" "}


def unescape_whitespace(escaped: str) -> str:
    """Convert a CLI-provided escaped string to actual whitespace characters.

    Accepts ``\\r``, ``\\n``, and ``\\t`` as two-character escape sequences and passes through
    literal space characters.

    >>> unescape_whitespace(r"\\r\\r\\n")
    '\\r\\r\\n'
    """
    result: list[str] = []
    i = 0
    while i < len(escaped):
        if escaped[i] == "\\" and i + 1 < len(escaped):
            nxt = escaped[i + 1]
            if nxt == "r":
                result.append("\r")
            elif nxt == "n":
                result.append("\n")
            elif nxt == "t":
                result.append("\t")
            else:
                result.append(escaped[i])
                result.append(nxt)
            i += 2
        else:
            result.append(escaped[i])
            i += 1
    return "".join(result)


def closing_whitespace_to_bytes(whitespace: str) -> bytes:
    """Map each whitespace character to its raw byte, ignoring unknowns."""
    return b"".join(_WHITESPACE_BYTES.get(ch, b"") for ch in whitespace)


def skip_json_object(line: str, idx: int) -> int:
    """Return the index just past the ``}`` matching the ``{`` at ``idx``.

    Tracks brace depth while respecting JSON strings and their backslash escapes, mirroring the Rust
    ``read_object_value`` helper.
    """
    depth = 0
    in_string = False
    escaped = False
    while idx < len(line):
        ch = line[idx]
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
        elif ch == '"':
            in_string = True
        elif ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return idx + 1
        idx += 1
    raise ValueError("unterminated format object")


def extract_raw_content(line: str) -> str:
    """Extract the raw content JSON substring using positional parsing.

    Handles both snapshot layouts (every field but ``digest`` and ``content`` is optional):

    * old: ``digest``, ``expected_digest``, ``closing_whitespace`` (a top-level string),
      ``timestamp``, ``url``, ``content``;
    * new: ``digest``, ``expected_digest``, ``timestamp``, ``url``, ``format`` (an object that
      carries ``closing_whitespace``), ``content``.
    """
    idx = DIGEST_PREFIX_LEN + DIGEST_LEN + 3

    if line[idx:].startswith("expected_digest"):
        idx += len('expected_digest":"') + DIGEST_LEN + 3

    # Old layout: a top-level closing_whitespace string.
    if line[idx:].startswith("closing_whitespace"):
        idx += len('closing_whitespace":"')
        # Scan for the closing quote, respecting JSON backslash escapes.
        while line[idx] != '"':
            if line[idx] == "\\":
                idx += 2
            else:
                idx += 1
        idx += 3

    if line[idx:].startswith("timestamp"):
        idx += len('timestamp":"') + 14 + 3

    if line[idx:].startswith("url"):
        idx += len('url":"')
        while line[idx] != '"':
            idx += 1
        idx += 3

    # New layout: a format object (holding closing_whitespace, an optional type, and metadata).
    if line[idx:].startswith("format"):
        idx += len('format":')
        idx = skip_json_object(line, idx)
        idx += 2  # skip the ',' separator and the opening '"' of the content key

    idx += len('content":')
    return line[idx:-1]


def compute_digest(content: str, closing_whitespace_bytes: bytes) -> str:
    hasher = hashlib.sha1(content.encode("utf-8"))
    hasher.update(closing_whitespace_bytes)
    return base64.b32encode(hasher.digest()).decode("ascii")


def open_input(path: str):
    """Open a file, using zstd decompression if the path ends with ``.zst``."""
    if path.endswith(".zst"):
        import zstandard

        file_handle = open(path, "rb")
        decompressor = zstandard.ZstdDecompressor()
        reader = decompressor.stream_reader(file_handle)
        import io

        return io.TextIOWrapper(reader, encoding="utf-8")
    return open(path, encoding="utf-8")


def validate_file(
    path: str, default_closing_whitespace: str
) -> tuple[int, int, int, int, list[str]]:
    """Validate all snapshot lines in a file."""
    total = 0
    valid = 0
    invalid = 0
    skipped = 0
    last_digest_bytes: Optional[bytes] = None
    out_of_order: list[str] = []
    default_whitespace_bytes = closing_whitespace_to_bytes(default_closing_whitespace)

    with open_input(path) as fh:
        for line_no, raw_line in enumerate(fh, start=1):
            line = raw_line.rstrip("\n").rstrip("\r")
            if not line:
                continue

            total += 1

            try:
                parsed = json.loads(line)
            except json.JSONDecodeError as exc:
                print(
                    f"line {line_no}: JSON parse error: {exc}",
                    file=sys.stderr,
                )
                invalid += 1
                continue

            digest = parsed.get("digest")
            if digest is None:
                print(
                    f"line {line_no}: missing digest field",
                    file=sys.stderr,
                )
                invalid += 1
                continue

            # Check sort order by digest bytes, not the Base32 string, since Base32 encoding does
            # not preserve byte ordering.
            digest_bytes = base64.b32decode(digest)
            if last_digest_bytes is not None and digest_bytes <= last_digest_bytes:
                out_of_order.append(digest)
            last_digest_bytes = digest_bytes

            # The new layout nests the format details (including closing_whitespace) in a `format`
            # object; the old layout put closing_whitespace at the top level.
            fmt = parsed.get("format")

            # A declared format type is reproduced through a codec this minimal tool does not
            # implement, so its digest cannot be checked here; every explicit type is skipped,
            # including "utf8".
            if isinstance(fmt, dict) and "type" in fmt:
                print(
                    f"line {line_no}: {digest}: unsupported format {fmt['type']!r}, skipped",
                    file=sys.stderr,
                )
                skipped += 1
                continue

            # Determine effective closing whitespace (top-level for old, format for new).
            closing_whitespace = parsed.get("closing_whitespace")
            if closing_whitespace is None and isinstance(fmt, dict):
                closing_whitespace = fmt.get("closing_whitespace")

            if closing_whitespace is not None:
                whitespace_bytes = closing_whitespace_to_bytes(closing_whitespace)
            else:
                whitespace_bytes = default_whitespace_bytes

            try:
                content = extract_raw_content(line)
            except (IndexError, ValueError) as exc:
                print(
                    f"line {line_no}: content extraction failed: {exc}",
                    file=sys.stderr,
                )
                invalid += 1
                continue

            computed = compute_digest(content, whitespace_bytes)

            if computed == digest:
                valid += 1
            else:
                print(
                    f"line {line_no}: {digest}: expected {digest}, computed {computed}",
                    file=sys.stderr,
                )
                invalid += 1

    return total, valid, invalid, skipped, out_of_order


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Validate JSONL snapshot files (optionally Zstandard-compressed).",
    )
    parser.add_argument(
        "file",
        help="Path to the .jsonl or .jsonl.zst file to validate.",
    )
    parser.add_argument(
        "--closing-whitespace",
        default=r"\r\r\n",
        help=(
            "Default closing whitespace as an escaped string "
            r"(e.g. '\r\r\n'). Used when a line has no closing_whitespace field. "
            r"Default: '\r\r\n'."
        ),
    )
    args = parser.parse_args()

    default_whitespace = unescape_whitespace(args.closing_whitespace)

    total, valid, invalid, skipped, out_of_order = validate_file(
        args.file, default_whitespace
    )

    summary = f"{total} lines, {valid} valid, {invalid} invalid"
    if skipped:
        summary += f", {skipped} skipped (unsupported format)"
    print(summary, file=sys.stderr)
    if out_of_order:
        print(f"{len(out_of_order)} out-of-order digests", file=sys.stderr)

    sys.exit(1 if invalid > 0 or out_of_order else 0)


if __name__ == "__main__":
    main()
