#!/usr/bin/env bash
# Lint prose a change touches, against the structural ASD-STE100 rules. The
# AGENTS.md prose rule applies when a file is touched, so this checks each
# changed file in full rather than the diff alone. .md/.mdx files are linted
# whole. Source files are linted through their comments.
#
# Usage:
#   scripts/ste-check.sh [BASE]        # files changed vs BASE..HEAD (default: origin/main)
#   scripts/ste-check.sh --docs [BASE] # only .md, .mdx and .txt files
#   scripts/ste-check.sh --files F...  # an explicit file list (the pre-commit hook)
#
# TODO.md is exempt: it is a ledger edited on every ticket close, not
# documentation prose. Fixtures are excluded: they are test data, and linting
# them can break the tests that read them.
#
# The linter's own files stay out. Their fixtures must contain the violations
# the rules detect, so linting them would fail on purpose.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

EXCLUDE='^(scripts/ste-|target/|node_modules/|site/dist/|probes/fixtures/|crates/acpilot-web/assets/|web/src/api/schema\.d\.ts$|TODO\.md$)'

docs_only=0
if [ "${1:-}" = "--files" ]; then
    shift
    files=("$@")
else
    if [ "${1:-}" = "--docs" ]; then
        docs_only=1
        shift
    fi
    base="${1:-origin/main}"
    mapfile -t files < <(git diff --name-only --diff-filter=ACM "${base}...HEAD")
    mapfile -t untracked < <(git ls-files --others --exclude-standard)
    files+=("${untracked[@]}")
fi

targets=()
for f in "${files[@]}"; do
    if [[ "$f" =~ $EXCLUDE ]]; then continue; fi
    if [ "$docs_only" = 1 ] && [[ ! "$f" =~ \.(md|mdx|txt)$ ]]; then continue; fi
    if [ -f "$f" ]; then targets+=("$f"); fi
done

if [ ${#targets[@]} -eq 0 ]; then
    echo "ste-check: no prose files changed"
    exit 0
fi

echo "ste-check: linting ${#targets[@]} file(s): ${targets[*]}"
exec python3 scripts/ste-comments.py "${targets[@]}"
