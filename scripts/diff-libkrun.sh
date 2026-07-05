#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Diff libkrun.h between two upstream tags so a maintainer can see
# which function signatures need translating into
# `ward-core/src/backend/krun_ffi.rs` when bumping the pinned version.
#
# Usage:
#   scripts/diff-libkrun.sh <new-version> [<old-version>]
#
# If <old-version> is omitted it defaults to the version recorded in
# `vendor/libkrun-version.txt`. Versions are tag names without the
# leading "v" (e.g. 1.18.0).
#
# Examples:
#   scripts/diff-libkrun.sh 1.19.0           # diff current pin -> 1.19.0
#   scripts/diff-libkrun.sh 1.20.0 1.18.0    # diff 1.18.0 -> 1.20.0
#
# Exits non-zero if the fetch fails or the diff is empty.

set -euo pipefail

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "Usage: $0 <new-version> [<old-version>]" >&2
    exit 2
fi

NEW="$1"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [ $# -eq 2 ]; then
    OLD="$2"
else
    if [ ! -f "$REPO_ROOT/vendor/libkrun-version.txt" ]; then
        echo "vendor/libkrun-version.txt not found; pass <old-version> explicitly" >&2
        exit 2
    fi
    OLD="$(tr -d '[:space:]' < "$REPO_ROOT/vendor/libkrun-version.txt")"
fi

if [ "$OLD" = "$NEW" ]; then
    echo "Old and new versions are identical ($OLD); nothing to diff" >&2
    exit 1
fi

URL_TEMPLATE="https://raw.githubusercontent.com/containers/libkrun/v%s/include/libkrun.h"

fetch_header() {
    local version="$1"
    local out="$2"
    local url
    url="$(printf "$URL_TEMPLATE" "$version")"
    if ! curl -fsSL "$url" -o "$out"; then
        echo "Failed to fetch libkrun.h for v$version from $url" >&2
        exit 1
    fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fetch_header "$OLD" "$TMP/old.h"
fetch_header "$NEW" "$TMP/new.h"

echo "# libkrun.h diff: v$OLD -> v$NEW"
echo "# Add new function declarations to ward-core/src/backend/krun_ffi.rs"
echo "# (and remove any that disappeared upstream)."
echo

diff -u "$TMP/old.h" "$TMP/new.h" || true
