#!/usr/bin/env python3
"""Protocol checks for `acpilotd-helper`: the part `rustc --test` cannot reach.

`cargo test` covers the pure functions inside the helper; this script starts the
helper as a process and checks the contract the daemon relies on — one JSON line
out per non-blank line in, ids echoed, a 1 MiB line intact, a clean exit on stdin
EOF, and a loud refusal for any argument but `--stdio`.

It is stdlib-only and runs on either side, which is the point: the same script
against `~/.local/bin/acpilotd-helper` inside the distro and against a
`rustc -O` build on Windows proves the helper is portable, not just portable in
theory.

    # inside the distro
    python3 spikes/wsl-helper/helper/check_protocol.py ~/.local/bin/acpilotd-helper

    # on Windows (build it first: rustc -O acpilotd-helper.rs -o acpilotd-helper.exe)
    python spikes\\wsl-helper\\helper\\check_protocol.py acpilotd-helper.exe
"""

import json
import subprocess
import sys

failures = []


def check(name, ok, detail=""):
    print(("ok   " if ok else "FAIL ") + name + (("  " + detail) if detail else ""))
    if not ok:
        failures.append(name)


def main(helper):
    # --version: one line, exit 0
    v = subprocess.run([helper, "--version"], capture_output=True, text=True)
    check("--version exit 0", v.returncode == 0, f"rc={v.returncode}")
    check("--version names the spike", "acpilotd-helper" in v.stdout, v.stdout.strip())

    # unknown argument: refusal on stderr, exit 2, nothing on stdout
    u = subprocess.run([helper, "--bogus"], capture_output=True, text=True)
    check("unknown arg exit 2", u.returncode == 2, f"rc={u.returncode}")
    check("unknown arg on stderr", "--bogus" in u.stderr)
    check("unknown arg keeps stdout clean", u.stdout == "")

    # the protocol: blank lines skipped, ids echoed, 1 MiB line intact, EOF exit 0
    pad = "x" * (1 << 20)
    lines = [
        "",
        '{"jsonrpc":"2.0","id":1,"method":"ping"}',
        "   ",
        '{"jsonrpc":"2.0","id":2,"method":"ping","params":{"note":"second"}}',
        '{"jsonrpc":"2.0","id":4242,"method":"ping","params":{"padding":"%s"}}' % pad,
    ]
    payload = "\n".join(lines) + "\n"
    p = subprocess.run([helper, "--stdio"], input=payload, capture_output=True, text=True)
    out = [line for line in p.stdout.splitlines() if line.strip()]
    check("EOF exit 0", p.returncode == 0, f"rc={p.returncode}")
    check("blank lines skipped (3 responses)", len(out) == 3, f"got {len(out)}")
    if len(out) != 3:
        return
    parsed = [json.loads(line) for line in out]
    check(
        "ids echoed in order",
        [r["id"] for r in parsed] == [1, 2, 4242],
        str([r.get("id") for r in parsed]),
    )
    check(
        "large line byte count",
        parsed[2]["result"]["request_bytes"] == len(lines[4]),
        f'{parsed[2]["result"]["request_bytes"]} vs {len(lines[4])}',
    )
    check("one response per request (no buffering)", len(p.stdout.splitlines()) == 3,
          f"got {len(p.stdout.splitlines())}")
    check("stderr quiet", p.stderr == "", p.stderr[:200])
    check(
        "argv is reported",
        parsed[0]["result"]["argv"] == ["--stdio"],
        str(parsed[0]["result"].get("argv")),
    )


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__.strip().splitlines()[-1])
        sys.exit(2)
    main(sys.argv[1])
    print()
    print("PASS" if not failures else "FAILED: " + ", ".join(failures))
    sys.exit(0 if not failures else 1)
