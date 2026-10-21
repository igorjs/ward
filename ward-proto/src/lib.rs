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

    mod attach {
        use super::super::pb::{
            AttachData, AttachExit, AttachMessage, AttachMode, AttachResize, AttachSignal,
            AttachStart, AttachStdin, attach_message::Kind,
        };
        use prost::Message;

        const SIGTERM: i32 = 15;

        fn round_trip(kind: Kind) -> Kind {
            let msg = AttachMessage { kind: Some(kind) };
            AttachMessage::decode(msg.encode_to_vec().as_slice())
                .expect("attach message must decode")
                .kind
                .expect("kind must survive")
        }

        #[test]
        fn given_each_attach_member_when_round_tripped_then_member_and_value_match() {
            // Arrange
            let members = vec![
                Kind::Start(AttachStart {
                    sandbox_id: "sb".into(),
                    ..Default::default()
                }),
                Kind::Stdin(AttachStdin {
                    data: b"in".to_vec(),
                }),
                Kind::Resize(AttachResize { rows: 40, cols: 80 }),
                Kind::Signal(AttachSignal { signo: SIGTERM }),
                Kind::Data(AttachData {
                    data: b"out\0".to_vec(),
                }),
                Kind::Exit(AttachExit { exit_code: 7 }),
            ];

            for member in members {
                // Act
                let decoded = round_trip(member.clone());

                // Assert
                assert_eq!(decoded, member);
            }
        }

        #[test]
        fn given_empty_stdin_when_round_tripped_then_it_stays_an_stdin_member_with_no_bytes() {
            // Arrange
            let eof = Kind::Stdin(AttachStdin::default());

            // Act
            let decoded = round_trip(eof);

            // Assert
            assert_eq!(decoded, Kind::Stdin(AttachStdin { data: Vec::new() }));
        }

        #[test]
        fn given_start_with_every_field_when_round_tripped_then_every_field_has_its_value() {
            // Arrange
            let start = AttachStart {
                sandbox_id: "sb".into(),
                mode: AttachMode::Exec as i32,
                command: vec!["sh".into(), "-c".into(), "id".into()],
                tty: true,
                env: [("K".to_string(), "V".to_string())].into(),
                workdir: "/work".into(),
                user: "1000:1000".into(),
                replay: true,
                follow: true,
            };

            // Act
            let decoded = match round_trip(Kind::Start(start.clone())) {
                Kind::Start(s) => s,
                other => panic!("expected start, got {other:?}"),
            };

            // Assert
            assert_eq!(decoded, start);
        }
    }
}
