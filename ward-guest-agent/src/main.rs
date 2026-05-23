// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Ward guest agent entry point.
//!
//! Inside a microVM this is the process libkrun launches. It listens on a
//! vsock port and serves one connection per executed process (see the
//! library crate for the protocol). vsock is Linux-only, so the binary only
//! does real work there; on other hosts it exits with a clear message so the
//! crate still compiles for dev/test on macOS.

/// vsock port the agent listens on. The daemon connects to this port on the
/// guest's CID. Ports below 1024 are conventionally privileged; 1024 is the
/// first unprivileged port and avoids clashing with libkrun's implicit
/// services.
#[cfg(target_os = "linux")]
const AGENT_VSOCK_PORT: u32 = 1024;

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> std::io::Result<()> {
    use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, AGENT_VSOCK_PORT))?;
    tracing::info!(
        port = AGENT_VSOCK_PORT,
        "ward-guest-agent listening on vsock"
    );

    loop {
        let (stream, _peer) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = ward_guest_agent::handle_connection(stream).await {
                tracing::warn!(error = %e, "connection handler failed");
            }
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ward-guest-agent runs inside a Linux microVM; unsupported on this host");
    std::process::exit(1);
}
