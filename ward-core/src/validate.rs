// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Input validation for gRPC request fields.
//!
//! All validation happens at the API boundary before requests reach
//! business logic. This prevents invalid data from propagating through
//! the system and ensures consistent error messages for clients.

use crate::protocol::ApiError;

/// Maximum sane values to prevent resource exhaustion.
const MAX_CPUS: u32 = 64;
const MAX_MEMORY_MB: u32 = 65_536; // 64 GiB
const MAX_PIDS: u32 = 65_536;
const MAX_TIMEOUT_SECONDS: u64 = 2_592_000; // 30 days
#[allow(dead_code)]
const MAX_VOLUME_SIZE_MB: u32 = 1_048_576; // 1 TiB

/// Maximum sizes for cross-sandbox communication primitives.
const MAX_TOPIC_LEN: usize = 128;
const MAX_GROUP_LEN: usize = 64;
/// 1 MiB per published message. Large enough for batched events; small enough
/// to bound per-sandbox memory under burst traffic.
pub const MAX_PUBLISH_PAYLOAD_BYTES: usize = 1_048_576;

/// Validate an OCI image reference.
///
/// SEC-021: enforces an allow-list character set rather than a
/// shell-metacharacter deny-list. The OCI grammar permits only ASCII
/// alphanumerics plus a small punctuation set (`:` for tag separator,
/// `/` for path segments, `.` `_` `-` inside names, `@` before digest).
/// Anything outside that set is rejected. Structural rules (no `..`
/// for path-traversal, length cap) layer on top.
///
/// Allow-list is strictly stronger than the previous deny-list:
/// shell metacharacters, control characters, whitespace, and unicode
/// homoglyphs that could escape escaping logic downstream are all
/// rejected by construction rather than enumeration.
pub fn image_ref(image: &str) -> Result<(), ApiError> {
    if image.is_empty() {
        return Err(ApiError::InvalidRequest(
            "image reference must not be empty".into(),
        ));
    }
    if image.len() > 255 {
        return Err(ApiError::InvalidRequest(
            "image reference exceeds 255 characters".into(),
        ));
    }
    if image.contains("..") {
        return Err(ApiError::InvalidRequest(
            "image reference must not contain '..'".into(),
        ));
    }
    // Allow-list: OCI-grammar-permitted characters only.
    if !image
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '/' | '.' | '_' | '-' | '@'))
    {
        return Err(ApiError::InvalidRequest(
            "image reference contains characters outside the OCI grammar (allowed: \
             alphanumerics, ':', '/', '.', '_', '-', '@')"
                .into(),
        ));
    }
    Ok(())
}

/// Validate resource limits are within sane bounds.
/// Zero values are allowed (means "use default").
pub fn resource_limits(
    cpus: u32,
    memory_mb: u32,
    pids_max: u32,
    timeout_seconds: u64,
) -> Result<(), ApiError> {
    if cpus > MAX_CPUS {
        return Err(ApiError::InvalidRequest(format!(
            "cpus must be <= {MAX_CPUS}"
        )));
    }
    if memory_mb > MAX_MEMORY_MB {
        return Err(ApiError::InvalidRequest(format!(
            "memory_mb must be <= {MAX_MEMORY_MB}"
        )));
    }
    if pids_max > MAX_PIDS {
        return Err(ApiError::InvalidRequest(format!(
            "pids_max must be <= {MAX_PIDS}"
        )));
    }
    if timeout_seconds > MAX_TIMEOUT_SECONDS {
        return Err(ApiError::InvalidRequest(format!(
            "timeout_seconds must be <= {MAX_TIMEOUT_SECONDS}"
        )));
    }
    Ok(())
}

