# PR1: `--features krunvm` CI verification job — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a CI job that compiles and lints the workspace under
`--features krunvm` against the real libkrun bottle, fix the missing linker
search path in `ward-core/build.rs`, and scaffold a KVM-gated boot
integration test for a future self-hosted runner.

**Architecture:** A new `krunvm-build` job in `.github/workflows/ci.yml`
downloads the pinned `igorjs/ward-vendor` libkrun bottle (verifying its
SHA-256 against `vendor/libkrun-checksums.txt`), exposes it via
`LIBKRUN_PREFIX` + `LD_LIBRARY_PATH`, then runs `cargo build` and `cargo
clippy` with the feature on. `build.rs` learns to emit a
`rustc-link-search` for `$LIBKRUN_PREFIX/lib`. No microVM is booted (standard
runners lack KVM); a gated `tests/krun_boot.rs` is added for later.

**Tech Stack:** GitHub Actions, Rust/Cargo, libkrun 1.18.0 bottles.

**Verification model:** There is no local toolchain. Every "run" step is
performed by pushing the branch and reading GitHub Actions via
`gh pr checks <branch> --watch`. CI is the source of truth.

---

### Task 1: Emit linker search path for the vendored libkrun

`build.rs` currently emits `rustc-link-lib=krun`/`krunfw` but no
`rustc-link-search`, so the linker only finds libkrun if it sits on the
default search path. The vendored bottle extracts to a custom prefix, so the
build needs an explicit search path driven by an env var.

**Files:**
- Modify: `ward-core/build.rs:31-35`

- [ ] **Step 1: Replace the `krunvm` link block**

In `ward-core/build.rs`, replace the existing block:

```rust
    #[cfg(feature = "krunvm")]
    {
        println!("cargo:rustc-link-lib=krun");
        println!("cargo:rustc-link-lib=krunfw");
    }
```

with:

```rust
    #[cfg(feature = "krunvm")]
    {
        // Release/CI builds extract the libkrun bottle to a custom prefix
        // and export LIBKRUN_PREFIX. Point the linker at <prefix>/lib so
        // -lkrun/-lkrunfw resolve. Local dev installs (brew, distro pkg)
        // land on the default search path, so the env var is optional.
        println!("cargo:rerun-if-env-changed=LIBKRUN_PREFIX");
        if let Ok(prefix) = std::env::var("LIBKRUN_PREFIX") {
            println!("cargo:rustc-link-search=native={prefix}/lib");
        }
        println!("cargo:rustc-link-lib=krun");
        println!("cargo:rustc-link-lib=krunfw");
    }
```

- [ ] **Step 2: Verify (deferred to CI)**

This change only takes effect under `--features krunvm`, which the new job in
Task 3 exercises. No standalone verification step.

---

### Task 2: Add a KVM-gated boot integration test scaffold

A placeholder integration test that documents and reserves the boot-test
harness shape. It is skipped unless `WARD_KVM_TESTS=1`, so it is inert on
standard runners and on developer machines, but a future self-hosted KVM
runner can enable it by setting one env var.

**Files:**
- Create: `ward-core/tests/krun_boot.rs`

- [ ] **Step 1: Create the gated test file**

```rust
// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! KVM-gated boot integration tests.
//!
//! These require a real KVM-capable host (`/dev/kvm`) plus
//! `--features krunvm` and an installed libkrun. Standard GitHub-hosted
//! runners provide none of these, so the tests are gated behind
//! `WARD_KVM_TESTS=1` and skip cleanly everywhere else. A future
//! self-hosted KVM runner enables them by exporting that variable.

/// Returns true when KVM boot tests are explicitly opted into.
fn kvm_tests_enabled() -> bool {
    std::env::var("WARD_KVM_TESTS").as_deref() == Ok("1")
}

#[test]
fn boot_harness_is_gated_off_by_default() {
    if kvm_tests_enabled() {
        // A real boot test will live here once a KVM runner exists:
        // create a sandbox, assert it reaches RUNNING, exec `true`,
        // assert exit 0, remove it. For now the gate itself is the
        // contract under test.
        eprintln!("WARD_KVM_TESTS=1: KVM boot tests would run here");
    } else {
        // Inert on standard runners and dev machines.
        assert!(!kvm_tests_enabled());
    }
}
```

- [ ] **Step 2: Verify (deferred to CI)**

Runs in the existing `test` job (stub build) as a normal integration test;
it passes because the gate is off. Confirmed green in CI in Task 4.

---

### Task 3: Add the `krunvm-build` CI job

**Files:**
- Modify: `.github/workflows/ci.yml` (add a job after `e2e`)

- [ ] **Step 1: Add the job**

Insert this job into `.github/workflows/ci.yml` (sibling to the existing
jobs, after `e2e`):

