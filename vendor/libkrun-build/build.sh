#!/usr/bin/env bash
#
# Build relocatable libkrun + libkrunfw artefacts for the host platform.
#
# Output: ./dist/libkrun-${VERSION}-${TARGET}.tar.gz
#
# The tarball layout:
#   lib/libkrun.${ext}              (with @rpath / $ORIGIN install name)
#   lib/libkrunfw.${ext}            (same)
#   include/libkrun.h
#   lib/pkgconfig/libkrun.pc        (synthesised, points at the tarball
#                                    layout so consumers can use pkg-config)
#
# This script is called by .github/workflows/vendor-libkrun.yml on a matrix
# of build hosts. It can also be run locally for debugging.
#
# Usage:
#   ./build.sh                    Build for the host's native triple.
#   TARGET=foo ./build.sh         Override the auto-detected target triple.
#
# Required tools:
#   - bash, make, gcc/clang
#   - cargo (Rust 1.75+)
#   - patchelf (Linux) or install_name_tool (macOS, ships with Xcode CLT)
#   - git, curl, tar, gzip, sha256sum / shasum
#
# Exit codes:
#   0   Success — ./dist/libkrun-${VERSION}-${TARGET}.tar.gz exists.
#   1   Unknown / unsupported target triple.
#   2   Dependency missing (one of the tools above).
#   3   Upstream clone or build failed.
#   4   Relocation failed (install_name_tool or patchelf returned non-zero).

set -euo pipefail

# ---------------------------------------------------------------------------
# Resolve VERSION + TARGET
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSION="$(tr -d '[:space:]' < "${SCRIPT_DIR}/version.txt")"

if [[ -z "${TARGET:-}" ]]; then
  # Auto-detect from rustc. Avoids hard-coding platform detection logic;
  # rustc already knows every supported triple by name.
  TARGET="$(rustc -vV | awk '/^host:/ {print $2}')"
fi

case "${TARGET}" in
  aarch64-apple-darwin)
    DYLIB_EXT="dylib"
    BACKEND="hvf"
    ;;
  x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu)
    DYLIB_EXT="so"
    BACKEND="kvm"
    ;;
  *)
    echo "error: unsupported target triple '${TARGET}'" >&2
    echo "supported: aarch64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu" >&2
    exit 1
    ;;
esac

echo "==> Building libkrun ${VERSION} for ${TARGET} (backend: ${BACKEND})"

# ---------------------------------------------------------------------------
# Sanity-check tooling
# ---------------------------------------------------------------------------

need() {
  command -v "$1" > /dev/null 2>&1 || { echo "error: missing required tool '$1'" >&2; exit 2; }
}

need bash
need make
need cargo
need git
need curl
need tar
need gzip
need sha256sum 2>/dev/null || need shasum

if [[ "${TARGET}" == *darwin* ]]; then
  need install_name_tool
else
  need patchelf
fi

# ---------------------------------------------------------------------------
# Work in a scratch directory under ./build/
# ---------------------------------------------------------------------------

WORK="${SCRIPT_DIR}/build/${TARGET}"
DIST="${SCRIPT_DIR}/dist"
STAGE="${WORK}/stage"

rm -rf "${WORK}" "${STAGE}"
mkdir -p "${WORK}" "${STAGE}/lib/pkgconfig" "${STAGE}/include" "${DIST}"

# ---------------------------------------------------------------------------
# Build libkrunfw first — libkrun links against it.
# ---------------------------------------------------------------------------

LIBKRUNFW_VERSION="${LIBKRUNFW_VERSION:-${VERSION}}"
echo "==> Cloning libkrunfw ${LIBKRUNFW_VERSION}"
git clone --depth 1 --branch "v${LIBKRUNFW_VERSION}" \
  https://github.com/containers/libkrunfw.git "${WORK}/libkrunfw" \
  || { echo "error: failed to clone libkrunfw" >&2; exit 3; }

(
  cd "${WORK}/libkrunfw"
  echo "==> Building libkrunfw"
  make -j"$(getconf _NPROCESSORS_ONLN)" || { echo "error: libkrunfw build failed" >&2; exit 3; }
)

# Stage libkrunfw.
cp "${WORK}/libkrunfw/libkrunfw.${DYLIB_EXT}"* "${STAGE}/lib/" 2>/dev/null || true

