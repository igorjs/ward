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
