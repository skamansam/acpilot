#!/usr/bin/env python3
"""Extract comments from source files and lint them with ste-lint.py.

For each file the script builds a shadow: the comment text of every source
line, dedented to column zero, one shadow line per source line. Dedented
because a comment at four spaces of indent reads as a code block to the
linter. Column zero keeps every comment in the prose path. Line numbers
stay true. Columns point into the line's comment text, not the source byte.

Comment delimiters (`//`, `/* */`, `#`, `<!-- -->`, `--`, docstring quotes) are
dropped so a marker never reads as markup. Prose files (.md, .mdx, .txt) pass
through unchanged. Files whose syntax this script does not know are skipped
silently, because the gate feeds every changed text file through here.

Usage:
    ste-comments.py FILE [FILE ...]

Exit 1 when ste-lint.py reports hard violations, 0 otherwise, 2 on usage error.
"""
import os
import re
import subprocess
import sys
import tempfile

PROSE = {".md", ".mdx", ".txt"}
# `//` line + `/* */` block. `rust` adds raw/byte strings, char-vs-lifetime and
# nested block comments. `html` adds <!-- --> for markup sections.
SLASH = {".rs": "rust", ".ts": "c", ".tsx": "c", ".js": "c", ".jsx": "c",
         ".mjs": "c", ".cjs": "c", ".css": "c", ".scss": "c", ".go": "c",
         ".c": "c", ".h": "c", ".cpp": "c", ".svelte": "html", ".astro": "html",
         ".html": "html", ".svg": "html"}
HASH_WS = {".yaml", ".yml", ".toml", ".gitignore", ".npmrc", ".cfg", ".ini"}
# `.py` keeps `#` anywhere (`x=1#foo` is a comment). Shell and PowerShell
# use `#` in expansions (`${#a}`, `$#`), so only a word-start `#` comments.
HASH_ANY = {".py"}
HASH_WS.update({".sh", ".bash", ".ps1"})
SQL = {".sql"}
SKIP = {".json", ".snap", ".lock", ".sig", ".gitattributes", ".svgz"}

CHAR_LITERAL = re.compile(r"'(?:\\.|[^\\'])'")


def _skip_string(text, i):
    """Index past a string starting at i (", ' or `), honoring backslash escapes."""
    n = len(text)
    quote = text[i]
    j = i + 1
    while j < n:
        c = text[j]
        if c == "\\":
            j += 2
            continue
        if c == quote:
            return j + 1
        if c == "\n" and quote != "`":
            return j
        j += 1
    return n


def _rust_raw_end(text, i):
    """End index of a r"..." or r#"..."# string at i, or None when not raw."""
    j = i + 1
    hashes = 0
    while j < len(text) and text[j] == "#":
        hashes += 1
        j += 1
    if j >= len(text) or text[j] != '"':
        return None
    end = '"' + "#" * hashes
    k = text.find(end, j + 1)
    return len(text) if k == -1 else k + len(end)


def _to_lines(keep, text):
    """kept comment chars -> one dedented shadow line per source line."""
    lines = []
    pos = 0
    for raw in text.split("\n"):
        frag = "".join(
            ch for ch, k in zip(raw, keep[pos:pos + len(raw)]) if k)
        lines.append(" ".join(frag.split()))
        pos += len(raw) + 1
    return "\n".join(lines)


def keep_c_like(text, lang):
    keep = bytearray(len(text))
    n = len(text)
    i = 0
    depth = 0
    block_end = "*/"
    in_block = False
    in_line = False
    # HTML markup takes `//` and `/*` as literal text (globs, URLs). Only
    # `<!-- -->` comments there. Raw-text elements get their own rules:
    # <script> uses the C set, <style> only `/* */`.
    raw = None
    while i < n:
        ch = text[i]
        two = text[i:i + 2]
        four = text[i:i + 4]
        if in_line:
            if ch == "\n":
                in_line = False
            else:
                keep[i] = 1
            i += 1
            continue
        if in_block:
            if lang == "rust" and two == "/*":
                depth += 1
                i += 2
                continue
            if text.startswith(block_end, i):
                depth -= 1
                if depth == 0:
                    in_block = False
                i += len(block_end)
                continue
            keep[i] = ch != "\n" and 1 or 0
            i += 1
            continue
        if lang == "html":
            if raw is not None:
                end = "</script" if raw == "script" else "</style"
                if text[i:i + len(end)].lower() == end:
                    raw = None
                    i += 1
                    continue
                if raw == "script" and two == "//":
                    in_line = True
                    i += 2
                    continue
                if two == "/*":
                    in_block = True
                    block_end = "*/"
                    depth = 1
                    i += 2
                    continue
                if raw == "script" and ch in "\"'`":
                    i = _skip_string(text, i)
                    continue
                i += 1
                continue
            low = text[i:i + 7].lower()
            if low == "<script" or low == "<style":
                raw = low[1:]
                gt = text.find(">", i)
                i = (gt + 1) if gt >= 0 else n
                continue
            if four == "<!--":
                in_block = True
                block_end = "-->"
                depth = 1
                i += 4
                continue
            i += 1
            continue
        if two == "//":
            in_line = True
            i += 2
            continue
        if two == "/*":
            in_block = True
            block_end = "*/"
            depth = 1
            i += 2
            continue
        if ch in "\"'`":
            if lang == "rust" and ch == "'":
                m = CHAR_LITERAL.match(text, i)
                i += len(m.group(0)) if m else 1
            else:
                i = _skip_string(text, i)
            continue
        if lang == "rust" and ch == "r":
            j = _rust_raw_end(text, i)
            if j:
                i = j
                continue
        i += 1
    return keep


