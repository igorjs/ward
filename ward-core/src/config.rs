// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

use std::path::PathBuf;

/// Daemon configuration, sourced from environment variables with sensible defaults.
#[derive(Debug, Clone)]
pub struct Config {
    /// Unix socket path the gRPC server will bind to.
    pub socket_path: PathBuf,
    /// Root data directory for sandbox state, volumes, snapshots, and images.
    pub data_dir: PathBuf,
    /// Log level filter string (e.g. "info", "debug", "ward=trace").
    pub log_level: String,
    /// Maximum number of concurrent sandboxes. Prevents resource exhaustion.
    pub max_sandboxes: usize,
    /// Maximum number of volumes. Prevents metadata and inode exhaustion.
    pub max_volumes: usize,
    /// Maximum number of cached OCI images. Prevents disk exhaustion.
    pub max_cached_images: usize,
}

impl Config {
    /// Build configuration from environment variables, falling back to platform defaults.
    pub fn from_env() -> Self {
        let socket_path = if let Ok(v) = std::env::var("WARD_SOCKET") {
            PathBuf::from(v)
        } else {
            default_socket_path()
        };

        let data_dir = if let Ok(v) = std::env::var("WARD_DATA_DIR") {
            PathBuf::from(v)
        } else {
            default_data_dir()
        };

        let log_level = std::env::var("WARD_LOG_LEVEL").unwrap_or_else(|_| "info".to_string());

        let max_sandboxes = std::env::var("WARD_MAX_SANDBOXES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);

        let max_volumes = std::env::var("WARD_MAX_VOLUMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);

        let max_cached_images = std::env::var("WARD_MAX_CACHED_IMAGES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(64);

        Self {
            socket_path,
            data_dir,
            log_level,
            max_sandboxes,
            max_volumes,
            max_cached_images,
        }
    }

    /// Ensure all required directories exist with secure permissions.
    ///
    /// The socket directory is set to 0700 (owner-only) to prevent other
    /// users from listing contents or connecting to the daemon socket.
    /// Data directories use the default umask since they contain daemon-managed
    /// data not directly accessible to external users.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
            // Restrict socket directory to owner-only access.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.data_dir.join("images"))?;
        std::fs::create_dir_all(self.data_dir.join("sandboxes"))?;
        std::fs::create_dir_all(self.data_dir.join("snapshots"))?;
        std::fs::create_dir_all(self.data_dir.join("volumes"))?;
        Ok(())
    }
}

fn default_socket_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home_dir().join(".ward").join("ward.sock")
    }

    #[cfg(target_os = "linux")]
    {
        // Prefer XDG_RUNTIME_DIR, fall back to /tmp/ward-$USER.
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            PathBuf::from(xdg).join("ward").join("ward.sock")
        } else {
            let user = std::env::var("USER").unwrap_or_else(|_| "ward".to_string());
            PathBuf::from(format!("/tmp/ward-{}", user)).join("ward.sock")
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        home_dir().join(".ward").join("ward.sock")
    }
}

fn default_data_dir() -> PathBuf {
    home_dir().join(".ward").join("data")
}

/// Resolve the user's home directory. Panics if HOME is unset rather than
/// falling back to a world-writable location like /tmp, which would place
/// daemon state (socket, data) in an insecure directory.
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .expect("HOME environment variable must be set; cannot determine safe default paths")
}
