#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Table-driven tests for scripts/check-proto-compat.sh.
#
# Each case commits a base proto into a scratch git repo, applies a sed
# mutation to the working tree and checks the script's verdict.
#
# Usage: scripts/check-proto-compat.test.sh

set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly CHECK="$SCRIPT_DIR/check-proto-compat.sh"
readonly BASE_REV="HEAD"
readonly PASS=0
readonly FAIL=1

readonly BASE_PROTO='syntax = "proto3";
package ward.v1;

// Comments such as "int32 gone = 9;" are not fields.
message A {
  string x = 1;
  int32 y = 2; // trailing comment
  map<string, string> env = 3;
  oneof kind {
    string left = 4;
    int32 right = 5 [deprecated = true];
  }
  message Inner {
    bool flag = 1;
  }
}

message B {
  string x = 1;
}

message Empty {}

enum E {
  E_UNSPECIFIED = 0;
  E_ONE = 1;
  E_TWO = 2;
}

service S {
  rpc Get(A) returns (B);
}'

# name | proto file | expected exit | sed mutation
readonly CASES=(
  "field removed|ward.proto|$FAIL|/int32 y = 2;/d"
  "field renumbered|ward.proto|$FAIL|s/int32 y = 2;/int32 y = 20;/"
  "oneof field removed|ward.proto|$FAIL|/string left = 4;/d"
  "map field renumbered|ward.proto|$FAIL|s/env = 3;/env = 30;/"
  "nested field removed|ward.proto|$FAIL|/bool flag = 1;/d"
  "same name removed from second message|ward.proto|$FAIL|/^message B/,/^}/{/string x = 1;/d;}"
  "field type changed|ward.proto|$FAIL|s/int32 y = 2;/string y = 2;/"
  "field label changed|ward.proto|$FAIL|s/string x = 1;/repeated string x = 1;/"
  "map value type changed|ward.proto|$FAIL|s/map<string, string> env/map<string, int32> env/"
  "oneof member type changed|ward.proto|$FAIL|s/string left = 4;/bytes left = 4;/"
  "field moved into oneof|ward.proto|$FAIL|s/int32 y = 2;.*/oneof moved { int32 y = 2; }/"
  "field moved out of oneof|ward.proto|$FAIL|s/string left = 4;/} string left = 4; oneof rest {/"
  "enum value removed|ward.proto|$FAIL|/E_ONE = 1;/d"
  "enum value renumbered|ward.proto|$FAIL|s/E_TWO = 2;/E_TWO = 3;/"
  "agent field removed|ward_agent.proto|$FAIL|/int32 y = 2;/d"
  "field respaced|ward.proto|$PASS|s/int32 y = 2;/int32   y   =   2;/"
  "map respaced|ward.proto|$PASS|s/map<string, string>/map<string,string>/"
  "field appended|ward.proto|$PASS|s/int32 y = 2;/int32 y = 2; string z = 6;/"
  "enum value appended|ward.proto|$PASS|s/E_TWO = 2;/E_TWO = 2; E_THREE = 3;/"
  "message appended|ward.proto|$PASS|s/^message Empty {}/message Empty {} message C { string q = 1; }/"
  "unchanged|ward.proto|$PASS|s/^syntax/syntax/"
)

root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT

failures=0
index=0
for entry in "${CASES[@]}"; do
  IFS='|' read -r name file expected mutation <<<"$entry"
  index=$((index + 1))

  # Arrange: base committed, mutated copy in the working tree.
  work="$root/case-$index"
  mkdir -p "$work/proto"
  printf '%s\n' "$BASE_PROTO" >"$work/proto/ward.proto"
  printf '%s\n' "$BASE_PROTO" >"$work/proto/ward_agent.proto"
  git -C "$work" init -q
  git -C "$work" add proto
  git -C "$work" -c user.name=test -c user.email=test@example.com \
    -c commit.gpgsign=false commit -q -m base
  sed -e "$mutation" "$work/proto/$file" >"$work/proto/$file.new"
  mv "$work/proto/$file.new" "$work/proto/$file"

  # Act
  actual=$PASS
  (cd "$work" && bash "$CHECK" "$BASE_REV") >/dev/null 2>&1 || actual=$?

  # Assert
  if [ "$actual" -eq "$expected" ]; then
    echo "ok   - $name"
  else
    echo "FAIL - $name (expected exit class $expected, got $actual)"
    failures=$((failures + 1))
  fi
done

if [ "$failures" -ne 0 ]; then
  echo "$failures case(s) failed"
  exit 1
fi
echo "all ${#CASES[@]} cases passed"
