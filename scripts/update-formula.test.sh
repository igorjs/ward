#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Tests for scripts/update-formula.sh against a scratch copy of the formula.
#
# Usage: scripts/update-formula.test.sh

set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly UPDATE="$SCRIPT_DIR/update-formula.sh"
readonly FORMULA_SRC="$SCRIPT_DIR/../packaging/homebrew/ward.rb"
readonly SHA_MAC=1111111111111111111111111111111111111111111111111111111111111111
readonly SHA_X86=2222222222222222222222222222222222222222222222222222222222222222
readonly SHA_ARM=3333333333333333333333333333333333333333333333333333333333333333

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

check() {
  local name=$1 cond=$2
  if eval "$cond"; then
    echo "ok   $name"
  else
    echo "FAIL $name"
    failures=$((failures + 1))
  fi
}

# fresh_case <name> <targets...>: new formula copy and dist dir holding
# sidecars for the listed targets; prints the case directory.
fresh_case() {
  local dir="$work/$1"
  shift
  mkdir -p "$dir/dist"
  cp "$FORMULA_SRC" "$dir/ward.rb"
  local target sha
  for target in "$@"; do
    case "$target" in
      aarch64-apple-darwin) sha=$SHA_MAC ;;
      x86_64-unknown-linux-gnu) sha=$SHA_X86 ;;
      *) sha=$SHA_ARM ;;
    esac
    echo "$sha  ward-1.2.3-${target}.tar.gz" >"$dir/dist/ward-1.2.3-${target}.tar.gz.sha256"
  done
  echo "$dir"
}

# Given all sidecars, When run, Then version, urls and checksums are rewritten.
dir=$(fresh_case all aarch64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu)
"$UPDATE" --require-all "$dir/ward.rb" 1.2.3 "$dir/dist"
check "version rewritten" "grep -q 'version \"1.2.3\"' '$dir/ward.rb'"
check "mac url rewritten" "grep -q 'download/v1.2.3/ward-1.2.3-aarch64-apple-darwin.tar.gz\"' '$dir/ward.rb'"
check "x86 linux url rewritten" "grep -q 'download/v1.2.3/ward-1.2.3-x86_64-unknown-linux-gnu.tar.gz\"' '$dir/ward.rb'"
check "arm linux url rewritten" "grep -q 'download/v1.2.3/ward-1.2.3-aarch64-unknown-linux-gnu.tar.gz\"' '$dir/ward.rb'"
check "each sha follows its url" "[[ \$(grep -A1 'aarch64-apple-darwin' '$dir/ward.rb' | grep -c $SHA_MAC) -eq 1 && \$(grep -A1 'x86_64-unknown' '$dir/ward.rb' | grep -c $SHA_X86) -eq 1 && \$(grep -A1 'aarch64-unknown' '$dir/ward.rb' | grep -c $SHA_ARM) -eq 1 ]]"
check "formula remains valid ruby" "ruby -c '$dir/ward.rb' >/dev/null 2>&1 || ! command -v ruby >/dev/null"

# Given a second run, Then the result is identical (idempotent).
cp "$dir/ward.rb" "$dir/first.rb"
"$UPDATE" --require-all "$dir/ward.rb" 1.2.3 "$dir/dist"
check "idempotent" "cmp -s '$dir/ward.rb' '$dir/first.rb'"

# Given a base url, Then urls use it.
dir=$(fresh_case base aarch64-apple-darwin)
"$UPDATE" "$dir/ward.rb" 1.2.3 "$dir/dist" "file:///tmp/dist"
check "base url override" "grep -q 'url \"file:///tmp/dist/ward-1.2.3-aarch64-apple-darwin.tar.gz\"' '$dir/ward.rb'"

# Given a partial dist without --require-all, Then other targets stay untouched.
check "other targets untouched" "grep -q 'v0.0.0/ward-0.0.0-x86_64-unknown-linux-gnu' '$dir/ward.rb'"

# Given a partial dist with --require-all, Then it fails.
dir=$(fresh_case strict aarch64-apple-darwin)
check "require-all fails on missing sidecar" "! '$UPDATE' --require-all '$dir/ward.rb' 1.2.3 '$dir/dist' 2>/dev/null"

# Given a malformed checksum, Then it fails.
dir=$(fresh_case bad aarch64-apple-darwin)
echo "nothex  file" >"$dir/dist/ward-1.2.3-aarch64-apple-darwin.tar.gz.sha256"
check "malformed checksum rejected" "! '$UPDATE' '$dir/ward.rb' 1.2.3 '$dir/dist' 2>/dev/null"

# Given a base url with awk/sed special characters, Then it fails.
dir=$(fresh_case badurl aarch64-apple-darwin)
check "unsafe base url rejected" "! '$UPDATE' '$dir/ward.rb' 1.2.3 '$dir/dist' 'file:///tmp/a&b|c' 2>/dev/null"

# Given an empty dist, Then it fails.
dir=$(fresh_case empty)
check "empty dist rejected" "! '$UPDATE' '$dir/ward.rb' 1.2.3 '$dir/dist' 2>/dev/null"

[[ $failures -eq 0 ]] || { echo "$failures failure(s)"; exit 1; }
echo "all passed"
