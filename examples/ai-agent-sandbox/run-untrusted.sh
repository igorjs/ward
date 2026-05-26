#!/usr/bin/env bash
#
# Run an untrusted command in a fresh ward sandbox, then clean up.
#
# Usage:
#     ./run-untrusted.sh <image> <command...>
#
# Examples:
#     ./run-untrusted.sh alpine:latest 'echo hello; uname -a'
#     ./run-untrusted.sh python:3.12-slim 'python -c "print(2+2)"'
#
# What this does:
#   1. ward create  — spawns a libkrun microVM from the OCI image
#   2. ward exec    — runs the command inside it, streams output
#   3. ward remove  — tears the microVM down (always; trap on EXIT)
#
# Designed to be the smallest reusable wrapper an AI agent or CI step
# can shell out to. Customise the resource limits, egress policy, and
# environment to taste.

set -euo pipefail

# ─── Args ────────────────────────────────────────────────────────────
if [[ $# -lt 2 ]]; then
  cat <<EOF >&2
Usage: $0 <image> <command...>
Example: $0 alpine:latest 'echo hello; uname -a'
EOF
  exit 1
fi
image=$1
shift
command=$*

# ─── Config ──────────────────────────────────────────────────────────
# Defaults tuned for "untrusted LLM output" workloads. Override via env.
egress_mode=${WARD_EGRESS_MODE:-Deny}
cpus=${WARD_CPUS:-1}
memory_mb=${WARD_MEMORY_MB:-512}
timeout_s=${WARD_TIMEOUT:-60}

# ─── Helpers ─────────────────────────────────────────────────────────
log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v ward >/dev/null 2>&1 || die "ward CLI not found on PATH"

# ─── 1. Create ───────────────────────────────────────────────────────
log "Creating sandbox from ${image} (egress=${egress_mode}, cpus=${cpus}, mem=${memory_mb}M, timeout=${timeout_s}s)"
sb=$(ward create "${image}" \
       --egress "${egress_mode}" \
       --cpus "${cpus}" \
       --memory-mb "${memory_mb}" \
       --timeout "${timeout_s}" \
       --json | jq -r .id)

if [[ -z "${sb}" || "${sb}" == "null" ]]; then
  die "ward create returned no sandbox id"
fi
log "Sandbox: ${sb}"

# ─── Cleanup trap ────────────────────────────────────────────────────
# Whatever happens after this point — successful exit, signal, exec
# failure — the sandbox is torn down. ward remove is idempotent and
# returns 0 if the sandbox is already gone (e.g. because the timeout
# fired before we got here).
cleanup() {
  local code=$?
  log "Removing sandbox ${sb}"
  ward remove "${sb}" >/dev/null 2>&1 || true
  exit "${code}"
}
trap cleanup EXIT INT TERM

# ─── 2. Exec ─────────────────────────────────────────────────────────
log "Executing: ${command}"
log "--- begin sandbox output ---"
# `sh -c` so the caller can pass a shell string; for argv-style
# invocation drop the wrapper and pass the array directly.
ward exec "${sb}" -- sh -c "${command}"
exit_code=$?
log "--- end sandbox output (exit=${exit_code}) ---"

exit "${exit_code}"