/// Validate a volume name.
/// Must be non-empty, alphanumeric + dash/underscore, max 64 chars.
pub fn volume_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty() {
        return Err(ApiError::InvalidRequest(
            "volume name must not be empty".into(),
        ));
    }
    if name.len() > 64 {
        return Err(ApiError::InvalidRequest(
            "volume name exceeds 64 characters".into(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::InvalidRequest(
            "volume name must contain only alphanumeric characters, dashes, and underscores".into(),
        ));
    }
    Ok(())
}

/// Validate a bind mount's source (host) and target (guest) paths.
///
/// Both must be absolute, and neither may contain a `..` component — a guest
/// `..` could escape its mount point, and a host `..` could widen what the
/// caller intended to share.
///
/// SEC-020: when `allow_host` is false (the safe default), source paths
/// must fall under one of the allowed roots: `/home/<user>/`, `/tmp/`,
/// `/var/lib/ward/`. Set `WARD_ALLOW_HOST_MOUNTS=1` at the daemon to
/// pass `allow_host=true` and lift the restriction (useful for
/// trusted operator workflows like CI runners with explicit host-FS
/// access). Even with the opt-in, system-critical roots (`/proc`,
/// `/sys`, `/dev`, `/etc`, `/root`, `/boot`, `/usr`) still require
/// `readonly: true` so a sandbox can't overwrite host system files.
pub fn mount(source: &str, target: &str, readonly: bool, allow_host: bool) -> Result<(), ApiError> {
    for (label, path) in [("source", source), ("target", target)] {
        if path.is_empty() {
            return Err(ApiError::InvalidRequest(format!(
                "mount {label} must not be empty"
            )));
        }
        if !path.starts_with('/') {
            return Err(ApiError::InvalidRequest(format!(
                "mount {label} must be an absolute path: {path}"
            )));
        }
        if path.split('/').any(|c| c == "..") {
            return Err(ApiError::InvalidRequest(format!(
                "mount {label} must not contain '..': {path}"
            )));
        }
    }

    // SEC-020: source allowlist (host-side path the sandbox can see).
    if !allow_host && !is_default_allowed_source(source) {
        return Err(ApiError::InvalidRequest(format!(
            "mount source {source} is outside the default allowlist \
             (/home/, /tmp/, /var/lib/ward/); set WARD_ALLOW_HOST_MOUNTS=1 \
             on the daemon to permit arbitrary host paths"
        )));
    }

    // SEC-020: system-critical paths always require readonly, even with
    // the host-mounts opt-in. A writable /etc inside a sandbox means
    // the guest can rewrite host system files via the virtiofs share.
    if is_sensitive_host_path(source) && !readonly {
        return Err(ApiError::InvalidRequest(format!(
            "mount source {source} is a sensitive system path \
             (/proc, /sys, /dev, /etc, /root, /boot, /usr); \
             readonly: true is required for these roots"
        )));
    }

    Ok(())
}

/// True if `path` falls under one of the default-allowed mount roots.
/// Match is by prefix on the canonical "/" form so `/home/alice/data`
/// and `/home/alice` both qualify, but `/homeless` does not.
fn is_default_allowed_source(path: &str) -> bool {
    const ALLOWED: &[&str] = &["/home/", "/tmp/", "/var/lib/ward/"];
    if path == "/tmp" || path == "/var/lib/ward" {
        return true;
    }
    ALLOWED.iter().any(|prefix| path.starts_with(prefix))
}

/// True if `path` is one of the system roots that must never be
/// mounted writable into a sandbox.
fn is_sensitive_host_path(path: &str) -> bool {
    const SENSITIVE: &[&str] = &["/proc", "/sys", "/dev", "/etc", "/root", "/boot", "/usr"];
    SENSITIVE
        .iter()
        .any(|root| path == *root || path.starts_with(&format!("{root}/")))
}

/// Validate a sandbox or volume ID (UUID format).
///
/// SEC-022: rejects uppercase hex characters outright. UUIDs are
/// generated by `uuid::Uuid::new_v4().to_string()` which always emits
/// lowercase; downstream HashMaps look up by exact-string match. A
/// caller sending `550E8400-...` (uppercase) would `NotFound` against
/// a record stored as `550e8400-...` — a footgun that could mask a
/// real bug. Rejecting at the validator boundary makes the mismatch
/// loud instead of silent.
pub fn entity_id(id: &str, entity: &str) -> Result<(), ApiError> {
    if id.is_empty() {
        return Err(ApiError::InvalidRequest(format!(
            "{entity} ID must not be empty"
        )));
    }
    // UUIDs are 36 chars (with hyphens) or 32 chars (without)
    if id.len() > 36 {
        return Err(ApiError::InvalidRequest(format!(
            "{entity} ID exceeds 36 characters"
        )));
    }
    if !id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f' | '-')) {
        return Err(ApiError::InvalidRequest(format!(
            "{entity} ID contains invalid characters (allowed: lowercase hex digits and '-')"
        )));
    }
    Ok(())
}

