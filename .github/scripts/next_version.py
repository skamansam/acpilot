#!/usr/bin/env python3
"""Work out the next version from the labels on a merged pull request.

The tag is the version. Labels say what kind of change a PR is, SemVer says what
that does to the number, and the highest of them wins — a PR that is both a fix
and a feature is a feature, because shipping it as a patch would hide the new
thing from anyone reading the changelog.

    breaking / major            → major
    feature / enhancement       → minor
    bug / docs / chore / …      → patch
    skip-release                → no release

A PR with none of these is a patch: something shipped, and pretending it did not
is worse than a slightly generous patch bump. The mapping is data at the top of
this file so a new label is a line, not a code path.

Run it with `--labels`, or let the release workflow call it with the labels GitHub
reports; either way it prints one line, `version=X.Y.Z`, for `$GITHUB_OUTPUT`.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys

# Label (lowercase) → bump. Add labels here rather than special-casing them.
MAJOR = {"breaking", "major", "breaking-change", "breaking change"}
MINOR = {"feature", "enhancement", "minor", "new-feature"}
PATCH = {
    "bug",
    "fix",
    "bugfix",
    "docs",
    "documentation",
    "chore",
    "refactor",
    "test",
    "tests",
    "ci",
    "build",
    "dependencies",
    "dependency",
    "performance",
    "perf",
    "security",
    "style",
    "maintenance",
}
SKIP = {"skip-release", "no-release", "release-skip"}

RANK = {"none": 0, "patch": 1, "minor": 2, "major": 3}


def bump_for(labels: list[str]) -> str:
    """The strongest bump the labels ask for."""
    wanted = "none"
    for label in labels:
        name = label.strip().lower()
        if name in SKIP:
            # An explicit skip outranks everything: someone said no.
            return "none"
        if name in MAJOR:
            wanted = "major"
        elif name in MINOR and RANK[wanted] < RANK["minor"]:
            wanted = "minor"
        elif name in PATCH and RANK[wanted] < RANK["patch"]:
            wanted = "patch"
        # An unknown label is silent: a project adds labels for its own reasons
        # and a release should not fail because one of them is unfamiliar.
    return wanted if wanted != "none" else "patch"


def last_version(tags: list[str]) -> tuple[int, int, int] | None:
    """The highest `vX.Y.Z` tag, or `None` when nothing has been released."""
    versions = []
    pattern = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")
    for tag in tags:
        match = pattern.match(tag.strip())
        if match:
            versions.append(tuple(int(part) for part in match.groups()))
    return max(versions) if versions else None


def next_version(current: tuple[int, int, int], bump: str) -> tuple[int, int, int]:
    major, minor, patch = current
    if bump == "major":
        # SemVer, not project taste: a breaking change is `major + 1.0.0`, which
        # for a 0.x line means 1.0.0. A project that wants to stay in 0.x uses the
        # `skip-release` label and cuts the version by hand.
        return (major + 1, 0, 0)
    if bump == "minor":
        return (major, minor + 1, 0)
    return (major, minor, patch + 1)


def tags_from_git() -> list[str]:
    output = subprocess.run(
        ["git", "tag", "--list", "v*"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return output.split()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--labels",
        default="",
        help="comma-separated labels from the pull request",
    )
    parser.add_argument(
        "--current",
        help="the version to bump from; defaults to the highest vX.Y.Z tag",
    )
    parser.add_argument(
        "--bump",
        choices=sorted(RANK),
        help="override the label-derived bump (a maintainer deciding by hand)",
    )
    args = parser.parse_args()

    labels = [label for label in args.labels.split(",") if label.strip()]
    bump = args.bump or bump_for(labels)
    if bump == "none":
        print("bump=none", flush=True)
        print("version=", flush=True)
        return 0

    tags = [args.current] if args.current else tags_from_git()
    previous = last_version(tags)
    # The first release is 0.1.0 whatever its labels say: there is nothing to
    # patch or feature against yet, and 0.x says "not stable" honestly.
    version = (0, 1, 0) if previous is None else next_version(previous, bump)
    text = ".".join(str(part) for part in version)
    print(f"bump={bump}", flush=True)
    print(f"version={text}", flush=True)
    print(f"tag=v{text}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
