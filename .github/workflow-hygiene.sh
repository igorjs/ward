#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Workflow-hygiene checks. Enforces the supply-chain invariants from
# the security/hardening-pass PR. Run by .github/workflows/workflow-
# hygiene.yml; also runnable locally:
#
#   bash .github/workflow-hygiene.sh
#
# Emits GitHub Actions ::error:: annotations so violations surface
# directly on the PR's Files Changed view.

set -euo pipefail

errors=0

emit_error() {
  local file=$1 msg=$2
  echo "::error file=$file::$msg"
  errors=$((errors + 1))
}

# -----------------------------------------------------------------------------
# Check 1: every workflow job has a harden-runner step
# -----------------------------------------------------------------------------
# Counted heuristically: number of `steps:` lines (one per job) must
# equal number of step-security/harden-runner uses. workflow-hygiene
# itself is exempt because the harden-runner step appears in its own
# steps block (the count is then expected: 1 + 1).

for f in .github/workflows/*.yml; do
  steps_count=$(grep -cE '^[[:space:]]+steps:[[:space:]]*$' "$f" || true)
  # Match only the actual `- uses: step-security/harden-runner` line,
  # not the string in comments / descriptions / doc blocks.
  harden_count=$(grep -cE '^[[:space:]]+- uses: step-security/harden-runner' "$f" || true)
  if [ "$steps_count" -ne "$harden_count" ]; then
    emit_error "$f" \
      "expected step-security/harden-runner in every job (steps=$steps_count, harden-runner=$harden_count)"
  fi
done

# -----------------------------------------------------------------------------
# Check 2: no mutable major-version tags for third-party actions
# -----------------------------------------------------------------------------
# Documented exceptions only; every other `uses:` line must reference a
# 40-char commit SHA with a trailing `# v<N>` comment.
#
# Exception 1: dtolnay/rust-toolchain ships rolling tags by design
# (per their docs at https://github.com/dtolnay/rust-toolchain).
#
# Exception 2: slsa-framework/slsa-github-generator REQUIRES tag-form
# refs and actively rejects commit SHAs at runtime with
# "Invalid ref: <sha>. Expected ref of the form refs/tags/vX.Y.Z"
# (exit 2 in `generate-builder.sh`). The tag value gets recorded in
# the signed provenance as the builder identity, so the project's
# threat model treats SHA pinning as a downgrade rather than an
# improvement: an attacker who controlled a specific SHA could mint
# provenance that looks like a different version. See the SLSA
# generator README's "Why we do not recommend pinning to a SHA"
# section.

while IFS= read -r line; do
  # Skip dtolnay's documented rolling tags.
  if [[ "$line" == *"dtolnay/rust-toolchain@"* ]]; then continue; fi
  # Skip SLSA generator: tag-pinned by upstream design (see comment above).
  if [[ "$line" == *"slsa-framework/slsa-github-generator/"* ]]; then continue; fi
  # Skip relative path references like `uses: ./.github/actions/foo`.
  if [[ "$line" == *"uses: ./"* ]]; then continue; fi
  emit_error ".github/workflows" "mutable tag ref (must pin to commit SHA): $line"
done < <(grep -hE 'uses: [^/]+/[^@]+@v[0-9]+([[:space:]]|$)' .github/workflows/*.yml 2>/dev/null || true)

# -----------------------------------------------------------------------------
# Check 3: every SHA pin carries a trailing `# v<N>` comment
# -----------------------------------------------------------------------------
# The comment is what makes `git diff` show "actions/checkout v6 -> v7"
# instead of an opaque 40-char SHA churn. Without it, bumps lose their
# human-readable changelog.

while IFS= read -r line; do
  if [[ "$line" == *"# v"* ]]; then continue; fi
  if [[ "$line" == *"dtolnay/rust-toolchain@"* ]]; then continue; fi
  if [[ "$line" == *"uses: ./"* ]]; then continue; fi
  emit_error ".github/workflows" "SHA pin missing trailing '# v<N>' comment: $line"
done < <(grep -hE 'uses: [^/]+/[^@]+@[0-9a-f]{40}' .github/workflows/*.yml 2>/dev/null || true)

# -----------------------------------------------------------------------------
# Report
# -----------------------------------------------------------------------------

if [ "$errors" -gt 0 ]; then
  echo ""
  echo "FAILED: $errors workflow-hygiene check(s) failed."
  echo "See https://github.com/igorjs/ward/blob/main/.github/workflow-hygiene.sh"
  echo "for the exact rules each line enforces."
  exit 1
fi

echo "All workflow-hygiene checks passed."
