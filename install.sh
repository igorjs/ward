#!/usr/bin/env bash
#
# ward installer.
#
# Detects the host platform, downloads the matching pre-built archive
# from the ward GitHub Releases, verifies its SHA-256, and installs the
# `ward` and `wardd` binaries under $WARD_INSTALL_DIR (default ~/.ward).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/igorjs/ward/main/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/igorjs/ward/main/install.sh | WARD_VERSION=v0.2.0 bash
#   curl -fsSL https://raw.githubusercontent.com/igorjs/ward/main/install.sh | WARD_INSTALL_DIR=/usr/local bash
#
# Environment:
#   WARD_VERSION      Release tag to install. Defaults to the latest stable
#                     release (whatever `latest` resolves to on GitHub).
#   WARD_INSTALL_DIR  Prefix to install into. Binaries go in
#                     $WARD_INSTALL_DIR/bin/; dylibs in $WARD_INSTALL_DIR/lib/.
#                     Default: $HOME/.ward
#   WARD_NO_MODIFY_PATH
#                     If set to any non-empty value, skip modifying shell rc
#                     files; print PATH instructions instead.
#
# Exit codes:
#   0   Success.
#   1   Unsupported platform.
#   2   Required tool missing (curl, tar, sha256sum/shasum).
#   3   Download failed.
#   4   SHA-256 verification failed.
#   5   Extraction failed.
#   6   GitHub API call to resolve `latest` failed.

set -euo pipefail

# SEC-013: clamp umask before any mkdir / cp so the install dir and
# every file inside it is born owner-only, regardless of the
# operator's shell umask. Personal-daemon install paths have no
# group-share use case; world/group read on a wardd binary lets
# co-located users overwrite it between install and next invocation
# (race-to-replace primitive on shared dev hosts).
umask 077

WARD_REPO="${WARD_REPO:-igorjs/ward}"
WARD_VERSION="${WARD_VERSION:-latest}"
WARD_INSTALL_DIR="${WARD_INSTALL_DIR:-${HOME}/.ward}"
TMP_DIR=""

# Use ANSI colours when stdout is a TTY. Piping curl to bash typically
# leaves stdout connected to the terminal, so colours work.
if [[ -t 1 ]]; then
  C_DIM="$(printf '\033[2m')"
  C_BOLD="$(printf '\033[1m')"
  C_GREEN="$(printf '\033[32m')"
  C_RED="$(printf '\033[31m')"
  C_RESET="$(printf '\033[0m')"
else
  C_DIM=""; C_BOLD=""; C_GREEN=""; C_RED=""; C_RESET=""
fi

log()  { printf "%s==>%s %s\n"   "$C_BOLD"  "$C_RESET" "$*"; }
warn() { printf "%swarn:%s %s\n" "$C_BOLD"  "$C_RESET" "$*" >&2; }
err()  { printf "%serror:%s %s\n" "$C_RED"  "$C_RESET" "$*" >&2; }
ok()   { printf "%s✓%s %s\n"     "$C_GREEN" "$C_RESET" "$*"; }

cleanup() {
  if [[ -n "$TMP_DIR" && -d "$TMP_DIR" ]]; then
    rm -rf -- "$TMP_DIR"
  fi
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Tooling sanity check
# ---------------------------------------------------------------------------

need() {
  command -v "$1" > /dev/null 2>&1 || {
    err "missing required tool: $1"
    exit 2
  }
}
need curl
need tar
# sha256: macOS uses `shasum -a 256`, Linux uses `sha256sum`. We'll detect.

if command -v sha256sum > /dev/null 2>&1; then
  SHA256_CMD="sha256sum"
elif command -v shasum > /dev/null 2>&1; then
  SHA256_CMD="shasum -a 256"
else
  err "missing required tool: sha256sum or shasum"
  exit 2
fi

# ---------------------------------------------------------------------------
# Platform detection
# ---------------------------------------------------------------------------

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin)
      case "$arch" in
        arm64 | aarch64) echo "aarch64-apple-darwin" ;;
        *)
          err "unsupported macOS architecture: $arch (ward currently supports Apple Silicon only)"
          exit 1
          ;;
      esac
      ;;
    Linux)
      case "$arch" in
        x86_64) echo "x86_64-unknown-linux-gnu" ;;
        aarch64 | arm64) echo "aarch64-unknown-linux-gnu" ;;
        *)
          err "unsupported Linux architecture: $arch"
          exit 1
          ;;
      esac
      ;;
    *)
      err "unsupported operating system: $os"
      exit 1
      ;;
  esac
}

