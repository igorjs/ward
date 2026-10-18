// Copyright 2026 Ward Contributors. SPDX-License-Identifier: Apache-2.0

//! Protobuf types and tonic gRPC stubs for the ward sandbox daemon's
//! public wire protocol.
//!
//! This crate exists so that ward's AGPL-3.0 server-side workspace
//! (`ward-core`, `ward-daemon`, `ward-runtime`, `ward-mcp`) and its
//! Apache-2.0 SDK side (`sdks/rust/ward-client`, downstream consumers)
//! can share a single set of generated bindings without crossing the
//! license boundary. See:
//!
//! - [ADR-004](https://github.com/igorjs/ward/blob/main/docs/adr/004-cli-protocol.md):
//!   the protobuf schema itself is CC0.
//! - [ADR-017](https://github.com/igorjs/ward/blob/main/docs/adr/017-license-posture.md):
//!   why the SDK and server boundary lives at the proto layer.
//!
//! ## Usage
//!
//! ```ignore
//! use ward_proto::pb;
//!
//! let req = pb::CreateSandboxRequest {
//!     image: "alpine".into(),
//!     ..Default::default()
//! };
//! ```
//!
//! Server-side workspace crates re-export `pb` so call sites stay
//! `crate::pb::...`-shaped; see `ward-core/src/lib.rs`.

/// Generated protobuf messages, enums, and the tonic gRPC service +
/// client stubs for the `ward.v1` package.
pub mod pb {
    tonic::include_proto!("ward.v1");
}

#[cfg(test)]
mod tests {
    use super::pb::{CreateSandboxRequest, SandboxInfo, SandboxStatus};
    use prost::Message;

    #[test]
    fn given_create_request_with_tty_when_round_tripped_then_tty_is_true() {
        // Arrange
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            tty: true,
            ..Default::default()
        };

        // Act
        let decoded = CreateSandboxRequest::decode(req.encode_to_vec().as_slice())
            .expect("request must decode");

        // Assert
        assert!(decoded.tty);
    }

    #[test]
    fn given_sandbox_status_when_numbers_read_then_old_values_keep_and_new_ones_are_fresh() {
        // Arrange
        let old = [
            (SandboxStatus::Creating, 1),
            (SandboxStatus::Running, 2),
            (SandboxStatus::Stopped, 3),
            (SandboxStatus::Failed, 4),
        ];

        // Act
        let new = [SandboxStatus::Exited as i32, SandboxStatus::Stopping as i32];

        // Assert
        for (status, number) in old {
            assert_eq!(status as i32, number);
        }
        for number in new {
            assert!(number > 4, "new status must use a number not used before");
        }
        assert_ne!(new[0], new[1]);
    }

    #[test]
    fn given_exit_code_zero_when_round_tripped_then_presence_differs_from_never_exited() {
        // Arrange
        let exited_zero = SandboxInfo {
            exit_code: Some(0),
            ..Default::default()
        };
        let never_exited = SandboxInfo::default();

        // Act
        let decoded_zero =
            SandboxInfo::decode(exited_zero.encode_to_vec().as_slice()).expect("info must decode");
        let decoded_never =
            SandboxInfo::decode(never_exited.encode_to_vec().as_slice()).expect("info must decode");

        // Assert
        assert_eq!(decoded_zero.exit_code, Some(0));
        assert_eq!(decoded_never.exit_code, None);
    }
}
