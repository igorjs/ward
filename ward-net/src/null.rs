// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! No-op network backend. Sandboxes attached via this backend have no
//! network egress and no port forwarding. Useful for the stub-backend
//! test path and for users who want hard isolation
//! (`WARD_NETWORK_BACKEND=none`).

use crate::{AttachId, AttachOptions, Error, NetworkBackend};

#[derive(Debug, Default)]
pub struct NullBackend;

#[async_trait::async_trait]
impl NetworkBackend for NullBackend {
    fn name(&self) -> &'static str {
        "none"
    }

    async fn probe(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn attach(&self, sandbox_id: &str, _opts: &AttachOptions) -> Result<AttachId, Error> {
        // Return the sandbox id verbatim — detach is a no-op anyway,
        // so the AttachId only needs to be stable per attach call.
        Ok(format!("none:{sandbox_id}"))
    }

    async fn detach(&self, _attach_id: &AttachId) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn given_null_backend_when_attach_detach_then_idempotent() {
        let b = NullBackend;
        let id = b.attach("sb-1", &AttachOptions::default()).await.unwrap();
        assert_eq!(id, "none:sb-1");
        b.detach(&id).await.unwrap();
        // Second detach is fine — null backend is trivially idempotent.
        b.detach(&id).await.unwrap();
    }

    #[tokio::test]
    async fn given_null_backend_when_probe_then_ok() {
        NullBackend.probe().await.unwrap();
    }
}
