// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Socket path resolution for the ward CLI.
//!
//! Lives in its own module so the resolution logic can be unit-tested
//! without spinning up clap or a tokio runtime. Each platform's default
//! mirrors `ward-core::config::Config`'s defaults so the CLI talks to the
//! same socket the daemon binds.

/// Resolve the daemon's Unix socket path.
///
/// Precedence: `WARD_SOCKET` env var, then platform default. macOS uses
/// `$HOME/.ward/ward.sock`; Linux prefers `$XDG_RUNTIME_DIR/ward/ward.sock`
/// and falls back to `/tmp/ward-$USER/ward.sock`. Other platforms get a
/// generic `/tmp/ward.sock`.
///
/// Returns a `String` (not `PathBuf`) because tonic's URI construction
/// needs string form anyway, and clap's `Option<String>` plays better
/// with strings on the input side.
pub fn default_socket() -> String {
    if let Ok(v) = std::env::var("WARD_SOCKET") {
        return v;
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        format!("{home}/.ward/ward.sock")
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            format!("{xdg}/ward/ward.sock")
        } else {
            let user = std::env::var("USER").unwrap_or_else(|_| "ward".to_string());
            format!("/tmp/ward-{user}/ward.sock")
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "/tmp/ward.sock".to_string()
    }
}