TARGET="$(detect_target)"
log "Detected target: ${C_BOLD}${TARGET}${C_RESET}"

# ---------------------------------------------------------------------------
# Resolve version
# ---------------------------------------------------------------------------

resolve_version() {
  if [[ "$WARD_VERSION" != "latest" ]]; then
    echo "$WARD_VERSION"
    return
  fi
  # Public API call; no auth required for releases.
  local url="https://api.github.com/repos/${WARD_REPO}/releases/latest"
  local tag
  tag="$(curl -fsSL "$url" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
  if [[ -z "$tag" ]]; then
    err "could not resolve latest release tag from $url"
    exit 6
  fi
  echo "$tag"
}

TAG="$(resolve_version)"
VERSION="${TAG#v}"
log "Installing ward ${C_BOLD}${VERSION}${C_RESET} (tag ${TAG})"

# ---------------------------------------------------------------------------
# Download + verify
# ---------------------------------------------------------------------------

TMP_DIR="$(mktemp -d)"
ARCHIVE="ward-${VERSION}-${TARGET}.tar.gz"
ARCHIVE_URL="https://github.com/${WARD_REPO}/releases/download/${TAG}/${ARCHIVE}"
SHA_URL="${ARCHIVE_URL}.sha256"

log "Downloading $ARCHIVE"
if ! curl --fail --silent --show-error --location \
        --output "${TMP_DIR}/${ARCHIVE}" "$ARCHIVE_URL"; then
  err "failed to download $ARCHIVE_URL"
  exit 3
fi
if ! curl --fail --silent --show-error --location \
        --output "${TMP_DIR}/${ARCHIVE}.sha256" "$SHA_URL"; then
  err "failed to download $SHA_URL"
  exit 3
fi

log "Verifying SHA-256"
EXPECTED_SHA="$(awk '{print $1}' "${TMP_DIR}/${ARCHIVE}.sha256")"
ACTUAL_SHA="$(${SHA256_CMD} "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')"
if [[ "$EXPECTED_SHA" != "$ACTUAL_SHA" ]]; then
  err "SHA-256 mismatch — refusing to install"
  err "expected: $EXPECTED_SHA"
  err "got:      $ACTUAL_SHA"
  exit 4
fi
ok "Checksum verified"

# ---------------------------------------------------------------------------
# SLSA Build L3 provenance verification
# ---------------------------------------------------------------------------
#
# Sigstore-backed in-toto provenance generated by ward's release.yml
# via slsa-framework/slsa-github-generator's L3 reusable workflow.
# Proves the tarball was built by an isolated GitHub-hosted runner
# under the named workflow + tag, with the build environment described
# by the provenance unchangeable by the workflow author. Defeats
# compromised-release-token attacks that the same-Release SHA-256
# sidecar cannot: a leaked release-publish token would let an attacker
# upload a matching .sha256 alongside a malicious tarball, but the
# attacker cannot forge a sigstore attestation chained to a Rekor
# transparency log entry AND naming the trusted SLSA generator
# workflow as the builder.
#
# Requires `slsa-verifier` (https://github.com/slsa-framework/slsa-verifier).
# Install via:
#   brew install slsa-verifier            (macOS / Linux)
#   go install github.com/slsa-framework/slsa-verifier/v2/cli/slsa-verifier@latest
#   or download a release binary from the project's GitHub Releases.
# If unavailable, we WARN and continue with SHA-256-only verification;
# users who want the strongest integrity check should install
# slsa-verifier and re-run.

if command -v slsa-verifier > /dev/null 2>&1; then
  PROVENANCE="${TMP_DIR}/multiple.intoto.jsonl"
  log "Downloading SLSA L3 provenance"
  if ! curl -fsSL -o "${PROVENANCE}" \
         "https://github.com/${WARD_REPO}/releases/download/v${VERSION}/multiple.intoto.jsonl"; then
    err "Failed to download multiple.intoto.jsonl from release v${VERSION}"
    exit 4
  fi
  log "Verifying SLSA L3 build provenance"
  if slsa-verifier verify-artifact \
       --provenance-path "${PROVENANCE}" \
       --source-uri "github.com/${WARD_REPO}" \
       "${TMP_DIR}/${ARCHIVE}" > /dev/null 2>&1; then
    ok "SLSA L3 provenance verified (built by ${WARD_REPO}'s release.yml under slsa-github-generator)"
  else
    err "SLSA verification FAILED, refusing to install"
    err "Run for full output:"
    err "  slsa-verifier verify-artifact --provenance-path ${PROVENANCE} --source-uri github.com/${WARD_REPO} ${TMP_DIR}/${ARCHIVE}"
    exit 4
  fi
