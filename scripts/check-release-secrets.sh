#!/usr/bin/env bash
# Warn when a release credential is close to its end, or already needs attention.
#
# `HOMEBREW_TAP_TOKEN` pushes the formula to the tap, and GitHub can attach an end
# date to it. `ACPILOT_SIGNING_KEY` has no end date at all: it works until a person
# replaces it. So the tap token reports its own end, and the signing key reports the
# date a repository variable records for its rotation.
#
# Usage:
#   HOMEBREW_TAP_TOKEN=... ACPILOT_SIGNING_KEY=... scripts/check-release-secrets.sh
#
# Exit status is 1 when a credential is past its end, and 0 otherwise. A credential
# inside the warning window exits 0 with a GitHub annotation. A weekly job that fails
# for a month teaches people to ignore it.
set -euo pipefail

WARN_DAYS="${RELEASE_SECRET_WARN_DAYS:-30}"
status=0

summary() {
    printf '%s\n' "$1"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        printf '%s\n' "$1" >>"$GITHUB_STEP_SUMMARY"
    fi
}

warn() {
    summary "::warning title=Release credential::$*"
}

fail_hard() {
    summary "::error title=Release credential::$*"
    status=1
}

# Days from now until the timestamp in `$1`, as a whole number. A timestamp in the
# past gives a negative number, and one this script cannot read gives `unknown`.
days_until() {
    STAMP="$1" python3 - <<'PY'
import datetime
import os
import sys

raw = os.environ["STAMP"].strip()
formats = (
    "%Y-%m-%d %H:%M:%S %Z",
    "%Y-%m-%dT%H:%M:%SZ",
    "%Y-%m-%dT%H:%M:%S%z",
    "%Y-%m-%d",
)
stamp = None
for fmt in formats:
    try:
        stamp = datetime.datetime.strptime(raw, fmt)
        break
    except ValueError:
        continue
if stamp is None:
    print("unknown")
    sys.exit(0)
now = datetime.datetime.now(datetime.timezone.utc)
if stamp.tzinfo is None:
    stamp = stamp.replace(tzinfo=datetime.timezone.utc)
print(int((stamp - now).total_seconds() // 86400))
PY
}

check_tap_token() {
    local token="${HOMEBREW_TAP_TOKEN:-}"
    if [ -z "$token" ]; then
        warn "HOMEBREW_TAP_TOKEN is not set, so the tap gets no formula and no check."
        return
    fi

    local headers
    if ! headers=$(curl -sS -D - -o /dev/null \
        -H "Authorization: Bearer $token" https://api.github.com/user 2>&1); then
        warn "GitHub did not answer the token request, so the tap token has no check."
        return
    fi

    # A fine-grained token carries this header. A classic token and an app token do not,
    # and neither has an end date to warn about.
    local expiry
    expiry=$(printf '%s\n' "$headers" | tr -d '\r' \
        | sed -n 's/^[Gg]ithub-[Aa]uthentication-[Tt]oken-[Ee]xpiration: *//p' | head -1)
    if [ -z "$expiry" ]; then
        summary "HOMEBREW_TAP_TOKEN: GitHub reports no end date for this token."
        return
    fi

    local days
    days=$(days_until "$expiry")
    case "$days" in
    unknown)
        warn "The tap token ends on a date this script cannot read: $expiry"
        ;;
    -* | 0)
        fail_hard "HOMEBREW_TAP_TOKEN ended on $expiry. Replace it before the next release."
        ;;
    *)
        if [ "$days" -le "$WARN_DAYS" ]; then
            warn "HOMEBREW_TAP_TOKEN ends in $days days, on $expiry."
        else
            summary "HOMEBREW_TAP_TOKEN: $days days left, to $expiry."
        fi
        ;;
    esac
}

check_rotate_by() {
    local by="${ACPILOT_KEY_ROTATE_BY:-}"
    if [ -z "$by" ]; then
        summary "ACPILOT_KEY_ROTATE_BY is not set, so the signing key records no rotation date."
        return
    fi

    local days
    days=$(days_until "$by")
    case "$days" in
    unknown)
        warn "ACPILOT_KEY_ROTATE_BY is not a date this script can read: $by"
        ;;
    -* | 0)
        warn "The signing key passed its rotation date, $by. Release the new key, then sign with it."
        ;;
    *)
        if [ "$days" -le "$WARN_DAYS" ]; then
            warn "The signing key reaches its rotation date in $days days, on $by."
        else
            summary "The signing key: $days days to its rotation date, $by."
        fi
        ;;
    esac
}

check_signing_key() {
    local key="${ACPILOT_SIGNING_KEY:-}"
    local pub="${ACPILOT_RELEASE_PUBKEY:-}"
    if [ -z "$key" ]; then
        warn "ACPILOT_SIGNING_KEY is not set, so no release can be signed."
        return
    fi

    local dir
    dir=$(mktemp -d)
    trap 'rm -rf "$dir"' RETURN
    printf '%s\n' "$key" >"$dir/key.pem"
    printf 'acpilot release key check\n' >"$dir/sample"
    if ! openssl pkeyutl -sign -rawin -inkey "$dir/key.pem" \
        -in "$dir/sample" -out "$dir/sample.sig" 2>/dev/null; then
        fail_hard "ACPILOT_SIGNING_KEY is not a private key that OpenSSL can read."
        return
    fi

    if [ -z "$pub" ]; then
        warn "ACPILOT_RELEASE_PUBKEY is not set, so no build can verify a signature."
        check_rotate_by
        return
    fi

    local derived
    derived=$(openssl pkey -in "$dir/key.pem" -pubout -outform DER 2>/dev/null \
        | tail -c 32 | od -An -tx1 -v | tr -d ' \n')
    if [ "$derived" != "$(printf '%s' "$pub" | tr 'A-Z' 'a-z')" ]; then
        fail_hard "ACPILOT_SIGNING_KEY does not match ACPILOT_RELEASE_PUBKEY. A rotation stopped halfway."
    else
        summary "ACPILOT_SIGNING_KEY matches ACPILOT_RELEASE_PUBKEY, and it signs."
    fi
    check_rotate_by
}

check_tap_token
check_signing_key

if [ "$status" -ne 0 ]; then
    summary "A release credential needs attention before the next release."
fi
exit "$status"
