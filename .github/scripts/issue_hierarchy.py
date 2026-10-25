#!/usr/bin/env python3
"""Give every roadmap issue a parent, so the board can say how a phase is doing.

`TODO.md` is a tree — phases, cross-cutting sections, open decisions — and the
tracker flattened it into 243 sibling issues. A board of siblings has no answer
to "is the engine phase nearly done"; it has an answer to "how many issues are
open", which is a different question.

GitHub's sub-issues are the tree. This script creates one parent per phase and per
group, and attaches the issues that belong to it:

    Phase P2 · Engine — ACP engine, sessions, permissions
      ├── P2-1 · `acpilot-agent` crate: ACP client over stdio
      ├── P2-2 · supervisor: spawn an `acp-stdio` agent
      └── …

Parents are issues, not a second kind of object, so they land on the board like
anything else and the built-in workflows and `roadmap.mjs` treat them the same
way. Every parent is closed when its last child closes: a finished phase whose
card still says Todo is the failure this is meant to prevent.

The mapping is derived, never hand-maintained:

* a phase parent owns every issue labelled `phase:P2`
* a cross-cutting parent owns every issue labelled `section:testing`
* the decisions parent owns every `Open decisions ([decide]) · …` issue
* the done parent owns every `Definition of done for v0.1.0 · …` issue

    python3 .github/scripts/issue_hierarchy.py             # what it would do
    python3 .github/scripts/issue_hierarchy.py --apply     # create and attach
    python3 .github/scripts/issue_hierarchy.py --only P2 --apply
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TODO = ROOT / "TODO.md"
REPO = "skamansam/acpilot"

PHASE = re.compile(r"^## (?P<name>.+?) \((?P<code>P[\d.]+)\)(?: — (?P<detail>.*))?$")
GROUP = re.compile(r"^## (?P<name>Open decisions|Definition of done for v0\.1\.0)")

# The two groups that are not phases. Their boxes are issues with no task code,
# so the parent is the only thing that groups them.
GROUPS = {
    "Open decisions ([decide])": "Open decisions ([decide])",
    "Definition of done for v0.1.0": "Definition of done for v0.1.0",
}

SECTION = re.compile(r"^### (?P<name>.+)$")
CROSS_CUTTING = re.compile(r"^## Cross-cutting work")


@dataclass
class Parent:
    """One parent issue, and the rule that says which issues belong to it."""

    title: str
    body: str
    # `label:phase:P2`, `label:section:testing`, `prefix:Open decisions ([decide]) ·`
    rule: str
    # For a re-run: the existing issue with this title, or None.
    number: int | None = None
    id: int | None = None

    @property
    def kind(self) -> str:
        return self.rule.split(":", 1)[0]

    @property
    def matches(self) -> str:
        return self.rule.split(":", 1)[1]

    def owns(self, issue: dict) -> bool:
        if self.kind == "label":
            return any(label["name"] == self.matches for label in issue["labels"])
        return issue["title"].startswith(self.matches)


def _clean(detail: str | None) -> str:
    """The heading's tail, without the estimate or the checkmark.

    `(≈12 days)`, `(post-v1, ≈40 days)` and `**done**` are schedule and status
    facts the board already knows, so none of them belongs in a parent's title.
    """
    if not detail:
        return ""
    detail = re.sub(r"\([^)]*≈[^)]*\)", "", detail)
    detail = detail.replace("(post-v1)", "").replace("**done**", "")
    return re.sub(r"\s+", " ", detail).strip(" —-")


def parents() -> list[Parent]:
    """Every parent the tree in `TODO.md` implies, in document order."""
    parents: list[Parent] = []
    phase: str | None = None
    cross_cutting = False

    for line in TODO.read_text().splitlines():
        if match := PHASE.match(line):
            name = re.sub(r"\s+phase$", "", match.group("name").strip())
            code = match.group("code")
            detail = _clean(match.group("detail"))
            title = f"Phase {code.lstrip('P')} · {name}" + (f" — {detail}" if detail else "")
            parents.append(
                Parent(
                    title=_clamp(title),
                    body=(
                        f"{match.group('name')} from `TODO.md`"
                        + (f": {detail}." if detail else ".")
                        + f"\n\nThe work is the issues labelled `phase:{code}`; each is a"
                        " sub-issue of this one, so the count on this card is the phase's"
                        " progress.\n\n**Source:** [`TODO.md`](TODO.md)"
                    ),
                    rule=f"label:phase:{code}",
                )
            )
            phase = code
            cross_cutting = False
            continue

        if CROSS_CUTTING.match(line):
            cross_cutting = True
            phase = None
            continue

        if match := GROUP.match(line):
            name = match.group("name")
            heading = line.removeprefix("## ").strip()
            title = GROUPS.get(heading, heading)
            prefix = "Open decisions ([decide]) ·" if name.startswith("Open") else "Definition of done for v0.1.0 ·"
            parents.append(
                Parent(
                    title=title,
                    body=(
                        f"`{heading}` from `TODO.md`.\n\n"
                        f'The work is the issues titled `{prefix} …`; each is a sub-issue of'
                        " this one.\n\n**Source:** [`TODO.md`](TODO.md)"
                    ),
                    rule=f"prefix:{prefix}",
                )
            )
            cross_cutting = False
            phase = None
            continue

        # A `###` section only names a parent under Cross-cutting work: phase
        # sections are already covered by the phase parent above them.
        if cross_cutting and (match := SECTION.match(line)):
            heading = match.group("name").strip()
            # `Performance budgets (measured in CI on a reference runner)` — the
            # parenthetical is a note to a reader, not part of the name.
            name = re.sub(r"\s*\([^)]*\)\s*$", "", heading)
            slug = re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")
            parents.append(
                Parent(
                    title=f"Cross-cutting · {_clamp(name)}",
                    body=(
                        f"`{heading}` under Cross-cutting work in `TODO.md`.\n\n"
                        f"The work is the issues labelled `section:{slug}`; each is a"
                        " sub-issue of this one.\n\n**Source:** [`TODO.md`](TODO.md)"
                    ),
                    rule=f"label:section:{slug}",
                )
            )

    return parents


def _clamp(title: str, limit: int = 120) -> str:
    if len(title) <= limit:
        return title
    return title[: limit - 3].rstrip(" ,;:—-") + "..."


def gh(*args: str, check: bool = True) -> str:
    result = subprocess.run(["gh", *args], capture_output=True, text=True, check=False)
    if check and result.returncode != 0:
        sys.exit(f"gh {' '.join(args)} failed:\n{result.stderr.strip()}")
    return result.stdout


def rest(path: str, check: bool = True) -> str:
    return gh("api", path, check=check)


def issues() -> list[dict]:
    """Every issue in the repository, once, so a re-run costs no extra calls."""
    out: list[dict] = []
    page = 1
    while True:
        batch = json.loads(rest(f"repos/{REPO}/issues?state=all&per_page=100&page={page}"))
        if not batch:
            return out
        out.extend(entry for entry in batch if "pull_request" not in entry)
        if len(batch) < 100:
            return out
        page += 1


def existing_parent(parent: Parent, known: list[dict]) -> dict | None:
    for issue in known:
        if issue["title"] == parent.title:
            return issue
    return None


def create_parent(parent: Parent) -> dict:
    url = gh(
        "issue",
        "create",
        "--repo",
        REPO,
        "--title",
        parent.title,
        "--body",
        parent.body,
        "--label",
        "type:task",
    ).strip()
    # The created URL is the number; the database id the sub-issue API wants is
    # only on the issue itself, so read it back rather than parse the URL. A read
    # in the same second as the create can still answer 404, so it is retried
    # rather than being allowed to strand a half-built tree.
    number = url.rsplit("/", 1)[-1]
    for attempt in range(4):
        read = rest(f"repos/{REPO}/issues/{number}", check=False).strip()
        if read:
            return json.loads(read)
        time.sleep(1 + attempt)
    sys.exit(f"created #{number}, but reading it back kept answering 404")


def children_of(number: int) -> set[int]:
    """The issue numbers already attached, so a re-run does not attach twice."""
    seen: set[int] = set()
    page = 1
    while True:
        batch = json.loads(
            rest(f"repos/{REPO}/issues/{number}/sub_issues?per_page=100&page={page}")
        )
        if not batch:
            return seen
        seen.update(entry["number"] for entry in batch)
        if len(batch) < 100:
            return seen
        page += 1


def attach(parent_number: int, child: dict) -> None:
    # `sub_issue_id` is the issue's database id, not its number: the two are
    # different namespaces and the API gives no hint when they are confused.
    gh(
        "api",
        "--method",
        "POST",
        f"repos/{REPO}/issues/{parent_number}/sub_issues",
        "-F",
        f"sub_issue_id={child['id']}",
    )


def close_parent(number: int) -> None:
    gh("issue", "close", str(number), "--repo", REPO, "--reason", "completed")


def board(number: int) -> None:
    """Put a parent on the roadmap, in the column its state earns.

    `roadmap.mjs` already owns that mapping, including "a closed issue is Done",
    so there is no second copy of it here. It reads `GH_TOKEN`, which a caller
    who is signed in to `gh` has not exported.
    """
    token = os.environ.get("GH_TOKEN") or gh("auth", "token").strip()
    result = subprocess.run(
        ["node", str(ROOT / ".github/scripts/roadmap.mjs"), "issue", str(number)],
        cwd=ROOT,
        env={**os.environ, "GH_TOKEN": token, "GITHUB_REPOSITORY": REPO},
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.exit(f"roadmap.mjs issue {number} failed:\n{result.stderr.strip()}")
    for line in result.stdout.splitlines():
        print(f"  board: {line}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="create and attach")
    parser.add_argument("--only", help="only parents whose rule mentions this code (`P2`, `testing`)")
    args = parser.parse_args()

    specs = parents()
    if not specs:
        print("no parents parsed — the TODO.md shape changed")
        return 1
    if args.only:
        # `--only P1` must not pick up `phase:P15`, so the code has to end the rule.
        pattern = re.compile(rf"(^|:){re.escape(args.only)}$")
        specs = [spec for spec in specs if pattern.search(spec.rule)]

    known = issues()
    by_title = {issue["title"]: issue for issue in known}
    total = 0
    made = 0

    for spec in specs:
        owned = [issue for issue in known if spec.owns(issue)]
        found = by_title.get(spec.title)
        if not owned and found is None:
            # A heading with no boxes under it — the performance-budget table is
            # the one — is a section of the document, not a parent of any work.
            print(f"\n{spec.title}\n  no issues, no parent")
            continue
        print(f"\n{spec.title}")
        print(f"  {len(owned)} issues, parent {'#' + str(found['number']) if found else 'to create'}")

        if not args.apply:
            for issue in owned[:3]:
                print(f"    #{issue['number']} {issue['title'][:80]}")
            if len(owned) > 3:
                print(f"    … and {len(owned) - 3} more")
            total += len(owned)
            made += 1
            continue

        if found is None:
            found = create_parent(spec)
            by_title[spec.title] = found
            print(f"  created #{found['number']}")
        made += 1
        spec.number, spec.id = found["number"], found["id"]

        attached = children_of(spec.number)
        for issue in owned:
            if issue["number"] in attached:
                continue
            attach(spec.number, issue)
            print(f"  + #{issue['number']} {issue['title'][:70]}")
        total += len(owned)

        # A parent is done when its last child is, and a parent that still says
        # Todo over a finished phase is the thing this script exists to avoid.
        if owned and all(issue["state"] == "closed" for issue in owned):
            if found["state"] == "open":
                close_parent(spec.number)
                print(f"  closed #{spec.number}: every child is closed")

        board(spec.number)

    verb = "attached" if args.apply else "would attach"
    print(f"\nroadmap: {made} parents, {total} children {verb}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