else
  warn "slsa-verifier not found, skipping SLSA L3 provenance verification."
  warn "SHA-256 protects against transport tampering only; for full"
  warn "supply-chain integrity, install slsa-verifier"
  warn "(brew install slsa-verifier) and re-run install."
fi

# ---------------------------------------------------------------------------
# Extract + install
# ---------------------------------------------------------------------------

log "Extracting"
if ! tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "${TMP_DIR}"; then
  err "tar extraction failed"
  exit 5
fi

EXTRACTED_DIR="${TMP_DIR}/ward-${VERSION}-${TARGET}"
if [[ ! -d "$EXTRACTED_DIR" ]]; then
  err "unexpected archive layout — no ${EXTRACTED_DIR} after extract"
  exit 5
fi

log "Installing to ${C_BOLD}${WARD_INSTALL_DIR}${C_RESET}"
mkdir -p "${WARD_INSTALL_DIR}/bin" "${WARD_INSTALL_DIR}/lib"
# SEC-013 follow-through: ensure the prefix itself is 0700, not just
# umask-derived. `mkdir -p` honours the current umask (077 above) so
# new dirs land at 0700, but an existing prefix from an older install
# may still be world-listable. Force it explicitly.
chmod 0700 "${WARD_INSTALL_DIR}"
# Copy with -p to preserve mtimes and exec bits; -R for nested directories.
cp -pR "${EXTRACTED_DIR}/bin/." "${WARD_INSTALL_DIR}/bin/"
if [[ -d "${EXTRACTED_DIR}/lib" ]] && [[ -n "$(ls -A "${EXTRACTED_DIR}/lib" 2>/dev/null || true)" ]]; then
  cp -pR "${EXTRACTED_DIR}/lib/." "${WARD_INSTALL_DIR}/lib/"
fi
# Documentation is best-effort.
for f in LICENSE README.md; do
  [[ -f "${EXTRACTED_DIR}/${f}" ]] && cp "${EXTRACTED_DIR}/${f}" "${WARD_INSTALL_DIR}/" || true
done

# ---------------------------------------------------------------------------
# Smoke test + PATH hint
# ---------------------------------------------------------------------------

log "Smoke test"
if "${WARD_INSTALL_DIR}/bin/ward" --version > /dev/null 2>&1; then
  ok "ward installed: $(${WARD_INSTALL_DIR}/bin/ward --version 2>&1 | head -n1)"
else
  warn "the installed ward binary did not respond to --version. Continuing,"
  warn "but you may need to investigate (rpath issues are the usual cause)."
fi

echo
ok "Installed ward ${VERSION} to ${WARD_INSTALL_DIR}"
echo

# ---------------------------------------------------------------------------
# Shell rc modification
# ---------------------------------------------------------------------------
#
# Append the export line to the user's shell rc file so future shells
# pick up ward on PATH. Idempotent: skipped entirely if the install dir
# is already on PATH, or if the rc file already references it.
#
# Hard limit of pipe-to-bash installers: we cannot modify the parent
# shell's environment, so the user still needs to `source` their rc
# file or open a new terminal for `ward` to resolve in this session.
# We print the source command to make that obvious.

print_path_hint() {
  echo "${C_DIM}# Add ward to your shell PATH:${C_RESET}"
  echo "  export PATH=\"${WARD_INSTALL_DIR}/bin:\$PATH\""
  echo
  echo "${C_DIM}# Permanently (bash):${C_RESET}"
  echo "  echo 'export PATH=\"${WARD_INSTALL_DIR}/bin:\$PATH\"' >> ~/.bashrc"
  echo "${C_DIM}# Permanently (zsh):${C_RESET}"
  echo "  echo 'export PATH=\"${WARD_INSTALL_DIR}/bin:\$PATH\"' >> ~/.zshrc"
  echo
}

