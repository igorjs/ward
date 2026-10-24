#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Fail when a public proto drops or renumbers a field or an enum value, or
# changes a field's type, label or oneof membership, compared with a base
# revision (ADR-004, ADR-022). Appending is allowed.
#
# Usage:
#   scripts/check-proto-compat.sh [<base-rev>]
#
# <base-rev> defaults to origin/main. Needs only bash, git and awk.

set -euo pipefail

readonly DEFAULT_BASE="origin/main"
readonly PROTO_FILES=(proto/ward.proto proto/ward_agent.proto)
readonly EXIT_INCOMPATIBLE=1
readonly EXIT_USAGE=2

base="${1:-$DEFAULT_BASE}"

if ! git rev-parse --verify --quiet "$base^{commit}" >/dev/null; then
  echo "error: base revision '$base' does not exist; fetch it first" >&2
  exit "$EXIT_USAGE"
fi

root="$(git rev-parse --show-toplevel)"
cd "$root"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Prints one "<scope>\t<name>\t<number>\t<shape>\t<oneof>" line per field and
# enum value. Scope is the dotted message or enum path; oneof groups share
# their message's scope. Shape is the label and type as written, with
# spacing normalised; it is empty for enum values. Oneof is the enclosing
# oneof group's name, empty outside one.
extract() {
  awk '
    BEGIN { depth = 0; in_block = 0 }
    {
      line = $0
      if (in_block) {
        if (!sub(/^.*\*\//, "", line)) next
        in_block = 0
      }
      while (match(line, /\/\*/)) {
        head = substr(line, 1, RSTART - 1)
        tail = substr(line, RSTART + 2)
        if (sub(/^.*\*\//, "", tail)) { line = head tail }
        else { line = head; in_block = 1 }
      }
      sub(/\/\/.*/, "", line)

      rest = line
      if (match(line, /^[ \t]*(message|enum|oneof|service)[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*\{/)) {
        header = substr(line, RSTART, RLENGTH)
        rest = substr(line, RSTART + RLENGTH)
        kind = header
        sub(/^[ \t]*/, "", kind)
        name = kind
        sub(/[ \t].*$/, "", kind)
        sub(/^[a-z]+[ \t]+/, "", name)
        sub(/[ \t]*\{$/, "", name)
        parent = (depth > 0) ? scope[depth] : ""
        depth++
        if (kind == "message" || kind == "enum")
          scope[depth] = (parent == "") ? name : parent "." name
        else
          scope[depth] = parent
        group[depth] = (kind == "oneof") ? name : ""
      }

      if (rest !~ /^[ \t]*(reserved|option|rpc|syntax|package|import|extensions)[ \t]/ &&
          match(rest, /[A-Za-z_][A-Za-z0-9_]*[ \t]*=[ \t]*[0-9]+/)) {
        decl = substr(rest, RSTART, RLENGTH)
        shape = substr(rest, 1, RSTART - 1)
        gsub(/[ \t]*,[ \t]*/, ",", shape)
        gsub(/[ \t]+/, " ", shape)
        gsub(/^ | $/, "", shape)
        field = decl
        sub(/[ \t]*=.*$/, "", field)
        number = decl
        sub(/^.*=[ \t]*/, "", number)
        printf "%s\t%s\t%s\t%s\t%s\n", scope[depth], field, number, shape, group[depth]
      }

      closes = gsub(/\}/, "}", rest)
      depth -= closes
      if (depth < 0) depth = 0
    }
  '
}

# Reads the new and old listings and reports what the old one had that the
# new one lost, renumbered, retyped or moved between oneof groups.
compare() {
  awk -F'\t' -v file="$1" '
    FILENAME == ARGV[1] { current[$1 FS $2] = $3; shape[$1 FS $2] = $4; group[$1 FS $2] = $5; next }
    {
      key = $1 FS $2
      if (!(key in current))
        printf "%s: %s.%s = %s was removed\n", file, $1, $2, $3
      else if (current[key] != $3)
        printf "%s: %s.%s changed number %s -> %s\n", file, $1, $2, $3, current[key]
      else if (shape[key] != $4)
        printf "%s: %s.%s changed type or label \"%s\" -> \"%s\"\n", file, $1, $2, $4, shape[key]
      else if (group[key] != $5)
        printf "%s: %s.%s changed oneof \"%s\" -> \"%s\"\n", file, $1, $2, $5, group[key]
    }
  ' "$2" "$3"
}

status=0
for file in "${PROTO_FILES[@]}"; do
  if ! git cat-file -e "$base:$file" 2>/dev/null; then
    echo "skip: $file does not exist at $base"
    continue
  fi
  git show "$base:$file" | extract >"$tmp/old.tsv"
  extract <"$file" >"$tmp/new.tsv"
  problems="$(compare "$file" "$tmp/new.tsv" "$tmp/old.tsv")"
  if [ -n "$problems" ]; then
    echo "$problems" >&2
    status=$EXIT_INCOMPATIBLE
  fi
done

if [ "$status" -ne 0 ]; then
  echo "error: wire-incompatible proto change against $base (see ADR-004)" >&2
  exit "$status"
fi
echo "proto compatibility OK against $base"
