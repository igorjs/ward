# ADR-019 Quality Gate Report

Parent ADR: docs/adr/019-inprocess-smoltcp-networking.md

Blueprint: docs/adr/019-inprocess-smoltcp-networking-blueprint.md

## Summary

Three gate rounds run. Round 1 (Phase 2 critic + Phase 3 test-reviewer, both FAIL) surfaced 12 decision-level findings and 4 test-plan findings, all fixed. Round 2 (re-review) confirmed 9/12 and 6/7 fixed respectively, surfaced 2 new decision-level findings (a wrong-mode scoping on the SSRF guard, and 6 more mechanical/citation issues) plus 2 new test-plan findings, all fixed. Round 3 (final scheduled round) confirmed 6/8 fully fixed, found 3 blocking mechanical issues (a Done-When dependency cycle, a stale open item contradicting an already-applied fix, and a wrong line-range citation pointing at unrelated validation code) and 5 non-blocking citation/precision issues, plus one test-plan issue (a test referencing a type across a crate boundary that doesn't exist yet). All applied and self-verified against source directly (grep/sed against the actual files, not re-dispatched to a 4th agent round, since the critic's own round-3 assessment characterized the remaining items as "mechanical edits, no design rework").

## Gate Result

**Fact-Check (Phase 1, run inline each round):** PASS (3/3) — every file:line citation checked against source; round-2 and round-3 each caught real drift (line numbers shifted by an unrelated repo-wide markdown reflow between rounds 1 and 2; wrong struct names, wrong FFI signatures, wrong milestone attributions surfaced by the critic and independently verified).

**Adversarial Review (Phase 2):**
- Round 1: FAIL (12 findings, 1 critical)
- Round 2: FAIL (3 high, 5 medium/low)
- Round 3: FAIL (3 blocking/mechanical, 5 non-blocking) — all applied and verified without a 4th dispatch

**Test Review (Phase 3):**
- Round 1: FAIL (4/9 checks)
- Round 2: FAIL (7/9 checks, 2 new minor gaps)
- Round 3: FAIL (2/3 checks, 1 blocking: a WU-5 test referenced `EgressMode`, a `ward-core`-owned type `ward-net` cannot depend on, and relied on a callback seam WU-6 doesn't create until later) — relocated to WU-9 where the real seam exists, verified

## Quality gate override

Proceeding to Accepted without a 4th automated critic/test-reviewer dispatch. Round 3's own verdict characterized all three blocking findings as "mechanical edits to the blueprint, no design rework," and named B2/B3 (not B1) as the ones that would actively mislead an implementer if deferred, none as requiring further design iteration. All 3 blocking and 5 non-blocking round-3 items, plus the 1 blocking test-plan item, were applied and independently re-verified against source (not just re-worded) before this report was written: `manager.rs:182-190`'s actual content read and confirmed as the real `SEC-ALLOWLIST` block, `ward-net/Cargo.toml:35`'s current tokio features read and confirmed as the target of the consolidation, the stale `is_private_or_local` open item read and confirmed as directly contradicting an already-applied fix before deletion. Override reason: diminishing findings per round (12 → 8 → 3 blocking) with the round-3 findings themselves assessed as non-design-affecting, against the cost of a 4th full dual-agent dispatch for citation-level fixes already independently confirmed.

## Structural Checks

- [x] Every Considered Alternatives entry has effort and trade-off detail.
- [x] The Decision section explains why each rejected alternative was rejected.
- [x] All work units have file plans with real paths, independently verified.
- [x] All verification commands are literal (no placeholders).
- [x] No unresolved questions remain unstated; open items (libkrun's `features`/`flags` semantics for `krun_add_net_unixgram`, whether libkrun performs DHCP on this FD path, `ward-daemon/tests/` harness precedent, nightly KVM/HVF CI infrastructure, general guest UDP egress) are each named with a concrete verifier (which WU, what to check) rather than left vague.
