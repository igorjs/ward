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
