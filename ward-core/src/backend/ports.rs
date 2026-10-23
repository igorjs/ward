// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Host listeners that forward into a sandbox.

use std::net::SocketAddr;
use std::sync::Arc;

use super::{Backend, Result};

/// Why a sandbox without a microVM cannot publish ports.
pub const NEEDS_MICROVM: &str = "port publishing needs a microVM";

/// An open host listener. Dropping it closes the port soon after;
/// [`close`](OpenPort::close) returns once the port is free.
#[async_trait::async_trait]
pub trait OpenPort: Send + Sync {
    /// The address bound, carrying the real port when 0 was asked for.
    fn local_addr(&self) -> SocketAddr;

    async fn close(self: Box<Self>);
}

pub type PortListener = Box<dyn OpenPort>;

/// Opens host listeners for sandboxes. The manager talks to this so tests
/// can stand in for the network stack.
#[async_trait::async_trait]
pub trait PortForwarder: Send + Sync {
    async fn open(&self, id: &str, bind: SocketAddr, guest_port: u16) -> Result<PortListener>;
}

/// Forwards through the backend that runs the sandbox.
pub struct BackendForwarder(pub Arc<dyn Backend>);

#[async_trait::async_trait]
impl PortForwarder for BackendForwarder {
    async fn open(&self, id: &str, bind: SocketAddr, guest_port: u16) -> Result<PortListener> {
        self.0.publish_port(id, bind, guest_port).await
    }
}
