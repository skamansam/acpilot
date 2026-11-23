#!/usr/bin/env python3
"""Apply the mechanical ASD-STE100 fixes a linter can place.

Semicolons become sentence breaks. Flagged long sentences split at an em
dash or a ", and", ", but" or ", so" boundary with enough words on both
sides. Rewrites happen inside the comment mask ste-comments.py computes,
so code, strings and code spans stay untouched. What this script cannot
fix stays for a hand pass. Run ste-comments.py afterwards to see the
remainder.

Usage:
    ste-fix.py FILE [FILE ...]
"""
import importlib.util
import os
import re
import subprocess
import sys

spec = importlib.util.spec_from_file_location(
    "ste_comments", os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                 "ste-comments.py"))
ste = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ste)

LINTER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "ste-lint.py")
FENCE = re.compile(r"^\s*(```|~~~)")
URL = re.compile(r"https?://\S+|<[^>]+>|`[^`]*`|&[a-zA-Z]+;|\]\([^)]*\)")
WORD_START = re.compile(r"[\[\('`*_]*([a-zA-Z])")


def prose_mask(text):
    """All-kept mask minus fenced blocks, indented code, code spans and URLs."""
    keep = bytearray([1]) * len(text)
    in_fence = False
    pos = 0
    for line in text.split("\n"):
        end = pos + len(line)
        if FENCE.match(line):
            keep[pos:end] = b"\0" * len(line)
            in_fence = not in_fence
        elif in_fence or line.startswith("    ") or line.startswith("\t"):
            keep[pos:end] = b"\0" * len(line)
        else:
            for m in URL.finditer(line):
                keep[pos + m.start():pos + m.end()] = b"\0" * (m.end() - m.start())
        pos = end + 1
    return keep


def kept_lines(text, keep):
    """Per line: list of (start, end) kept spans in the original text."""
    spans, pos = [], 0
    for line in text.split("\n"):
        line_spans, i = [], pos
        while i < pos + len(line):
            if keep[i]:
                j = i
                while j < pos + len(line) and keep[j]:
                    j += 1
                line_spans.append((i, j))
                i = j
            else:
                i += 1
        spans.append(line_spans)
        pos += len(line) + 1
    return spans


def fix_semicolons(text, keep):
    edits, n, i = [], len(text), 0
    while i < n:
        if text[i] == ";" and keep[i]:
            j = i + 1
            while j < n and text[j] in " )\"'`":
                j += 1
            prev = i - 1
            while prev >= 0 and text[prev] == " ":
                prev -= 1
            edits.append((i, i + 1, "" if prev >= 0 and text[prev] == "." else "."))
            m = WORD_START.match(text[j:]) if j < n and keep[j] else None
            if m and text[j] != "\n" and m.group(1).islower():
                start = j + m.start(1)
                edits.append((start, start + 1, m.group(1).upper()))
        i += 1
    return apply_edits(text, edits)


def apply_edits(text, edits):
    for (a, b, rep) in sorted(edits, reverse=True):
        text = text[:a] + rep + text[b:]
    return text


def lint_lines(path):
    """Line numbers with hard long-sentence findings, from ste-comments.py."""
    proc = subprocess.run(
        [sys.executable, os.path.join(os.path.dirname(LINTER),
                                      "ste-comments.py"), path],
        capture_output=True, text=True)
    lines = set()
    for m in re.finditer(rf"{re.escape(path)}:(\d+):\d+ long-sentence",
                         proc.stdout):
        lines.add(int(m.group(1)))
    return lines


PATS = [
    re.compile(r" — "),          # em dash
    re.compile(r" – "),          # en dash
    re.compile(r", (?:but|so) "),
    re.compile(r", and "),
]


def fix_long(text, keep, flagged):
    """One split per flagged region, tried at each pattern in order."""
    edits = []
    floor = len(text)
    for (a, b) in sorted(flagged, reverse=True):
        if b > floor:
            continue
        for pat in PATS:
            m = pat.search(text, a, b)
            if not m:
                continue
            if pat is PATS[3] and len(text[m.end():b].split()) < 4:
                continue
            edits.append((m.start(), m.end(), ". "))
            nxt = WORD_START.match(text[m.end():])
            if nxt and m.end() + nxt.start(1) < len(keep) \
                    and keep[m.end() + nxt.start(1)] and nxt.group(1).islower():
                s = m.end() + nxt.start(1)
                edits.append((s, s + 1, nxt.group(1).upper()))
            floor = a
            break
    if not edits:
        return text
    for (a, b, rep) in sorted(edits, reverse=True):
        text = text[:a] + rep + text[b:]
    return text


def sentence_regions(text, keep, lines):
    """Flagged lines expanded to (start, end) over contiguous kept lines."""
    spans = kept_lines(text, keep)
    regions = []
    for ln in sorted(lines):
        a = None
        cur = ln - 1
        while cur < len(spans) and spans[cur]:
            if a is None:
                a = spans[cur][0][0]
            b = spans[cur][-1][1]
            cur += 1
        if a is not None:
            regions.append((a, b))
    return regions


def keep_for(path, text):
    """The comment mask for TEXT read as PATH's file type."""
    ext = os.path.splitext(path)[1].lower()
    if ext in ste.PROSE:
        return prose_mask(text)
    if ext in ste.SLASH:
        return ste.keep_c_like(text, ste.SLASH[ext])
    if ext in ste.HASH_WS:
        return ste.keep_hash(text, True, ext.lstrip("."))
    if ext in ste.HASH_ANY:
        return ste.keep_hash(text, False, "py" if ext == ".py" else ext.lstrip("."))
    if ext in ste.SQL:
        return ste.keep_sql(text)
    return None


def fix_file(path):
    got = ste.keep_mask(path)
    if not got:
        return 0
    text = got[0]
    ext = os.path.splitext(path)[1].lower()
    if ext in ste.SKIP or (ext not in ste.PROSE and ext not in ste.SLASH
                           and ext not in ste.HASH_WS and ext not in ste.HASH_ANY
                           and ext not in ste.SQL):
        return 0
    keep = keep_for(path, text)
    new = fix_semicolons(text, keep)
    for _ in range(3):
        flagged = lint_lines(path) if new == text else lint_lines_text(path, new)
        if not flagged:
            break
        keep = keep_for(path, new)
        regions = sentence_regions(new, keep, flagged)
        if not regions:
            break
        again = fix_long(new, keep, regions)
        if again == new:
            break
        new = again
    if new != text:
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(new)
        return 1
    return 0


def lint_lines_text(path, text):
    """Flagged lines when the on-disk file is not yet rewritten."""
    import tempfile
    with tempfile.NamedTemporaryFile(
            "w", suffix=os.path.splitext(path)[1], delete=False) as fh:
        fh.write(text)
        tmp = fh.name
    try:
        proc = subprocess.run(
            [sys.executable, os.path.join(os.path.dirname(LINTER),
                                          "ste-comments.py"), tmp],
            capture_output=True, text=True)
        lines = set()
        for m in re.finditer(rf"{re.escape(tmp)}:(\d+):\d+ long-sentence",
                             proc.stdout):
            lines.add(int(m.group(1)))
        return lines
    finally:
        os.unlink(tmp)


def main():
    changed = sum(fix_file(p) for p in sys.argv[1:])
    print(f"ste-fix: rewrote {changed} file(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