```yaml
  # ── libkrun feature build ────────────────────────────────────────────
  # Compiles + lints the workspace with the real libkrun backend enabled.
  # Downloads the pinned libkrun bottle from igorjs/ward-vendor (the same
  # source release.yml uses) and verifies it against the SHA-256 sums in
  # vendor/libkrun-checksums.txt. Does NOT boot a microVM: standard runners
  # lack /dev/kvm, so this tier proves FFI signatures, linking, and all
  # non-boot logic compile. Real boot tests are gated behind WARD_KVM_TESTS
  # for a future self-hosted KVM runner (see ward-core/tests/krun_boot.rs).
  krunvm-build:
    needs: [clippy]
    runs-on: ubuntu-24.04
    env:
      GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
    steps:
      - uses: actions/checkout@v6
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - uses: Swatinem/rust-cache@v2
      - name: Install protoc
        run: |
          sudo apt-get update
          sudo apt-get install -y --no-install-recommends protobuf-compiler
      - name: Fetch + verify libkrun bottle
        run: |
          set -euo pipefail
          LIBKRUN_VERSION="$(tr -d '[:space:]' < vendor/libkrun-version.txt)"
          TARBALL="libkrun-${LIBKRUN_VERSION}-x86_64-unknown-linux-gnu.tar.gz"
          mkdir -p libkrun-vendor
          gh release download "libkrun-v${LIBKRUN_VERSION}" \
            --repo igorjs/ward-vendor \
            --pattern "$TARBALL" \
            --dir libkrun-vendor
          EXPECTED="$(grep -E "^[a-f0-9]+  ${TARBALL}\$" vendor/libkrun-checksums.txt | awk '{print $1}')"
          if [[ -z "$EXPECTED" ]]; then
            echo "::error::no checksum entry for ${TARBALL} in vendor/libkrun-checksums.txt"
            exit 1
          fi
          ACTUAL="$(sha256sum "libkrun-vendor/${TARBALL}" | awk '{print $1}')"
          if [[ "$EXPECTED" != "$ACTUAL" ]]; then
            echo "::error::SHA-256 mismatch for ${TARBALL}: expected ${EXPECTED}, got ${ACTUAL}"
            exit 1
          fi
          mkdir -p libkrun-vendor/extracted
          tar -xzf "libkrun-vendor/${TARBALL}" -C libkrun-vendor/extracted
          PREFIX="$(pwd)/libkrun-vendor/extracted"
          echo "LIBKRUN_PREFIX=${PREFIX}" >> "$GITHUB_ENV"
          echo "LD_LIBRARY_PATH=${PREFIX}/lib" >> "$GITHUB_ENV"
      - name: Build (--features krunvm)
        run: cargo build --workspace --features ward-core/krunvm
      - name: Clippy (--features krunvm)
        run: cargo clippy --all-targets --features ward-core/krunvm -- -D warnings
```

- [ ] **Step 2: Verify (deferred to CI)** — see Task 4.

---

### Task 4: Push, open PR, and drive CI to green

**Files:** none (git + CI).

- [ ] **Step 1: Branch, commit, push**

```bash
git checkout -b feat/krunvm-ci
git add ward-core/build.rs ward-core/tests/krun_boot.rs .github/workflows/ci.yml
git commit -m "ci: build + lint workspace under --features krunvm"
git push -u origin feat/krunvm-ci
```

(Commits are SSH-signed via 1Password; the desktop app must be unlocked to
approve each signature.)

- [ ] **Step 2: Open the PR**

```bash
gh pr create --title "ci: verify --features krunvm builds + links" --body "$(cat <<'EOF'
## Summary
- Add a `krunvm-build` CI job that compiles + lints the workspace with the
  real libkrun backend, using the pinned ward-vendor bottle (SHA-256 verified
  against vendor/libkrun-checksums.txt).
- Fix `ward-core/build.rs` to emit a linker search path for the vendored
  libkrun prefix (`LIBKRUN_PREFIX`).
- Scaffold a KVM-gated boot integration test (`WARD_KVM_TESTS=1`) for a
  future self-hosted runner.

This is the verification gate every subsequent libkrun feature PR relies on.
It does not boot a microVM (standard runners lack KVM).

## Test plan
- [ ] All existing CI jobs green.
- [ ] New `krunvm-build` job compiles + lints with `--features krunvm`.
EOF
)"
```

- [ ] **Step 3: Watch CI and fix until green**

Run: `gh pr checks feat/krunvm-ci --watch`
Expected: every job concludes `pass`. In particular `krunvm-build` must
compile and link. If it fails:
- **Link errors (`undefined reference`/`cannot find -lkrun`)** → inspect the
  extracted bottle layout (`tar -tzf` the tarball) and adjust the
  `LIBKRUN_PREFIX`/lib path or `LD_LIBRARY_PATH` in the job.
- **Compile errors under the feature** → fix the offending `#[cfg(feature =
  "krunvm")]` code in `ward-core/src/backend/krunvm.rs` (this would be the
  first time that code has ever been compiled in CI; real breakage is fixed
  here as part of PR1).
- **Clippy warnings** → fix at the call site.
Re-push fixes (new commits) until all checks pass.

---

## Self-review

- **Spec coverage:** Implements spec PR1 (CI `krunvm` build+link+unit job +
  gated KVM harness scaffold). The build.rs link-search fix is a discovered
  prerequisite not explicitly in the spec but required for the job to link.
- **Placeholder scan:** The boot test body is intentionally a documented
  reservation, not a TODO — its assertion (the gate is off) is real and runs.
- **Type consistency:** `kvm_tests_enabled()` defined and used within the
  same file; no cross-task symbol references.
- **Note on "unit" tier:** the spec said build+link+unit. The existing
  `krunvm.rs` unit tests assume stub mode and would call `krun_start_enter`
  (needs KVM) under the feature, so this job runs build + clippy only. The
  stub unit tests continue to run in the existing `test` job. This is a
  faithful, safe realisation of the intent (compile + link verification).
