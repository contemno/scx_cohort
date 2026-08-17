#!/usr/bin/env bash
# next-version.sh — compute the next release version (bare X.Y.Z) for autotag.
#
#   next = max(patch-bump-of-latest-release, floor)
#
#   * floor: the intended minimum version, hand-maintained in ./VERSION. Bump
#     its minor/major to cut a minor/major release.
#   * patch-bump: the latest plain vX.Y.Z release tag with patch + 1 (prerelease
#     -dev.* tags are ignored). With no release tag yet, the floor is the first
#     release.
#
# Taking the max of the two means a floor bump (e.g. 0.1.x -> 0.2.0) cuts exactly
# one minor/major release; once the latest release reaches the floor, the
# patch-bump wins again and patch releases resume — no revert needed, and a stale
# floor never lowers or re-cuts a version.
#
# Prints the bare X.Y.Z to stdout; autotag.yml decorates it into the
# vX.Y.Z / vX.Y.Z-dev.<date>.<sha> tag.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

if [ ! -f VERSION ]; then
    echo "next-version: ./VERSION not found (it holds the version floor, e.g. 0.1.0)" >&2
    exit 1
fi
floor="$(tr -d ' \t\r\n' < VERSION)"
floor="${floor#v}"  # tolerate an accidental leading 'v'

if ! printf '%s' "$floor" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "next-version: version floor '$floor' is not X.Y.Z (e.g. 0.2.0)" >&2
    exit 1
fi

# Patch-bump candidate from the latest plain vX.Y.Z release tag.
latest="$(git tag -l 'v*' \
    | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -n1 || true)"
if [ -n "$latest" ]; then
    IFS=. read -r major minor patch <<<"${latest#v}"
    bump="${major}.${minor}.$((patch + 1))"
else
    bump="$floor"
fi

# next = max(bump, floor) in version order; the floor never lowers the result.
printf '%s\n' "$bump" "$floor" | sort -V | tail -n1
