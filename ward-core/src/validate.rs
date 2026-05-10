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
/// Must be non-empty, no path traversal, no shell metacharacters.
pub fn image_ref(image: &str) -> Result<(), ApiError> {
    if image.is_empty() {
        return Err(ApiError::InvalidRequest(
            "image reference must not be empty".into(),
        ));
    }
    if image.contains("..") {
        return Err(ApiError::InvalidRequest(
            "image reference must not contain '..'".into(),
        ));
    }
    // Reject shell metacharacters that could be exploited
    const FORBIDDEN: &[char] = &[
        ';', '&', '|', '$', '`', '\\', '\'', '"', '<', '>', '(', ')', '{', '}', '\n', '\r', '\0',
    ];
    if image.contains(FORBIDDEN) {
        return Err(ApiError::InvalidRequest(
            "image reference contains forbidden characters".into(),
        ));
    }
    if image.len() > 255 {
        return Err(ApiError::InvalidRequest(
            "image reference exceeds 255 characters".into(),
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

/// Validate a sandbox or volume ID (UUID format).
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
    if !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(ApiError::InvalidRequest(format!(
            "{entity} ID contains invalid characters"
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
