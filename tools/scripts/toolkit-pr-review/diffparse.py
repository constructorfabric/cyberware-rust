#!/usr/bin/env python3
"""Unified-diff parsing for toolkit-pr-review.

Turns `git diff` / `gh pr diff` output into per-file records carrying the exact line
numbers a review comment may anchor on, and slices the diff so a shard agent is handed
only the hunks for its own files.

Two things here are deliberately stricter than the prose this replaces:

1. **Ranges cover changed lines only, not whole hunks.** The prose computed a file's
   ranges from the hunk header (`newStart` .. `newStart + newCount - 1`), which spans
   the context lines around the change as well. A finding anchored on an unchanged
   context line passed validation and was posted against code the PR never touched.
   Here the hunk body is walked line by line, so a range contains added lines and
   nothing else.

2. **Both sides are recorded.** `right` holds added-line numbers in the head file,
   `left` holds removed-line numbers in the base file. GitHub accepts a review comment
   on either side, so a finding about deleted code can point at the deleted line rather
   than at whatever line happened to survive next to it.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field

HUNK_RE = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
# Git quotes a path in the header as soon as it holds a byte outside plain ASCII, a
# quote or a backslash, writing `diff --git "a/f\303\274.rs" "b/f\303\274.rs"` with
# C-style escapes. The unquoted alternative anchored on a literal `a/`, so a quoted
# header matched nothing, the file never became a FileDiff and it dropped out of the
# review with no warning. Each side is quoted independently, so both forms can appear
# on one line.
GIT_HEADER_RE = re.compile(
    r'^diff --git (?:"a/((?:[^"\\]|\\.)*)"|a/(.+?)) (?:"b/((?:[^"\\]|\\.)*)"|b/(.+))$'
)

_ESCAPES = {
    "a": 0x07, "b": 0x08, "t": 0x09, "n": 0x0A,
    "v": 0x0B, "f": 0x0C, "r": 0x0D, '"': 0x22, "\\": 0x5C,
}


def unquote_path(text: str) -> str:
    """Decode the C-style escaping git uses for a quoted path in a diff header.

    Octal escapes carry raw bytes, so they are collected as bytes and decoded as UTF-8
    at the end; decoding each one alone would mangle every multi-byte character.
    """
    out = bytearray()
    i = 0
    while i < len(text):
        ch = text[i]
        if ch != "\\":
            out.extend(ch.encode("utf-8"))
            i += 1
            continue
        nxt = text[i + 1] if i + 1 < len(text) else ""
        if nxt in _ESCAPES:
            out.append(_ESCAPES[nxt])
            i += 2
        elif nxt.isdigit():
            octal = text[i + 1:i + 4]
            out.append(int(octal, 8) & 0xFF)
            i += 1 + len(octal)
        else:
            # Not an escape git produces; keep the backslash as written.
            out.extend(ch.encode("utf-8"))
            i += 1
    return out.decode("utf-8", "replace")

Range = tuple[int, int]


@dataclass
class FileDiff:
    """One file's section of a unified diff."""

    path: str
    status: str = "modified"          # added | modified | deleted | renamed
    old_path: str | None = None       # set when status == "renamed"
    binary: bool = False
    right: list[Range] = field(default_factory=list)
    left: list[Range] = field(default_factory=list)
    lines: list[str] = field(default_factory=list)   # the raw section, for slicing

    @property
    def added_lines(self) -> int:
        return sum(b - a + 1 for a, b in self.right)

    @property
    def removed_lines(self) -> int:
        return sum(b - a + 1 for a, b in self.left)

    def text(self) -> str:
        return "".join(self.lines)


def _collapse(numbers: list[int]) -> list[Range]:
    """[3,4,5,9] -> [(3,5),(9,9)]. Input must be ascending."""
    out: list[Range] = []
    for n in numbers:
        if out and n == out[-1][1] + 1:
            out[-1] = (out[-1][0], n)
        else:
            out.append((n, n))
    return out


def parse(text: str) -> dict[str, FileDiff]:
    """Parse a unified diff into {path: FileDiff}, keyed by the path at the review head.

    A deleted file is keyed by the path it had in the base, since that is the only path
    it has.
    """
    files: dict[str, FileDiff] = {}
    cur: FileDiff | None = None
    right_hits: list[int] = []
    left_hits: list[int] = []
    old_line = new_line = 0
    in_hunk = False

    def flush() -> None:
        nonlocal cur, right_hits, left_hits
        if cur is not None:
            cur.right = _collapse(right_hits)
            cur.left = _collapse(left_hits)
            files[cur.path] = cur
        right_hits, left_hits = [], []

    for raw in text.splitlines(keepends=True):
        line = raw.rstrip("\n")

        m = GIT_HEADER_RE.match(line)
        if m:
            flush()
            a_quoted, a_plain, b_quoted, b_plain = m.groups()
            a_path = unquote_path(a_quoted) if a_quoted is not None else a_plain
            b_path = unquote_path(b_quoted) if b_quoted is not None else b_plain
            # b/ is the head-side path; for a pure deletion git still prints it, and the
            # `+++ /dev/null` line below is what marks the file as gone.
            cur = FileDiff(path=b_path, lines=[raw])
            if a_path != b_path:
                cur.old_path = a_path
            in_hunk = False
            continue

        if cur is None:
            continue
        cur.lines.append(raw)

        if line.startswith("rename from "):
            cur.status = "renamed"
            cur.old_path = line[len("rename from "):]
            continue
        if line.startswith("Binary files ") or line.startswith("GIT binary patch"):
            cur.binary = True
            continue
        if line.startswith("--- "):
            in_hunk = False
            if line == "--- /dev/null":
                cur.status = "added"
            continue
        if line.startswith("+++ "):
            in_hunk = False
            if line == "+++ /dev/null":
                cur.status = "deleted"
                # The head-side path is meaningless for a deleted file; key it by the
                # base path so callers can find it by the only name it has.
                if cur.old_path:
                    cur.path = cur.old_path
                elif cur.path.startswith("b/"):
                    cur.path = cur.path[2:]
            continue

        m = HUNK_RE.match(line)
        if m:
            old_line = int(m.group(1))
            new_line = int(m.group(3))
            in_hunk = True
            continue

        if not in_hunk:
            continue

        # Inside a hunk body. git writes context lines with a leading space, but an
        # empty context line can arrive as a genuinely empty string.
        if line == "":
            old_line += 1
            new_line += 1
        elif line.startswith("+"):
            right_hits.append(new_line)
            new_line += 1
        elif line.startswith("-"):
            left_hits.append(old_line)
            old_line += 1
        elif line.startswith(" "):
            old_line += 1
            new_line += 1
        elif line.startswith("\\"):
            pass                      # "\ No newline at end of file"
        else:
            in_hunk = False           # trailing junk between sections

    flush()
    return files


def slice_diff(files: dict[str, FileDiff], paths: list[str] | set[str]) -> str:
    """Emit only the sections for `paths`, in the order the paths were given.

    This is what keeps a shard from being handed the whole PR's diff. On a 53-file PR
    the full diff is ~136k tokens, and handing it to each of 7 shards costs ~950k tokens
    to tell every shard about files it is forbidden to review.
    """
    order = list(paths)
    return "".join(files[p].text() for p in order if p in files)


def changed_paths(files: dict[str, FileDiff]) -> list[str]:
    return sorted(files)