/// Validate an exec command is non-empty.
pub fn exec_command(command: &[String]) -> Result<(), ApiError> {
    if command.is_empty() {
        return Err(ApiError::InvalidRequest("command must not be empty".into()));
    }
    if command[0].is_empty() {
        return Err(ApiError::InvalidRequest(
            "command executable must not be empty".into(),
        ));
    }
    Ok(())
}

/// Validate a language name for the run RPC.
pub fn language_name(language: &str) -> Result<(), ApiError> {
    if language.is_empty() {
        return Err(ApiError::InvalidRequest(
            "language must not be empty".into(),
        ));
    }
    if !language
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(ApiError::InvalidRequest(
            "language name contains invalid characters".into(),
        ));
    }
    Ok(())
}

/// Validate a pub/sub topic name.
///
/// Topics use a dotted-segment syntax (e.g. `agent.results.build`). Restricting
/// the character set keeps topics safe to render in logs, embed in metrics,
/// and persist in audit records without escaping.
pub fn topic_name(topic: &str) -> Result<(), ApiError> {
    if topic.is_empty() {
        return Err(ApiError::InvalidRequest("topic must not be empty".into()));
    }
    if topic.len() > MAX_TOPIC_LEN {
        return Err(ApiError::InvalidRequest(format!(
            "topic exceeds {MAX_TOPIC_LEN} characters"
        )));
    }
    // Allow alphanumeric, dash, underscore, and dot (for namespacing).
    if !topic
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(ApiError::InvalidRequest(
            "topic must contain only alphanumeric characters, dashes, underscores, and dots".into(),
        ));
    }
    // Leading/trailing/consecutive dots are confusing and ambiguous for routing.
    if topic.starts_with('.') || topic.ends_with('.') || topic.contains("..") {
        return Err(ApiError::InvalidRequest(
            "topic must not have leading, trailing, or consecutive dots".into(),
        ));
    }
    Ok(())
}

/// Validate a communication group name.
///
/// Group names act as opaque routing keys: two sandboxes with identical group
/// strings can exchange messages. They must be sanitisable for logs.
pub fn group_name(group: &str) -> Result<(), ApiError> {
    if group.is_empty() {
        return Err(ApiError::InvalidRequest(
            "communication group must not be empty when mode is GROUP".into(),
        ));
    }
    if group.len() > MAX_GROUP_LEN {
        return Err(ApiError::InvalidRequest(format!(
            "communication group exceeds {MAX_GROUP_LEN} characters"
        )));
    }
    if !group
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::InvalidRequest(
            "communication group must contain only alphanumeric characters, dashes, and underscores"
                .into(),
        ));
    }
    Ok(())
}

