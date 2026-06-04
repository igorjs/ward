// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! smoltcp backend — research scaffold.
//!
//! Per ADR-018, smoltcp is not on the v0.1 critical path. This module
//! exists so the [`crate::NetworkBackend`] trait shape covers all three
//! candidates uniformly and so future work has a deliberate starting
//! point (rather than discovering, six months from now, that smoltcp
//! needs a different trait surface than passt).
//!
//! What this scaffold does today:
//! - Compiles against `smoltcp` so we know the feature flag works.
//! - Implements `probe()` (smoltcp is in-process so probing always
//!   succeeds).
//! - `attach` / `detach` return `Error::Unimplemented` with a pointer at
//!   ADR-018's "Future work" section.
//!
//! What it does NOT do today: parse virtio-net frames, manage flows,
//! proxy bytes. All of that is future work.

use crate::{AttachId, AttachOptions, Error, NetworkBackend};

#[derive(Debug, Default)]
pub struct SmoltcpBackend;

#[async_trait::async_trait]
impl NetworkBackend for SmoltcpBackend {
    fn name(&self) -> &'static str {
        "smoltcp"
    }

    async fn probe(&self) -> Result<(), Error> {
        // smoltcp is in-process; nothing to probe. We do compile-check
        // that smoltcp's types are reachable so a future feature drift
        // surfaces at the right boundary.
        let _ = std::mem::size_of::<smoltcp::wire::IpAddress>();
        Ok(())
    }

    async fn attach(
        &self,
        _sandbox_id: &str,
        _opts: &AttachOptions,
    ) -> Result<AttachId, Error> {
        Err(Error::Unimplemented(
            "smoltcp backend: see docs/adr/018-rootless-networking.md \
             'Future work' for the planned implementation. Use \
             WARD_NETWORK_BACKEND=passt for now."
                .into(),
        ))
    }

    async fn detach(&self, _attach_id: &AttachId) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn given_scaffold_when_probe_then_ok() {
        SmoltcpBackend.probe().await.unwrap();
    }

    #[tokio::test]
    async fn given_scaffold_when_attach_then_unimplemented() {
        let err = SmoltcpBackend
            .attach("sb", &AttachOptions::default())
            .await
            .unwrap_err();
        match err {
            Error::Unimplemented(msg) => assert!(msg.contains("018")),
            other => panic!("expected Unimplemented, got {other:?}"),
        }
    }
}
