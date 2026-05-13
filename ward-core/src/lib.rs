// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

pub mod backend;
pub mod config;
pub mod egress;
pub mod grpc;
pub mod protocol;
pub mod sandbox;
pub mod validate;
pub mod volume;

/// Generated protobuf types and gRPC service traits.
pub mod pb {
    tonic::include_proto!("ward.v1");
}