/// Validate that a publish payload does not exceed the per-message cap.
pub fn publish_payload(payload: &[u8]) -> Result<(), ApiError> {
    if payload.len() > MAX_PUBLISH_PAYLOAD_BYTES {
        return Err(ApiError::InvalidRequest(format!(
            "publish payload exceeds {MAX_PUBLISH_PAYLOAD_BYTES} bytes"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
//
// Every validator is exercised on three axes:
//   1. Happy path — a typical valid input is accepted.
//   2. Named-rule rejections — each branch that returns an error is hit.
//   3. Boundary — exactly-at-limit accepted, one-over rejected.
//
// `rstest`'s `#[case]` attribute generates one test function per row, so
// failures point at the specific input rather than a single opaque table.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    // ----- SEC-020 mount allowlist + sensitive-path boundaries -------------

    #[test]
    fn given_homeless_path_when_is_default_allowed_then_rejected() {
        // Regression guard: the allowlist matches by prefix WITH a
        // trailing slash, so `/homeless` must NOT match `/home/`.
        // Without the trailing slash a naive prefix check would let a
        // sandbox bind-mount the operator's `/homeless` directory.
        assert!(!is_default_allowed_source("/homeless/secrets"));
        assert!(!is_default_allowed_source("/homeless"));
    }

    #[test]
    fn given_etcetera_path_when_is_sensitive_then_not_flagged() {
        // Symmetric to the allowlist case: `/etc` must match `/etc` and
        // `/etc/passwd`, but NOT `/etcetera`. The `format!("{root}/")`
        // suffix is what enforces the boundary.
        assert!(!is_sensitive_host_path("/etcetera"));
        assert!(!is_sensitive_host_path("/etcetera/passwd"));
    }

    #[test]
    fn given_etc_root_and_subpath_when_is_sensitive_then_flagged() {
        // Positive boundary cases: the bare root and any subpath under
        // it both count as sensitive. Both must require `readonly`.
        assert!(is_sensitive_host_path("/etc"));
        assert!(is_sensitive_host_path("/etc/passwd"));
    }

    #[test]
    fn given_tmp_and_var_lib_ward_roots_when_is_default_allowed_then_ok() {
        // Bare root paths (without trailing slash) should match the
        // allowlist; otherwise mounting `/tmp` itself would inexplicably
        // fail while mounting `/tmp/subdir` would succeed.
        assert!(is_default_allowed_source("/tmp"));
        assert!(is_default_allowed_source("/var/lib/ward"));
    }

    // ----- image_ref ------------------------------------------------------

    #[rstest]
    #[case::simple("alpine:latest")]
    #[case::registry_path("docker.io/library/python:3.12-slim")]
    #[case::tag_with_dash("node:22-alpine")]
    #[case::digest("alpine@sha256:abcd1234")]
    fn image_ref_accepts_valid(#[case] input: &str) {
        assert!(image_ref(input).is_ok(), "expected {input:?} to be valid");
    }

    #[rstest]
    #[case::empty("")]
    #[case::path_traversal("../../../etc/passwd")]
    #[case::semicolon("alpine;rm")]
    #[case::backtick("alpine`whoami`")]
    #[case::dollar("alpine$(id)")]
    #[case::newline("alpine\n")]
    #[case::null("alpine\0")]
    fn image_ref_rejects_invalid(#[case] input: &str) {
        assert!(
            image_ref(input).is_err(),
            "expected {input:?} to be rejected"
        );
    }

    #[test]
    fn image_ref_rejects_at_length_boundary() {
        // 255 chars exactly: accepted; 256 chars: rejected.
        let max = "a".repeat(255);
        let over = "a".repeat(256);
        assert!(image_ref(&max).is_ok());
        assert!(image_ref(&over).is_err());
    }

    // ----- resource_limits ------------------------------------------------

    #[test]
    fn resource_limits_zero_allowed() {
        // Zero means "use default" — explicitly allowed.
        assert!(resource_limits(0, 0, 0, 0).is_ok());
    }

    #[test]
    fn resource_limits_typical_values() {
        assert!(resource_limits(2, 4096, 256, 600).is_ok());
    }

    #[rstest]
    #[case::cpus_over(MAX_CPUS + 1, 0, 0, 0)]
    #[case::memory_over(0, MAX_MEMORY_MB + 1, 0, 0)]
    #[case::pids_over(0, 0, MAX_PIDS + 1, 0)]
    #[case::timeout_over(0, 0, 0, MAX_TIMEOUT_SECONDS + 1)]
    fn resource_limits_rejects_over_cap(
        #[case] cpus: u32,
        #[case] memory_mb: u32,
        #[case] pids_max: u32,
        #[case] timeout_seconds: u64,
    ) {
        assert!(resource_limits(cpus, memory_mb, pids_max, timeout_seconds).is_err());
    }

    #[test]
    fn resource_limits_at_cap_allowed() {
        // Exactly-at-cap is allowed; off-by-one mistakes would catch this.
        assert!(resource_limits(MAX_CPUS, MAX_MEMORY_MB, MAX_PIDS, MAX_TIMEOUT_SECONDS).is_ok());
    }

    // ----- volume_name ----------------------------------------------------

    #[rstest]
    #[case::alphanumeric("data")]
    #[case::with_dash("build-cache")]
    #[case::with_underscore("shared_state")]
    #[case::mixed("agent-1_logs")]
    fn volume_name_accepts_valid(#[case] input: &str) {
        assert!(volume_name(input).is_ok());
    }

    #[rstest]
    #[case::empty("")]
    #[case::slash("../escape")]
    #[case::space("my volume")]
    #[case::dot("my.volume")]
    #[case::null("vol\0name")]
    fn volume_name_rejects_invalid(#[case] input: &str) {
        assert!(volume_name(input).is_err());
    }

    #[test]
    fn volume_name_length_boundary() {
        let max = "a".repeat(64);
        let over = "a".repeat(65);
        assert!(volume_name(&max).is_ok());
        assert!(volume_name(&over).is_err());
    }

    // ----- entity_id ------------------------------------------------------

    #[rstest]
    #[case::full_uuid("550e8400-e29b-41d4-a716-446655440000")]
    #[case::no_hyphens("550e8400e29b41d4a716446655440000")]
    #[case::short_hex("deadbeef")]
    fn entity_id_accepts_valid(#[case] input: &str) {
        assert!(entity_id(input, "sandbox").is_ok());
    }

    #[rstest]
    #[case::empty("")]
    #[case::path_traversal("../etc")]
    #[case::non_hex("not-a-uuid-zzzz")]
    #[case::too_long("a".repeat(37))]
    fn entity_id_rejects_invalid(#[case] input: String) {
        assert!(entity_id(&input, "volume").is_err());
    }

    #[test]
    fn entity_id_error_message_uses_entity_label() {
        // The message includes the entity name passed in, so logs are useful.
        let err = entity_id("", "snapshot").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("snapshot"), "got: {msg}");
    }

    // ----- exec_command ---------------------------------------------------

    #[test]
    fn exec_command_accepts_single_arg() {
        assert!(exec_command(&["ls".to_string()]).is_ok());
    }

    #[test]
    fn exec_command_accepts_multi_arg() {
        assert!(exec_command(&["ls".to_string(), "-la".to_string(), "/tmp".to_string()]).is_ok());
    }

    #[test]
    fn exec_command_rejects_empty_slice() {
        assert!(exec_command(&[]).is_err());
    }

    #[test]
    fn exec_command_rejects_empty_executable() {
        // `["", "arg"]` is invalid — there's no program to run.
        assert!(exec_command(&[String::new(), "arg".to_string()]).is_err());
    }

    // ----- language_name --------------------------------------------------

    #[rstest]
    #[case("python")]
    #[case("node")]
    #[case("go_run")]
    fn language_name_accepts_valid(#[case] input: &str) {
        assert!(language_name(input).is_ok());
    }

    #[rstest]
    #[case::empty("")]
    #[case::with_dash("go-run")] // dash not allowed
    #[case::with_space("go run")]
    #[case::null("ruby\0")]
    fn language_name_rejects_invalid(#[case] input: &str) {
        assert!(language_name(input).is_err());
    }

    // ----- topic_name -----------------------------------------------------

    #[rstest]
    #[case::single_segment("results")]
    #[case::two_segments("agent.results")]
    #[case::nested("agent.results.build")]
    #[case::with_dash("agent-1.events")]
    #[case::with_underscore("agent_1.events")]
    fn topic_name_accepts_valid(#[case] input: &str) {
        assert!(topic_name(input).is_ok());
    }

    #[rstest]
    #[case::empty("")]
    #[case::leading_dot(".events")]
    #[case::trailing_dot("events.")]
    #[case::consecutive_dots("events..build")]
    #[case::space("agent results")]
    #[case::slash("agent/results")]
    fn topic_name_rejects_invalid(#[case] input: &str) {
        assert!(topic_name(input).is_err());
    }

    #[test]
    fn topic_name_length_boundary() {
        let max = "a".repeat(MAX_TOPIC_LEN);
        let over = "a".repeat(MAX_TOPIC_LEN + 1);
        assert!(topic_name(&max).is_ok());
        assert!(topic_name(&over).is_err());
    }

    // ----- group_name -----------------------------------------------------

    #[rstest]
    #[case("build-team")]
    #[case("agents_v2")]
    #[case("PROD")]
    fn group_name_accepts_valid(#[case] input: &str) {
        assert!(group_name(input).is_ok());
    }

    #[rstest]
    #[case::empty("")]
    #[case::space("two words")]
    #[case::dot("namespaced.group")] // dots are for topics, not groups
    fn group_name_rejects_invalid(#[case] input: &str) {
        assert!(group_name(input).is_err());
    }

    #[test]
    fn group_name_length_boundary() {
        let max = "g".repeat(MAX_GROUP_LEN);
        let over = "g".repeat(MAX_GROUP_LEN + 1);
        assert!(group_name(&max).is_ok());
        assert!(group_name(&over).is_err());
    }

    // ----- publish_payload ------------------------------------------------

    #[test]
    fn publish_payload_accepts_empty() {
        // Empty payloads are valid — useful for "ping" messages.
        assert!(publish_payload(&[]).is_ok());
    }

    #[test]
    fn publish_payload_accepts_at_cap() {
        let exactly_at_cap = vec![0u8; MAX_PUBLISH_PAYLOAD_BYTES];
        assert!(publish_payload(&exactly_at_cap).is_ok());
    }

    #[test]
    fn publish_payload_rejects_over_cap() {
        let over = vec![0u8; MAX_PUBLISH_PAYLOAD_BYTES + 1];
        assert!(publish_payload(&over).is_err());
    }

    // ----- Error variant verification -------------------------------------

    #[test]
    fn all_validators_return_invalid_request_variant() {
        // Validators must surface failures as InvalidRequest so the gRPC
        // boundary maps them to Code::InvalidArgument. Returning Backend
        // or Internal would mis-classify the error as a server fault.
        let cases: Vec<ApiError> = vec![
            image_ref("").unwrap_err(),
            resource_limits(MAX_CPUS + 1, 0, 0, 0).unwrap_err(),
            volume_name("").unwrap_err(),
            entity_id("", "x").unwrap_err(),
            exec_command(&[]).unwrap_err(),
            language_name("").unwrap_err(),
            topic_name("").unwrap_err(),
            group_name("").unwrap_err(),
            publish_payload(&vec![0u8; MAX_PUBLISH_PAYLOAD_BYTES + 1]).unwrap_err(),
        ];
        for err in cases {
            assert_eq!(
                std::mem::discriminant(&err),
                std::mem::discriminant(&ApiError::InvalidRequest(String::new())),
                "validator returned wrong ApiError variant: {err}",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property-based tests
// ---------------------------------------------------------------------------
//
// The rstest example suite above pins specific concrete inputs. proptest
// generates random inputs from a strategy and tries to falsify a stated
// invariant. The two layers complement each other: examples document the
// API's behaviour on known cases; properties guard against edge cases the
// author did not enumerate.
//
// Every property is a one-way assertion ("inputs in X → result Y"). We do
// NOT use proptest to assert exhaustive equivalence with a reference
// implementation; the validators are simple enough that example tests
// cover the positive happy paths and these properties guard the negative
// invariants where the universe of bad inputs is too large to enumerate.

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    // ----- entity_id ------------------------------------------------------

    proptest! {
        /// Any string of 1..=36 chars from the LOWERCASE-hex+dash alphabet
        /// must pass. SEC-022: tightened from `[0-9a-fA-F-]` to lowercase
        /// only so the validator rejects uppercase UUIDs at the boundary
        /// rather than silently returning NotFound after a case-sensitive
        /// HashMap lookup.
        #[test]
        fn property_entity_id_accepts_any_lowercase_hex_or_dash_within_length(
            id in "[0-9a-f-]{1,36}"
        ) {
            prop_assert!(entity_id(&id, "sandbox").is_ok(), "rejected: {id:?}");
        }

        /// Any string of 1..=36 chars containing a non-hex non-dash char
        /// must be rejected. The `g-z` range is entirely outside hex AND
        /// outside dash, so a string drawn from it always contains at
        /// least one invalid character.
        #[test]
        fn property_entity_id_rejects_non_hex_non_dash_chars(
            id in "[g-z]{1,36}"
        ) {
            prop_assert!(entity_id(&id, "sandbox").is_err(), "accepted: {id:?}");
        }

        /// SEC-022: any string containing uppercase hex characters must
        /// be rejected (UUIDs are emitted lowercase; uppercase wouldn't
        /// match HashMap lookups and would silently NotFound without
        /// this guard).
        #[test]
        fn property_entity_id_rejects_uppercase_hex(
            id in "[A-F]{1,36}"
        ) {
            prop_assert!(entity_id(&id, "sandbox").is_err(), "accepted: {id:?}");
        }

        /// Any string longer than 36 chars must be rejected regardless of
        /// content (length check fires before char check).
        #[test]
        fn property_entity_id_rejects_length_over_36(
            id in "[0-9a-f-]{37,80}"
        ) {
            prop_assert!(entity_id(&id, "sandbox").is_err(), "accepted: {id:?}");
        }
    }

    // ----- language_name --------------------------------------------------

    proptest! {
        /// Any non-empty alphanumeric+underscore string is accepted.
        #[test]
        fn property_language_name_accepts_alphanumeric_underscore(
            name in "[a-zA-Z0-9_]{1,40}"
        ) {
            prop_assert!(language_name(&name).is_ok(), "rejected: {name:?}");
        }

        /// Any string containing one of these clearly-bad chars rejects.
        /// `-`, `.`, `/`, `+`, `=` are all outside the allowed alphabet,
        /// so a generated string with at least one of them must fail.
        #[test]
        fn property_language_name_rejects_disallowed_chars(
            prefix in "[a-zA-Z0-9_]{0,12}",
            bad in r"[\-./+=]",
            suffix in "[a-zA-Z0-9_]{0,12}",
        ) {
            let name = format!("{prefix}{bad}{suffix}");
            prop_assert!(language_name(&name).is_err(), "accepted: {name:?}");
        }
    }

    // ----- volume_name ----------------------------------------------------

    proptest! {
        /// Any non-empty alphanumeric+dash+underscore string up to 64 chars
        /// is accepted.
        #[test]
        fn property_volume_name_accepts_valid_alphabet(
            name in "[a-zA-Z0-9_-]{1,64}"
        ) {
            prop_assert!(volume_name(&name).is_ok(), "rejected: {name:?}");
        }

        /// Length > 64 always rejects.
        #[test]
        fn property_volume_name_rejects_length_over_64(
            name in "[a-zA-Z0-9_-]{65,100}"
        ) {
            prop_assert!(volume_name(&name).is_err(), "accepted: {name:?}");
        }
    }

    // ----- topic_name -----------------------------------------------------

    proptest! {
        /// Two-segment topics (SEG.SEG) with only allowed chars and no
        /// leading/trailing/consecutive dots must be accepted.
        #[test]
        fn property_topic_name_accepts_well_formed_dotted_segments(
            topic in "[a-zA-Z0-9_-]{1,8}\\.[a-zA-Z0-9_-]{1,8}"
        ) {
            prop_assert!(topic_name(&topic).is_ok(), "rejected: {topic:?}");
        }

        /// Any topic containing '..' must be rejected, regardless of
        /// position. The alphanumeric pre/post ensures the topic isn't
        /// rejected for some other reason first.
        #[test]
        fn property_topic_name_rejects_consecutive_dots(
            pre in "[a-zA-Z0-9]{1,8}",
            post in "[a-zA-Z0-9]{1,8}"
        ) {
            let topic = format!("{pre}..{post}");
            prop_assert!(topic_name(&topic).is_err(), "accepted: {topic:?}");
        }

        /// Length > 128 always rejects.
        #[test]
        fn property_topic_name_rejects_length_over_max(
            topic in "[a-z]{129,200}"
        ) {
            prop_assert!(topic_name(&topic).is_err(), "accepted: {topic:?}");
        }
    }

    // ----- image_ref security boundary ------------------------------------

    proptest! {
        /// Any image_ref containing one of the clearly-bad shell
        /// metacharacters must reject. We sample a subset of the
        /// FORBIDDEN list — covering the easy-to-express chars is enough
        /// for the property; the rstest suite handles edge cases like
        /// `\n` and `\0` that need explicit escapes.
        #[test]
        fn property_image_ref_rejects_shell_metacharacters(
            prefix in "[a-z0-9]{1,16}",
            meta in r"[;&|$<>(){}]",
            suffix in "[a-z0-9]{1,16}",
        ) {
            let img = format!("{prefix}{meta}{suffix}");
            prop_assert!(image_ref(&img).is_err(), "accepted: {img:?}");
        }

        /// Any image_ref containing '..' must reject (path-traversal guard).
        #[test]
        fn property_image_ref_rejects_path_traversal(
            prefix in "[a-z0-9]{0,16}",
            suffix in "[a-z0-9]{0,16}"
        ) {
            let img = format!("{prefix}..{suffix}");
            prop_assert!(image_ref(&img).is_err(), "accepted: {img:?}");
        }
    }

    // ----- mount paths ----------------------------------------------------

    #[test]
    fn given_allowed_source_path_when_mount_then_ok() {
        // Default allowlist covers /home/, /tmp/, /var/lib/ward/.
        for (source, target) in [
            ("/home/alice/data", "/mnt/data"),
            ("/tmp/build", "/build"),
            ("/var/lib/ward/cache", "/var/cache"),
        ] {
            assert!(
                mount(source, target, false, false).is_ok(),
                "{source:?} -> {target:?}"
            );
        }
    }

    #[test]
    fn given_arbitrary_host_path_when_allow_host_false_then_rejected() {
        // SEC-020: default-deny on sources outside the allowlist.
        assert!(matches!(
            mount("/srv/app/cache", "/var/cache", false, false),
            Err(ApiError::InvalidRequest(_))
        ));
    }

    #[test]
    fn given_arbitrary_host_path_when_allow_host_true_then_accepted() {
        // Operator opt-in lifts the source restriction.
        assert!(mount("/srv/app/cache", "/var/cache", false, true).is_ok());
    }

    #[test]
    fn given_sensitive_path_when_writable_then_rejected_even_with_opt_in() {
        // SEC-020: /etc, /proc, /sys etc. must be readonly even when
        // WARD_ALLOW_HOST_MOUNTS lifts the source allowlist.
        for path in ["/etc", "/proc", "/sys", "/dev", "/root", "/boot", "/usr"] {
            assert!(
                matches!(
                    mount(path, "/x", false, true),
                    Err(ApiError::InvalidRequest(_))
                ),
                "expected {path:?} (writable) to be rejected"
            );
        }
    }

    #[test]
    fn given_sensitive_path_when_readonly_and_opt_in_then_accepted() {
        for path in ["/etc", "/etc/passwd", "/proc", "/usr/lib"] {
            assert!(
                mount(path, "/x", true, true).is_ok(),
                "expected {path:?} readonly with opt-in to be accepted"
            );
        }
    }

    #[test]
    fn given_invalid_paths_when_mount_then_invalid_request() {
        // Structural rules fire before the allowlist; opt-in is true so
        // these inputs hit the empty/relative/traversal checks alone.
        let cases = [
            ("data", "/mnt/data"),            // relative source
            ("/data", "mnt/data"),            // relative target
            ("", "/mnt/data"),                // empty source
            ("/data", ""),                    // empty target
            ("/data", "/mnt/../../etc"),      // traversal in target
            ("/data/../secret", "/mnt/data"), // traversal in source
        ];
        for (source, target) in cases {
            assert!(
                matches!(
                    mount(source, target, false, true),
                    Err(ApiError::InvalidRequest(_))
                ),
                "expected rejection for {source:?} -> {target:?}",
            );
        }
    }

    // ----- publish_payload size cap ---------------------------------------

    proptest! {
        // Each case can allocate up to ~1 MiB. Cut cases sharply so CI
        // stays fast — the boundary is what matters and 32 random sizes
        // around it is more than enough.
        #![proptest_config(ProptestConfig { cases: 32, .. ProptestConfig::default() })]

        /// payload.len() <= MAX → accepted; payload.len() > MAX → rejected.
        /// Generated sizes straddle the boundary so both halves get hit.
        #[test]
        fn property_publish_payload_respects_size_cap(
            payload in prop::collection::vec(any::<u8>(), 0..=MAX_PUBLISH_PAYLOAD_BYTES + 1024)
        ) {
            let result = publish_payload(&payload);
            if payload.len() <= MAX_PUBLISH_PAYLOAD_BYTES {
                prop_assert!(result.is_ok(), "rejected len {}", payload.len());
            } else {
                prop_assert!(result.is_err(), "accepted len {}", payload.len());
            }
        }
    }
}
