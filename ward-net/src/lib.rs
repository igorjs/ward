// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward network backends.
//!
//! [`NetworkBackend`] is the trait every backend implements. Per
//! `docs/adr/020-smoltcp-only-networking.md`, ward ships two:
//!
//! - [`null::NullBackend`] — no-op. Sandbox has no network egress; the
//!   stub-backend tests use this and so does `WARD_NETWORK_BACKEND=none`.
//! - [`smoltcp_backend::SmoltcpBackend`] — in-process TCP/IP stack, the
//!   sole network-egress backend (`docs/adr/019-inprocess-smoltcp-networking.md`,
//!   `docs/adr/020-smoltcp-only-networking.md`). `passt` and `gvproxy`
//!   backends existed here previously; ADR-020 removed both.
//!
//! Backend selection in production lives in `ward-core` (or
//! `ward-runtime`) where the libkrun FD plumbing happens. This crate
//! intentionally keeps the FD glue out — it just describes *how to
//! build the rootless network attachment*, not where to plug it in.

use std::path::PathBuf;

pub mod null;
pub mod smoltcp_backend;

/// Errors surfaced by network backends.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The backend's external process failed to spawn or exited non-zero.
    #[error("network backend process error: {0}")]
    Process(String),

    /// The backend was asked to do something it does not implement (yet).
    #[error("network backend not implemented: {0}")]
    Unimplemented(String),
}

/// One forwarded port. `host` is the listener on the host machine;
/// `guest` is the in-VM port the listener forwards to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortMap {
    pub host: u16,
    pub guest: u16,
    pub protocol: Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

/// Config a backend needs to attach a sandbox.
#[derive(Debug, Clone, Default)]
pub struct AttachOptions {
    /// Ports to forward from host to guest.
    pub ports: Vec<PortMap>,
    /// Optional MAC address override for the virtio-net device. `None`
    /// means "let libkrun pick one".
    pub mac: Option<[u8; 6]>,
}

/// Identifier returned by [`NetworkBackend::attach`]. Opaque to callers;
/// backends use it to look up their own internal state when detaching.
pub type AttachId = String;

/// Trait every backend implements. Async since a future backend may
/// spawn a child process via tokio; smoltcp itself is sync but the
/// trait stays async so it isn't tied to one implementation's shape.
#[async_trait::async_trait]
pub trait NetworkBackend: Send + Sync {
    /// Short, log-friendly name for this backend (e.g. `"smoltcp"`).
    fn name(&self) -> &'static str;

    /// Check that the backend can actually run on this host. Always
    /// true for both current backends (`none` is a no-op, `smoltcp`
    /// runs in-process).
    async fn probe(&self) -> Result<(), Error>;

    /// Attach a sandbox to the network. Returns an opaque ID the caller
    /// hands back to `detach` later.
    async fn attach(&self, sandbox_id: &str, opts: &AttachOptions) -> Result<AttachId, Error>;

    /// Detach a sandbox previously attached with `attach`. Idempotent.
    async fn detach(&self, attach_id: &AttachId) -> Result<(), Error>;
}

/// Lookup a backend by name. Used by the daemon's startup config so
/// `WARD_NETWORK_BACKEND=smoltcp` works without compile-time changes.
pub fn backend_by_name(name: &str) -> Result<Box<dyn NetworkBackend>, Error> {
    match name {
        "none" => Ok(Box::new(null::NullBackend)),
        "smoltcp" => Ok(Box::new(smoltcp_backend::SmoltcpBackend)),
        other => Err(Error::Unimplemented(format!(
            "unknown network backend: {other} (known: none, smoltcp)"
        ))),
    }
}

/// Where ward looks for runtime sockets / FIFOs that backends create.
/// Defaults to `$WARD_DATA_DIR/net` if `data_dir` is provided, otherwise
/// `$TMPDIR/ward-net`.
pub fn runtime_dir(data_dir: Option<&std::path::Path>) -> PathBuf {
    match data_dir {
        Some(d) => d.join("net"),
        None => std::env::temp_dir().join("ward-net"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_known_backend_when_lookup_then_returns_instance() {
        let b = backend_by_name("none").expect("none backend exists");
        assert_eq!(b.name(), "none");
    }

    #[test]
    fn given_unknown_backend_when_lookup_then_errors() {
        // `dyn NetworkBackend` isn't Debug — pattern-match the Err arm
        // directly rather than going through unwrap_err / panic-on-Ok.
        match backend_by_name("nope") {
            Err(Error::Unimplemented(msg)) => assert!(msg.contains("nope")),
            Err(other) => panic!("expected Unimplemented, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[test]
    fn given_data_dir_when_runtime_dir_then_joins_under_net() {
        let d = std::path::Path::new("/var/lib/ward");
        assert_eq!(runtime_dir(Some(d)), PathBuf::from("/var/lib/ward/net"));
    }
}
