// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! passt(1) backend.
//!
//! Per ADR-018 this is ward's default rootless network backend. passt is
//! a userspace TCP/IP translator; ward spawns one per sandbox, hands
//! libkrun the resulting FD via `krun_set_passt_fd`, and lets passt
//! forward sandbox traffic onto the host's normal socket layer.
//!
//! This module owns three concerns:
//!
//! 1. **Probe** — confirm `passt` is on `$PATH`. Falls back with a
//!    clear `Error::DependencyMissing` hint pointing at `docs/rootless.md`.
//! 2. **Command-line construction** — translate [`AttachOptions::ports`]
//!    into the passt flags the daemon needs to exec. Pure function so
//!    it's unit-testable without spawning anything.
//! 3. **Lifecycle** — spawn + supervise + reap the passt subprocess.
//!    The FD plumbing into libkrun lives in `ward-core` (it needs the
//!    krun_ctx_id), so this crate exposes a `spawn_for_attach` that
//!    returns the FD; the caller injects it into libkrun.

use std::sync::RwLock;

use crate::{AttachId, AttachOptions, Error, NetworkBackend, Protocol};

/// Name of the passt binary we probe for.
const PASST_BIN: &str = "passt";

#[derive(Debug, Default)]
pub struct PasstBackend {
    /// Map of attach_id -> child pid. Lookup table so detach can SIGTERM
    /// the right passt process. RwLock is fine — attach/detach are not
    /// hot-path operations.
    children: RwLock<std::collections::HashMap<AttachId, u32>>,
}

#[async_trait::async_trait]
impl NetworkBackend for PasstBackend {
    fn name(&self) -> &'static str {
        "passt"
    }

    async fn probe(&self) -> Result<(), Error> {
        match which::which(PASST_BIN) {
            Ok(p) => {
                tracing::debug!(path = %p.display(), "passt binary found");
                Ok(())
            }
            Err(_) => Err(Error::DependencyMissing {
                what: format!(
                    "{PASST_BIN}(1) — install via your package manager \
                     (apt install passt / brew install passt) or see \
                     docs/rootless.md"
                ),
            }),
        }
    }

    async fn attach(&self, sandbox_id: &str, opts: &AttachOptions) -> Result<AttachId, Error> {
        // The actual FD-injection into libkrun lives in ward-core
        // because it needs the krun_ctx_id, which this crate does not
        // see. Real spawn is deferred to the integration layer; this
        // method records the attach so detach has something to find.
        // See ADR-018 "Implementation" for the planned flow.
        let attach_id = format!("passt:{sandbox_id}");
        let _argv = build_command_line(sandbox_id, opts);
        // Until the FD-injection layer lands, record a placeholder pid
        // (0) so the map shape is stable. The real spawn integration
        // will replace this with the actual child pid.
        self.children
            .write()
            .map_err(|e| Error::Process(format!("attach lock poisoned: {e}")))?
            .insert(attach_id.clone(), 0);
        Ok(attach_id)
    }

    async fn detach(&self, attach_id: &AttachId) -> Result<(), Error> {
        let pid = self
            .children
            .write()
            .map_err(|e| Error::Process(format!("detach lock poisoned: {e}")))?
            .remove(attach_id);
        // Idempotent: detaching an unknown id is fine. When the real
        // spawn lands, this is where we'd SIGTERM the captured pid.
        let _ = pid;
        Ok(())
    }
}

/// Pure command-line builder. Translates [`AttachOptions::ports`] into
/// the argv `passt` expects. Public so the daemon can render a debug
/// view of what would be spawned, and so the tests can pin the exact
/// arg order across passt versions.
pub fn build_command_line(sandbox_id: &str, opts: &AttachOptions) -> Vec<String> {
    let mut argv = vec![PASST_BIN.to_string()];

    // `-f` foreground — keep passt attached so ward owns lifecycle.
    argv.push("-f".to_string());

    // Per-sandbox PID file so concurrent ward sandboxes don't collide
    // and so an operator can `kill $(cat /tmp/ward-net/<id>.pid)` if
    // needed. The path scheme is informational here; actual file
    // creation belongs to the spawn layer.
    argv.push("--pid".to_string());
    argv.push(format!("/tmp/ward-net/{sandbox_id}.pid"));

    let tcp: Vec<String> = opts
        .ports
        .iter()
        .filter(|p| p.protocol == Protocol::Tcp)
        .map(|p| format!("{}:{}", p.host, p.guest))
        .collect();
    if !tcp.is_empty() {
        argv.push("--tcp-ports".to_string());
        argv.push(tcp.join(","));
    }

    let udp: Vec<String> = opts
        .ports
        .iter()
        .filter(|p| p.protocol == Protocol::Udp)
        .map(|p| format!("{}:{}", p.host, p.guest))
        .collect();
    if !udp.is_empty() {
        argv.push("--udp-ports".to_string());
        argv.push(udp.join(","));
    }

    if let Some(mac) = opts.mac {
        argv.push("--mac-addr".to_string());
        argv.push(format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        ));
    }

    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PortMap;

    #[test]
    fn given_no_options_when_build_then_minimal_argv() {
        let argv = build_command_line("sb-1", &AttachOptions::default());
        assert_eq!(argv[0], "passt");
        assert!(argv.contains(&"-f".to_string()));
        assert!(argv.iter().any(|a| a.ends_with("sb-1.pid")));
    }

    #[test]
    fn given_tcp_ports_when_build_then_includes_flag() {
        let opts = AttachOptions {
            ports: vec![
                PortMap {
                    host: 8080,
                    guest: 80,
                    protocol: Protocol::Tcp,
                },
                PortMap {
                    host: 8443,
                    guest: 443,
                    protocol: Protocol::Tcp,
                },
            ],
            ..Default::default()
        };
        let argv = build_command_line("sb-2", &opts);
        let i = argv.iter().position(|a| a == "--tcp-ports").unwrap();
        assert_eq!(argv[i + 1], "8080:80,8443:443");
    }

    #[test]
    fn given_udp_ports_when_build_then_includes_flag() {
        let opts = AttachOptions {
            ports: vec![PortMap {
                host: 5353,
                guest: 53,
                protocol: Protocol::Udp,
            }],
            ..Default::default()
        };
        let argv = build_command_line("sb-3", &opts);
        let i = argv.iter().position(|a| a == "--udp-ports").unwrap();
        assert_eq!(argv[i + 1], "5353:53");
    }

    #[test]
    fn given_mac_when_build_then_formats_as_colon_hex() {
        let opts = AttachOptions {
            mac: Some([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
            ..Default::default()
        };
        let argv = build_command_line("sb-4", &opts);
        let i = argv.iter().position(|a| a == "--mac-addr").unwrap();
        assert_eq!(argv[i + 1], "52:54:00:12:34:56");
    }

    #[tokio::test]
    async fn given_attach_then_detach_when_detach_again_then_idempotent() {
        let b = PasstBackend::default();
        let id = b.attach("sb-5", &AttachOptions::default()).await.unwrap();
        b.detach(&id).await.unwrap();
        // Detaching an unknown id is fine.
        b.detach(&id).await.unwrap();
    }
}
