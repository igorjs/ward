// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

mod server;
pub use server::WardGrpcServer;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
