# Ward rollout plan (post 2026-05-29 session)

This document captures where `main` is after the SLSA L3 + 13-branch
consolidation campaign, and what's still required before a real v0.1.0
release. It lives under `tmp/` because it's a planning artefact, not
project documentation; once v0.1.0 ships this file can be deleted.

## Current state of `main`

All work that existed as local-only branches at the start of the
session is now on `origin/main`. The 13 PRs landed in this session
(#66-#78), plus the SLSA L3 saga (#59-#65), brings ward to:

### Supply-chain posture (complete)

- Release pipeline produces SLSA Build L3 attestation via the
  `slsa-framework/slsa-github-generator/.../generator_generic_slsa3.yml@v2.1.0`
  reusable workflow.
- `install.sh` verifies via `slsa-verifier verify-artifact ...` (the
  canonical SLSA-framework tool, not `gh attestation verify` which
  queries GitHub's separate first-party Attestations API).
- README shows SLSA Level 3 badge alongside CI, cargo-audit, OpenSSF
  Scorecard, License, Rust, Status.
- `workflow-hygiene.sh` SHA-pin rule has a documented exemption for
  `slsa-framework/slsa-github-generator` (the generator rejects SHA
  pins at runtime; the exemption matches the existing
  `dtolnay/rust-toolchain` pattern).

### Security hardening (complete)

All 22 SEC-* findings from the May 2026 audit have remediation on
`main`:

- SEC-002/003/004: umask + secure dir creation (PR #38)
- SEC-005: server-side egress resolve + private-range deny (PR #38)
- SEC-009: gRPC message-size caps + concurrency limits (PR #75)
- SEC-010: agent frame allocator caps + len=0 rejection (PR #66)
- SEC-012: SLSA build provenance (PRs #38, #59-#65)
- SEC-013: install.sh umask + chmod (PR #38)
- SEC-014: atomic snapshot swap (Linux EXCHANGE; macOS RENAME_SWAP) (PRs #38, #68)
- SEC-015: per-connection + global concurrency caps (PR #75)
- SEC-017/018: kill_process ownership verification (PR #67)
- SEC-019: registry allowlist with normalisation (PR #78)
- SEC-020: mount source allowlist + readonly-required for sensitive paths (PR #76)
- SEC-021/022: strict OCI grammar + lowercase entity_id (PR #69)
- Workflow hygiene drift fixes (PR #60 + repo-config #4, #5)

### Observability (complete)

- Opt-in Prometheus metrics via `WARD_METRICS_ADDR` (PR #77)
- Sandbox lifecycle metrics + broker pub/sub counters + egress
  decision counters
- Timeout-driven cleanup routes through the same path as user-initiated
  remove (no gauge leak)

### Documentation (complete for current surface)

- 3 deferred ADRs (013 multi-tenant authn/authz, 014 WASM backend,
  015 live migration) in `docs/adr/` (PR #70)
- AI-agent sandbox example with reusable shell wrapper (PR #71)
- README posture badges + CONTRIBUTING supply-chain section (PR #74)

### SDK scaffolds (Apache-2.0, surface-only)

- Python (`sdks/python/`): pyproject + WardClient + 8 smoke tests (PR #72)
- TypeScript (`sdks/typescript/`): package.json + Promise-based client (PR #73)
- Go (`sdks/go/`): module + option-pattern client (PR #73)
- Rust (`sdks/rust/ward-client/`): in-workspace crate with NO path-dep
  on AGPL `ward-core` (proto types come from a future ward-proto
  crate or local codegen) (PR #73)

All four are NotImplementedError-bodied; the gRPC wire-up is gated on
the agent + real exec path landing.

## What still blocks v0.1.0

A v0.1.0 release would be misleading without the lead feature
("run untrusted code in hardware-isolated microVMs") actually working
under `--features krunvm`. Today that path stubs the exec channel.

### Critical (lead feature)

| Issue | Title |
|---|---|
| #7  | feat(backend): real OCI image pull via oci-distribution crate |
| #9  | feat(agent): ward-agent guest-side init binary (vsock RPC server) |
| #10 | feat(backend): real exec via vsock to ward-agent |
| #12 | feat(backend): real kill_process via vsock signal |
| #13 | feat(backend): real stream_output via vsock pipe |
| #14 | feat(backend): real write_stdin via vsock pipe |

Without #9 + #10 specifically, `ward exec` is decorative under
`--features krunvm`. #12-#14 follow naturally from #10.

### Important (release ergonomics)

| Issue | Title |
|---|---|
| #21 | build(release): Homebrew tap + auto-update workflow |
| #22 | build(release): Linux .deb packaging + apt repo with GPG signing |
| #23 | build(release): launchd plist + systemd unit for auto-start wardd |
| #24 | ci: lint job that compiles with --features krunvm on Linux |
| #26 | docs: real README with value prop, quickstart, links |
| #28 | docs: deployment guide for running wardd in production |

### Nice-to-have (post-v0.1.0)

- #16 graceful shutdown on SIGTERM/SIGINT
- #18 CLI --json output mode
- #19 CLI table formatting
- #20 review gRPC Status::internal sites
- #25 perf benchmarks
- #27 architecture docs
- #29 libkrun snapshot/restore when upstream lands
- #30 Apple notarisation (.pkg installer + code signing)
- #31 non-root workloads via krun_setuid / krun_setgid
- #32 network port publishing (`-p` parity)
- #33 spike: multi-port virtio-console as vsock alternative
- #35 hand-maintained extern "C" replacing krun-sys (already largely done)
- ADRs 013/014/015 as proposed/deferred (#56, #57, #58 - tracking issues)
- SDK wire-up: #39-#42 (Python/TS/Go/Rust)
- SLSA L3 cosign (#45 second half)
- Cache OCI images (#54)
- Per-sandbox resource accounting (#55)

## Recommended sequencing

1. **`v0.0.1-alpha.1` prerelease NOW** (cheap, validates production
   path): `gh release create v0.0.1-alpha.1 --prerelease --target main
   --generate-notes`. Triggers release.yml, builds three target
   tarballs with libkrun bundled, attaches SLSA L3 attestation,
   validates `install.sh` end-to-end against real users. Doesn't
   become "Latest" because of `--prerelease`. Distinguishes clearly
   that this is not v0.1.0.
2. **#9 (agent) + #10 (real exec via vsock)** as the first real
   feature work. Estimated 1-2 weeks; this is the boundary between
   "demo" and "real".
3. **#12, #13, #14 (kill/stream/stdin)** follow naturally from #10.
4. **#7 (real OCI image pull)** could land in parallel with the
   agent work since it touches a different module.
5. **#24 (CI lint with --features krunvm on Linux)** lands when #9-#14
   land; CI then exercises the real backend path on every PR.
6. **`v0.1.0` proper**, after #7 + #9 + #10 + #12 + #13 + #14 + #24
   are all on `main`. Drop the "Status: pre-release" badge from
   README at the same time.
7. Distribution (#21 Homebrew, #22 .deb, #23 systemd/launchd) can
   land between v0.1.0 and v0.2.0.

## Known caveats / loose ends

- `e2e_volume` fails on macOS local test runs because `mkfs.ext4`
  isn't available. Pre-existing, unrelated to recent work. Either
  gate the test on `#[cfg(target_os = "linux")]` or ship a stub
  formatter for macOS.
- Dependabot PR open (`dependabot/go_modules/sdks/go/google.golang.org/grpc-1.79.3`)
  bumping go.mod grpc dep. Routine.
- `tmp/` directory is NOT gitignored. This `tmp/ROLLOUT-PLAN.md` is
  intentionally tracked under a `tmp/rollout-plan` branch so it has
  a stable URL; delete the branch + the file once v0.1.0 ships.
- One commit (`fix(release): add libprotobuf-dev for well-known proto
  includes on Linux`) was accidentally direct-pushed to `main` during
  the SLSA L3 saga because `git checkout -b X origin/main` made the
  local branch track `origin/main`. The change is signed + correct +
  what we'd have merged via PR. Process fix: use `git switch -c X`
  for new branches (no tracking inherited from base).

## Verification commands

L3 attestation, end-to-end, given a release tag `v<x.y.z>`:

```sh
# Download
gh release download v<x.y.z> --repo igorjs/ward --pattern '*.tar.gz' --pattern 'multiple.intoto.jsonl'

# Verify
slsa-verifier verify-artifact \
  --provenance-path multiple.intoto.jsonl \
  --source-uri github.com/igorjs/ward \
  ward-<x.y.z>-aarch64-apple-darwin.tar.gz

# Expected: "PASSED: SLSA verification passed"
```

If `slsa-verifier` isn't on PATH: `brew install slsa-verifier`.

## Open questions

- Should `repo-config`'s `repo-ward.tf` add the SLSA generator's
  status check (e.g. `slsa_provenance / final`) to
  `required_status_check_contexts`? Answer: no, because release.yml
  only runs on tag-push events, never on PRs; adding it as a
  required PR check would block every PR forever. The L3 gate is
  already enforced in-workflow: `publish` needs `slsa_provenance`,
  so a failed attestation skips the release entirely. The branch
  protection ruleset doesn't need a separate signal.
- Should `install.sh` fall back to anything when `slsa-verifier` is
  missing? Current behaviour: WARN + continue with SHA-256-only.
  Alternative: hard-require the verifier (refuse to install).
  Current default is more user-friendly and consistent with the
  prior `gh CLI` fallback shape.