def shadow_c_like(text, lang):
    return _to_lines(keep_c_like(text, lang), text)


def keep_hash(text, ws_rule, lang):
    keep = bytearray(len(text))
    n = len(text)
    i = 0
    line_start = True
    prefix_only_ws = True
    while i < n:
        ch = text[i]
        if line_start:
            if ch in " \t":
                i += 1
                continue
            line_start = False
            prefix_only_ws = True
        if ch == "\n":
            line_start = True
            prefix_only_ws = True
            i += 1
            continue
        if ch in "\"'":
            triple = text[i:i + 3]
            if triple in ("'''", '"""') and lang in ("py", "toml"):
                end = text.find(triple, i + 3)
                j = n if end == -1 else end + 3
                if lang == "py" and prefix_only_ws:
                    for k in range(i + 3, max(i + 3, j - 3)):
                        if text[k] != "\n":
                            keep[k] = 1
                    i = j
                    continue
                i = j
                continue
            i = _skip_string(text, i)
            continue
        if ch == "#":
            if (not ws_rule) or prefix_only_ws or text[i - 1] in " \t":
                j = text.find("\n", i)
                end = n if j == -1 else j
                for k in range(i + 1, end):
                    keep[k] = 1
                i = end
                continue
        if lang == "ps1" and text[i:i + 2] == "<#":
            end = text.find("#>", i + 2)
            j = n if end == -1 else end + 2
            for k in range(i + 2, j - 2):
                if text[k] != "\n":
                    keep[k] = 1
            i = j
            continue
        prefix_only_ws = prefix_only_ws and ch in " \t"
        i += 1
    return keep


def shadow_hash(text, ws_rule, lang):
    return _to_lines(keep_hash(text, ws_rule, lang), text)


def keep_sql(text):
    keep = bytearray(len(text))
    n = len(text)
    i = 0
    while i < n:
        two = text[i:i + 2]
        if two == "--":
            j = text.find("\n", i)
            end = n if j == -1 else j
            for k in range(i + 2, end):
                keep[k] = 1
            i = end
            continue
        if two == "/*":
            end = text.find("*/", i + 2)
            j = n if end == -1 else end + 2
            for k in range(i + 2, j - 2):
                if text[k] != "\n":
                    keep[k] = 1
            i = j
            continue
        if text[i] == "'":
            i = _skip_string(text, i)
            continue
        i += 1
    return keep


def shadow_sql(text):
    return _to_lines(keep_sql(text), text)


def keep_mask(path):
    """The comment positions of PATH as (text, keep) — keep[i] is 1 when text[i]
    is prose a fixer may rewrite. Prose files keep every byte. The fixer masks
    fences and code spans itself. None when the type is skipped."""
    ext = os.path.splitext(path)[1].lower()
    if ext in SKIP:
        return None
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            text = fh.read()
    except OSError:
        return None
    if ext in PROSE:
        return text, bytearray([1]) * len(text)
    if ext in SLASH:
        return text, keep_c_like(text, SLASH[ext])
    if ext in HASH_WS:
        return text, keep_hash(text, True, ext.lstrip("."))
    if ext in HASH_ANY:
        return text, keep_hash(text, False, "py" if ext == ".py" else ext.lstrip("."))
    if ext in SQL:
        return text, keep_sql(text)
    return None


def shadow(path):
    """The lintable text for PATH, or None when the type has no comment syntax."""
    ext = os.path.splitext(path)[1].lower()
    if ext in SKIP:
        return None
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            text = fh.read()
    except OSError:
        return None
    if ext in PROSE:
        return text
    if ext in SLASH:
        return shadow_c_like(text, SLASH[ext])
    if ext in HASH_WS:
        return shadow_hash(text, True, ext.lstrip("."))
    if ext in HASH_ANY:
        return shadow_hash(text, False, "py" if ext == ".py" else ext.lstrip("."))
    if ext in SQL:
        return shadow_sql(text)
    return None


def main():
    files = sys.argv[1:]
    if not files:
        print("usage: ste-comments.py FILE [FILE ...]", file=sys.stderr)
        return 2
    linter = os.path.join(os.path.dirname(os.path.abspath(__file__)), "ste-lint.py")
    shadows = {}
    with tempfile.TemporaryDirectory(prefix="ste-comments-") as tmp:
        argv = []
        for path in files:
            text = shadow(path)
            if text is None:
                continue
            name = os.path.join(tmp, f"s{len(shadows)}.txt")
            with open(name, "w", encoding="utf-8") as fh:
                fh.write(text)
            shadows[name] = path
            argv.append(name)
        if not argv:
            return 0
        proc = subprocess.run(
            [sys.executable, linter, *argv],
            capture_output=True, text=True)
        out = proc.stdout
        for tmp_path, src in shadows.items():
            out = out.replace(tmp_path + ":", src + ":")
        sys.stdout.write(out)
        if proc.stderr:
            sys.stderr.write(proc.stderr)
        return proc.returncode


if __name__ == "__main__":
    sys.exit(main())
