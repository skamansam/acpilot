#!/usr/bin/env python3
"""Turn `TODO.md` into one GitHub issue per checkbox.

The phases in `TODO.md` are the plan, and the task codes (`P2-5`, `P11-3`) are the
stable identifiers commits and pull requests already cite. The tracker should use
the same codes rather than invent a second vocabulary, so each issue is

    P2-5 · terminal: `terminal/create|output|wait_for_exit|kill|release`

with the phase, the code and the checkbox state in the body, and labels that say
which phase it belongs to. A task that is already checked off in `TODO.md` is
created **closed**: the roadmap is a record of what shipped as well as what is
left, and a tracker that starts at "everything is open" loses that.

A checkbox with no code — the cross-cutting work under `### Testing`,
`### Security` and friends, and the boxes under `Open decisions` — becomes an
issue too, titled by its section instead of by a code nobody cites. One box, one
card on the roadmap; grouping them into a section issue would hide work behind a
checkbox list.

Re-running is safe: an issue whose title already exists is left alone, so a
`TODO.md` edit adds what is new and never duplicates.

    python3 .github/scripts/todo_issues.py --dry-run          # what it would do
    python3 .github/scripts/todo_issues.py --apply            # create the issues
    python3 .github/scripts/todo_issues.py --apply --project 5

`--project` also adds each new issue to that Projects v2 number, which needs a
token with the `project` scope (`gh auth refresh -s project`).
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TODO = ROOT / "TODO.md"

# `- [ ] **P12-1** text`, `- [x] **P0-1 · ACP handshake against a real agent** (2d) — done`
# Codes are `P2-5`, `P4.5-3`, `XT-1`: digits, dots and the hyphen between the
# phase and the number, which `\w` alone does not cover.
CODE = r"P[\w.-]+(?<!-)"
ITEM = re.compile(rf"^- \[( |x)\] \*\*({CODE})(?: · ([^*]+?))?\*\*\s*(.*)$")
PHASE = re.compile(r"^## (.+)$")
SECTION = re.compile(r"^### (.+)$")

REPO = "skamansam/acpilot"


def _clamp(text: str, limit: int) -> str:
    """A title long enough to be a paragraph stops being a handle.

    Cut on a word boundary where there is one: `.../tests/wire_contract` at 120
    characters reads as a typo, and the body has the whole sentence anyway.
    """
    if len(text) <= limit:
        return text
    cut = text[: limit - 3]
    if " " in cut:
        cut = cut.rsplit(" ", 1)[0]
    return cut.rstrip(" ,;:·-") + "..."


@dataclass
class Task:
    code: str
    checked: bool
    name: str
    first_line: str
    phase: str
    section: str | None
    detail: list[str] = field(default_factory=list)

    @property
    def title(self) -> str:
        """`P2-5 · terminal: ...` — the code first, so sorting is by phase.

        A box with no code is titled by its section instead: `Testing · Unit tests
        per crate.` The section is the only handle it has.
        """
        summary = self.name or self.first_line
        summary = re.sub(r"\s+", " ", summary).strip()
        # A title is a handle, not the specification: the body has the whole text.
        if len(summary) > 120:
            summary = _clamp(summary, 120)
        title = f"{self.code} · {summary}" if self.code else f"{self.section or self.phase} · {summary}"
        return _clamp(title, 120)

    @property
    def labels(self) -> list[str]:
        # `P4.5` → `phase:P4.5`; the label is the code the phase prose uses.
        phase = re.match(r"P[\d.]+", self.code)
        labels = ["type:task"]
        if phase:
            labels.append(f"phase:{phase.group(0)}")
        # A codeless box keeps its section as a label, so the cross-cutting work is
        # still one search away even though it is not one issue.
        elif self.code.startswith("XT"):
            labels.append("section:dependencies-upgrades")
        elif self.section and self.section not in {self.phase}:
            slug = re.sub(r"[^a-z0-9]+", "-", self.section.lower()).strip("-")
            labels.append(f"section:{slug}")
        if self.checked:
            labels.append("status:shipped")
        return labels

    @property
    def body(self) -> str:
        parts: list[str] = []
        if self.first_line:
            parts.append(self.first_line.strip())
        if self.detail:
            parts.append("\n".join(self.detail).rstrip())
        where = f"**Phase:** {self.phase}"
        if self.section:
            where += f" → {self.section}"
        what = (
            f"**Task:** `{self.code}` — the code `TODO.md`, commits and the other "
            f"tasks refer to this work by.\n"
            if self.code
            else "**Task:** one box from `TODO.md`, with no task code of its own.\n"
        )
        parts.append(
            f"---\n{where}\n"
            + what
            + "**Source:** [`TODO.md`](TODO.md) "
            + ("— already checked off there." if self.checked else "")
        )
        return "\n\n".join(parts)


def parse(text: str) -> list[Task]:
    tasks: list[Task] = []
    phase = ""
    section: str | None = None
    current: Task | None = None

    lines = text.splitlines()
    for index, line in enumerate(lines):
        if (heading := PHASE.match(line)) and not line.startswith("###"):
            phase = heading.group(1).strip()
            section = None
            current = None
            continue
        if heading := SECTION.match(line):
            section = heading.group(1).strip()
            current = None
            continue
        if item := ITEM.match(line):
            checked = item.group(1) == "x"
            rest = item.group(4).strip()
            name = (item.group(3) or "").strip()
            if not name:
                # No `· short name`: the first clause is the best title there is.
                name = re.split(r"[.;(]", rest, maxsplit=1)[0].strip()
            current = Task(
                code=item.group(2),
                checked=checked,
                name=name,
                first_line=rest,
                phase=phase,
                section=section,
            )
            tasks.append(current)
            continue
        if line.startswith("- ["):
            # A box with no task code: the cross-cutting work under `### Testing`,
            # `### Security` and the rest, plus the boxes under a phase with no
            # `###` sections (`Open decisions`, `Definition of done`), which is its
            # own section. Some of them carry a bold code of their own (`**XT-1**`);
            # the others are titled by their section.
            checked = line.startswith("- [x]")
            after = line[6:].strip()
            coded = re.match(r"\*\*([^*]+)\*\*\s*(.*)$", after)
            code, rest = (coded.group(1).strip(), coded.group(2).strip()) if coded else ("", after)
            current = Task(
                code=code,
                checked=checked,
                # The first clause is the best title there is: these boxes were
                # written as prose, not as names.
                name=re.split(r"[.;(]", rest, maxsplit=1)[0].strip(),
                first_line=rest,
                phase=phase,
                section=section or phase,
            )
            tasks.append(current)
            continue
        if current is None:
            continue
        if line.startswith("  ") or not line.strip():
            current.detail.append(line.rstrip())
            continue
        # A task's detail is indented; the cross-cutting prose is wrapped flush
        # left, so it has to be claimed here or the box loses everything after its
        # first line. A blank line ends the claim, which is what keeps the table
        # after the last performance budget out of that box.
        previous_blank = index == 0 or not lines[index - 1].strip()
        if not current.code and not previous_blank and not line.startswith(("- ", "#", "|", ">", "```", "---")):
            current.detail.append(line.rstrip())
            continue
        current = None
    return tasks


def gh(*args: str, check: bool = True) -> str:
    result = subprocess.run(["gh", *args], capture_output=True, text=True, check=False)
    if check and result.returncode != 0:
        sys.exit(f"gh {' '.join(args)} failed:\n{result.stderr.strip()}")
    return result.stdout


def existing_titles() -> set[str]:
    """Every issue title, so a re-run is a no-op."""
    out = gh(
        "issue",
        "list",
        "--repo",
        REPO,
        "--state",
        "all",
        "--limit",
        "1000",
        "--json",
        "title",
    )
    return {issue["title"] for issue in json.loads(out or "[]")}


def existing_codes() -> set[str]:
    """Every task code that already has an issue, so a re-run is a no-op."""
    out = gh(
        "issue",
        "list",
        "--repo",
        REPO,
        "--state",
        "all",
        "--limit",
        "1000",
        "--json",
        "number,title",
    )
    codes: set[str] = set()
    for issue in json.loads(out or "[]"):
        # `CODE`, not `P\w+`: a code is `P1-3`, `P4.5-3` or `XT-1`, and the hyphen
        # is not a word character — matching `P1` against `P1-3 · ...` is how a
        # re-run once created a second issue for a task that already had one.
        if match := re.match(rf"({CODE}|XT-\d+) ·", issue["title"]):
            codes.add(match.group(1))
    return codes


def ensure_labels(tasks: list[Task]) -> None:
    wanted: dict[str, tuple[str, str]] = {
        "type:task": ("One task from TODO.md", "1d76db"),
    }
    for task in tasks:
        for label in task.labels:
            if label.startswith("phase:") and label not in wanted:
                wanted[label] = (f"Task from {label[6:]} in TODO.md", "5319e7")
    # Section labels group the codeless boxes by where they live in TODO.md. They
    # are created rather than skipped, because `gh issue create` refuses the whole
    # issue when one of its labels is unknown.
    for task in tasks:
        for label in task.labels:
            if label.startswith("section:") and label not in wanted:
                wanted[label] = (f"Cross-cutting {label[8:]} work from TODO.md", "bfd4f2")
    if any(task.checked for task in tasks):
        wanted["status:shipped"] = ("Already checked off in TODO.md", "0e8a16")

    out = gh("label", "list", "--repo", REPO, "--limit", "200", "--json", "name")
    have = {label["name"] for label in json.loads(out or "[]")}
    for name, (description, colour) in wanted.items():
        if name in have:
            continue
        print(f"label  {name}")
        gh(
            "label",
            "create",
            name,
            "--repo",
            REPO,
            "--description",
            description,
            "--color",
            colour,
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="create the issues")
    parser.add_argument("--dry-run", action="store_true", help="the default")
    parser.add_argument("--project", type=int, help="Projects v2 number to add to")
    parser.add_argument("--owner", default="skamansam", help="project owner")
    parser.add_argument("--only", help="only tasks whose code starts with this")
    args = parser.parse_args()

    tasks = parse(TODO.read_text())
    if args.only:
        tasks = [task for task in tasks if task.code.startswith(args.only)]
    if not tasks:
        print("no tasks parsed — the TODO.md shape changed")
        return 1

    open_tasks = [task for task in tasks if not task.checked]
    print(f"{len(tasks)} boxes: {len(open_tasks)} open, {len(tasks) - len(open_tasks)} done")

    if not args.apply:
        for task in tasks[:3]:
            print(f"\n{task.title}\n  labels: {', '.join(task.labels)}")
            print("  " + task.body.replace("\n", "\n  ")[:400])
        print(f"\n(dry run: {len(tasks)} issues would be created)")
        return 0

    ensure_labels(tasks)
    already = existing_codes()
    titles = existing_titles()
    created = skipped = 0

    issues: list[tuple[str, str, list[str], bool]] = [
        (task.title, task.body, task.labels, task.checked) for task in tasks
    ]

    for title, body, labels, done in issues:
        code = title.split(" · ")[0]
        if (title in titles) or (code in already and re.match(rf"({CODE}|XT-\d+)$", code)):
            skipped += 1
            continue
        create = [
            "issue",
            "create",
            "--repo",
            REPO,
            "--title",
            title,
            "--body",
            body,
        ]
        for label in labels:
            create += ["--label", label]
        url = gh(*create).strip()
        created += 1
        if done:
            # A box that is ticked in TODO.md shipped already; the issue is the
            # record of that, not a queue item.
            gh("issue", "close", url, "--repo", REPO, "--reason", "completed")
        if args.project:
            gh(
                "project",
                "item-add",
                str(args.project),
                "--owner",
                args.owner,
                "--url",
                url,
            )
        print(f"{created:3d}  {title}")

    print(f"\n{created} created, {skipped} already there")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