# ---------------------------------------------------------------------------
# Build libkrun, linking against the staged libkrunfw.
# ---------------------------------------------------------------------------

echo "==> Cloning libkrun ${VERSION}"
git clone --depth 1 --branch "v${VERSION}" \
  https://github.com/containers/libkrun.git "${WORK}/libkrun" \
  || { echo "error: failed to clone libkrun" >&2; exit 3; }

(
  cd "${WORK}/libkrun"
  echo "==> Building libkrun"
  # libkrun's Makefile picks up LIBRARY_PATH / PKG_CONFIG_PATH from the env.
  export LIBRARY_PATH="${STAGE}/lib:${LIBRARY_PATH:-}"
  make -j"$(getconf _NPROCESSORS_ONLN)" || { echo "error: libkrun build failed" >&2; exit 3; }
)

# Stage libkrun + headers.
cp "${WORK}/libkrun/target/release/libkrun.${DYLIB_EXT}"* "${STAGE}/lib/" 2>/dev/null || true
cp "${WORK}/libkrun/include/libkrun.h" "${STAGE}/include/"

# ---------------------------------------------------------------------------
# Relocate install names so the dylibs are portable.
#
# Without this step, the dylibs reference their build-time paths (e.g.
# /tmp/build/libkrun.dylib), and they only work when extracted to the
# exact same location on the consumer machine. After rewriting, they
# reference @rpath/libkrun.dylib (macOS) or $ORIGIN/libkrun.so (Linux),
# which lets the loader find them relative to the executable.
# ---------------------------------------------------------------------------

echo "==> Rewriting install names for relocatability"
if [[ "${TARGET}" == *darwin* ]]; then
  # Resolve the realpath to the actual versioned dylib if the unversioned
  # name is a symlink (which it usually is on macOS Homebrew builds).
  for lib in libkrun libkrunfw; do
    file="${STAGE}/lib/${lib}.${DYLIB_EXT}"
    [[ -L "$file" ]] && file="$(readlink "$file")" && file="${STAGE}/lib/${file}"
    install_name_tool -id "@rpath/${lib}.${DYLIB_EXT}" "$file" || exit 4
  done
  # libkrun loads libkrunfw — rewrite its LC_LOAD_DYLIB entry to @rpath too.
  install_name_tool -change \
    "/usr/local/lib/libkrunfw.${DYLIB_EXT}" \
    "@rpath/libkrunfw.${DYLIB_EXT}" \
    "${STAGE}/lib/libkrun.${DYLIB_EXT}" || true
else
  # Linux: set RUNPATH = $ORIGIN so the loader looks next to the .so.
  for lib in libkrun libkrunfw; do
    file="${STAGE}/lib/${lib}.${DYLIB_EXT}"
    patchelf --set-rpath '$ORIGIN' "$file" || exit 4
  done
fi

# ---------------------------------------------------------------------------
# Synthesise libkrun.pc so consumers using pkg-config (e.g. krun-sys's
# build.rs) find the staged include + lib paths.
#
# ${prefix} is intentionally a placeholder; ward-core/build.rs rewrites
# it to the actual OUT_DIR path at consumer build time. pkg-config
# respects $PKG_CONFIG_SYSROOT_DIR for this kind of relocation.
# ---------------------------------------------------------------------------

cat > "${STAGE}/lib/pkgconfig/libkrun.pc" <<EOF
prefix=__VENDOR_PREFIX__
exec_prefix=\${prefix}
libdir=\${exec_prefix}/lib
includedir=\${prefix}/include

Name: libkrun
Description: Dynamic library for spawning microVMs
Version: ${VERSION}
Libs: -L\${libdir} -lkrun
Cflags: -I\${includedir}
EOF

# ---------------------------------------------------------------------------
# Tar + checksum the result.
# ---------------------------------------------------------------------------

TARBALL="libkrun-${VERSION}-${TARGET}.tar.gz"
echo "==> Producing ${TARBALL}"
tar -C "${STAGE}" -czf "${DIST}/${TARBALL}" .

# Cross-platform SHA-256. macOS ships `shasum` (Perl), Linux ships `sha256sum`.
if command -v sha256sum > /dev/null 2>&1; then
  (cd "${DIST}" && sha256sum "${TARBALL}")
else
  (cd "${DIST}" && shasum -a 256 "${TARBALL}")
fi

echo "==> Done. Tarball at ${DIST}/${TARBALL}"
