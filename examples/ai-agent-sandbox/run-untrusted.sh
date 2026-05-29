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
#   1. ward create  spawns a libkrun microVM from the OCI image
#   2. ward exec    starts the command inside it, returns a pid
#   3. ward logs    streams stdout / stderr / exit until the pid ends
#   4. ward remove  tears the microVM down (always; trap on EXIT)
#
# Designed to be the smallest reusable wrapper an AI agent or CI step
# can shell out to. Customise the resource limits, environment, and
# the command surface to taste.
#
# Egress policy: the daemon defaults to Deny when the CreateSandbox
# request omits an egress field. The CLI does not yet expose an
# --egress flag (tracked upstream); to relax egress today, talk to
# the daemon over gRPC directly.

set -euo pipefail

# Args
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

# Config: defaults tuned for "untrusted LLM output" workloads. Override via env.
cpus=${WARD_CPUS:-1}
memory_mb=${WARD_MEMORY_MB:-512}
timeout_s=${WARD_TIMEOUT:-60}

# Helpers
log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v ward >/dev/null 2>&1 || die "ward CLI not found on PATH"
command -v awk  >/dev/null 2>&1 || die "awk not found on PATH"

# 1. Create
log "Creating sandbox from ${image} (cpus=${cpus}, mem=${memory_mb}M, timeout=${timeout_s}s, egress=daemon-default-Deny)"
sb=$(ward create "${image}" \
       --cpus "${cpus}" \
       --memory "${memory_mb}" \
       --timeout "${timeout_s}" \
     | awk '/^id:/{print $2}')

if [[ -z "${sb}" ]]; then
  die "ward create returned no sandbox id"
fi
log "Sandbox: ${sb}"

# Cleanup trap: whatever happens (successful exit, signal, exec failure)
# the sandbox is torn down. ward remove is idempotent and returns 0 if
# the sandbox is already gone (e.g. because the timeout fired before
# we got here).
cleanup() {
  local code=$?
  log "Removing sandbox ${sb}"
  ward remove "${sb}" >/dev/null 2>&1 || true
  exit "${code}"
}
trap cleanup EXIT INT TERM

# 2. Exec: launch the command, capture its pid. `sh -c` is a convenience
# so callers can pass shell strings; the guest is the trust boundary,
# not this script. For argv-style invocation drop the wrapper and pass
# the array directly:  ward exec "$sb" -- prog arg1 arg2
log "Executing: ${command}"
pid=$(ward exec "${sb}" -- sh -c "${command}" | awk '/^pid:/{print $2}')
if [[ -z "${pid}" ]]; then
  die "ward exec returned no pid"
fi

# 3. Logs: stream stdout / stderr / exit lines until the process ends.
# `ward logs` returns the process's exit code on its own line; capture
# it with a structured grep, NOT $? (which would be ward logs's own
# exit code, almost always 0).
log "--- begin sandbox output ---"
log_output=$(ward logs "${sb}" "${pid}")
printf '%s\n' "${log_output}"
log "--- end sandbox output ---"

exit_code=$(printf '%s\n' "${log_output}" | awk '/^exit:/{print $2; exit}')
exit_code=${exit_code:-0}
log "Process exit code: ${exit_code}"

exit "${exit_code}"
