#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Rewrite version, url and sha256 in the Homebrew formula for a release.
#
# Usage: scripts/update-formula.sh [--require-all] <formula> <version> <dist-dir> [base-url]
#
# <dist-dir> holds ward-<version>-<target>.tar.gz.sha256 files (the
# `<sha256>  <file>` sidecars the release build writes). Each url line in
# the formula names its target; the sha256 line below it takes that
# target's checksum. Targets without a sidecar are left untouched unless
# --require-all is given, which turns them into an error.
#
# <base-url> defaults to the GitHub release download URL for v<version>;
# CI passes a file:// directory to install a locally built tarball.

set -euo pipefail

readonly TARGETS=(aarch64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu)

die() {
  echo "update-formula: $*" >&2
  exit 1
}

require_all=0
if [[ "${1:-}" == "--require-all" ]]; then
  require_all=1
  shift
fi

[[ $# -ge 3 && $# -le 4 ]] || die "usage: $0 [--require-all] <formula> <version> <dist-dir> [base-url]"
readonly FORMULA=$1 VERSION=$2 DIST=$3
readonly BASE_URL=${4:-https://github.com/igorjs/ward/releases/download/v${VERSION}}

[[ "$BASE_URL" =~ ^[A-Za-z0-9:/._+-]+$ ]] || die "invalid base-url: $BASE_URL"
[[ -f "$FORMULA" ]] || die "formula not found: $FORMULA"
[[ -d "$DIST" ]] || die "dist directory not found: $DIST"
[[ "$VERSION" =~ ^[0-9A-Za-z][0-9A-Za-z.+-]*$ ]] || die "invalid version: $VERSION"

# Build "target=sha" pairs for the awk pass.
pairs=()
for target in "${TARGETS[@]}"; do
  sidecar="$DIST/ward-${VERSION}-${target}.tar.gz.sha256"
  if [[ ! -f "$sidecar" ]]; then
    [[ $require_all -eq 0 ]] || die "missing checksum file: $sidecar"
    continue
  fi
  sha=$(awk 'NR == 1 { print $1 }' "$sidecar")
  [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die "malformed checksum in $sidecar"
  pairs+=("${target}=${sha}")
done
[[ ${#pairs[@]} -gt 0 ]] || die "no checksum files for version $VERSION in $DIST"

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

awk -v version="$VERSION" -v base="$BASE_URL" -v pairs="${pairs[*]}" '
  BEGIN {
    n = split(pairs, list, " ")
    for (i = 1; i <= n; i++) {
      split(list[i], kv, "=")
      sha[kv[1]] = kv[2]
    }
  }
  /^[[:space:]]*version "/ {
    sub(/"[^"]*"/, "\"" version "\"")
  }
  /^[[:space:]]*url "/ {
    current = ""
    for (t in sha) {
      if (index($0, "-" t ".tar.gz\"") > 0) current = t
    }
    if (current != "") {
      sub(/"[^"]*"/, "\"" base "/ward-" version "-" current ".tar.gz\"")
    }
  }
  /^[[:space:]]*sha256 "/ && current != "" {
    sub(/"[^"]*"/, "\"" sha[current] "\"")
    current = ""
  }
  { print }
' "$FORMULA" > "$tmp"

cat "$tmp" > "$FORMULA"