modify_path() {
  local install_bin="${WARD_INSTALL_DIR}/bin"

  # Already on PATH? Nothing to do.
  case ":$PATH:" in
    *":${install_bin}:"*)
      return 0
      ;;
  esac

  local shell_name
  shell_name="$(basename "${SHELL:-/bin/sh}")"

  local rc_file="" line_to_add=""
  case "$shell_name" in
    zsh)
      rc_file="${HOME}/.zshrc"
      line_to_add="export PATH=\"${install_bin}:\$PATH\""
      ;;
    bash)
      # macOS login shells source .bash_profile, not .bashrc; honour
      # whichever the user already has.
      if [[ "$(uname -s)" == "Darwin" && -f "${HOME}/.bash_profile" ]]; then
        rc_file="${HOME}/.bash_profile"
      else
        rc_file="${HOME}/.bashrc"
      fi
      line_to_add="export PATH=\"${install_bin}:\$PATH\""
      ;;
    fish)
      rc_file="${HOME}/.config/fish/config.fish"
      line_to_add="set -gx PATH ${install_bin} \$PATH"
      mkdir -p "$(dirname "$rc_file")"
      ;;
    *)
      warn "unrecognised shell '$shell_name' — skipping rc modification"
      print_path_hint
      return 0
      ;;
  esac

  # Idempotent: skip if the install dir is already referenced.
  if [[ -f "$rc_file" ]] && grep -Fq "$install_bin" "$rc_file"; then
    ok "PATH already configured in $rc_file"
    return 0
  fi

  # Append. touch first so the redirect doesn't fail on missing parent
  # for shells like fish where we just created the dir above.
  if ! touch "$rc_file" 2> /dev/null; then
    warn "could not write to $rc_file — falling back to hint"
    print_path_hint
    return 0
  fi
  {
    echo ""
    echo "# Added by ward installer ($(uname -s) $(date -u +%Y-%m-%d))"
    echo "$line_to_add"
  } >> "$rc_file"

  ok "Updated PATH in $rc_file"
  echo
  echo "${C_DIM}# To use ward in this shell session, run:${C_RESET}"
  case "$shell_name" in
    fish)
      echo "  source $rc_file"
      ;;
    *)
      echo "  . $rc_file"
      ;;
  esac
  echo "${C_DIM}# Or open a new terminal.${C_RESET}"
  echo
}

if [[ -z "${WARD_NO_MODIFY_PATH:-}" ]]; then
  modify_path
else
  case ":$PATH:" in
    *":${WARD_INSTALL_DIR}/bin:"*) ;;
    *) print_path_hint ;;
  esac
fi

echo "${C_DIM}# Start the daemon:${C_RESET}"
echo "  wardd &"
echo
echo "${C_DIM}# Then use the CLI:${C_RESET}"
echo "  ward info"
echo "  ward health"
echo "  ward create alpine"
echo

# ---------------------------------------------------------------------------
# Rootless prerequisites hint (post-install, non-fatal)
# ---------------------------------------------------------------------------
#
# ward runs unprivileged — see docs/rootless.md — but each platform
# has a one-time setup step the user has to do themselves (we won't
# sudo on their behalf). Print a per-platform pointer so they don't
# discover this on their first `ward create`.

case "$(uname -s)" in
  Linux)
    if [[ -e /dev/kvm ]] && ! [[ -r /dev/kvm && -w /dev/kvm ]]; then
      echo "${C_DIM}# Linux rootless setup:${C_RESET}"
      echo "  You're not yet in the 'kvm' group. Run:"
      echo "    sudo usermod -aG kvm \$USER"
      echo "  then log out and back in. ward will start failing at"
      echo "  sandbox creation until this is done."
      echo "  Optional for network egress: sudo apt install passt"
      echo "  (or your distro equivalent). See docs/rootless.md."
      echo
    fi
    ;;
  Darwin)
    # The Hypervisor entitlement is applied at release-build time by
    # ward's release.yml. Local builds from source require manual
    # codesigning — only relevant if the user built from source
    # rather than running this script. Mention it lightly.
    echo "${C_DIM}# macOS rootless setup:${C_RESET}"
    echo "  The installed wardd is signed with the Hypervisor"
    echo "  entitlement — no further setup needed. If you instead"
    echo "  built wardd from source, see docs/rootless.md for the"
    echo "  one-time codesign step."
    echo
    ;;
esac
