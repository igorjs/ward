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

/// Bag of environment-variable values used by `Config::from_values`.
/// Owned `Option<String>` so tests can pass explicit inputs without
/// touching the process env (which is global and parallel-unsafe).
#[derive(Debug, Clone, Default)]
pub struct ConfigEnv {
    pub ward_socket: Option<String>,
    pub ward_data_dir: Option<String>,
    pub ward_log_level: Option<String>,
    pub ward_max_sandboxes: Option<String>,
    pub ward_max_volumes: Option<String>,
    pub ward_max_cached_images: Option<String>,
    pub home: Option<String>,
    pub xdg_runtime_dir: Option<String>,
    pub user: Option<String>,
}

impl Config {
    /// Build configuration from environment variables, falling back to platform defaults.
    pub fn from_env() -> Self {
        Self::from_values(ConfigEnv {
            ward_socket: std::env::var("WARD_SOCKET").ok(),
            ward_data_dir: std::env::var("WARD_DATA_DIR").ok(),
            ward_log_level: std::env::var("WARD_LOG_LEVEL").ok(),
            ward_max_sandboxes: std::env::var("WARD_MAX_SANDBOXES").ok(),
            ward_max_volumes: std::env::var("WARD_MAX_VOLUMES").ok(),
            ward_max_cached_images: std::env::var("WARD_MAX_CACHED_IMAGES").ok(),
            home: std::env::var("HOME").ok(),
            xdg_runtime_dir: std::env::var("XDG_RUNTIME_DIR").ok(),
            user: std::env::var("USER").ok(),
        })
    }

    /// Pure version of `from_env`: builds a Config from explicit env values.
    /// Every field of `Config` is decided by inputs passed in here — no
    /// process-env access — so unit tests can drive every code path
    /// deterministically and in parallel.
    pub fn from_values(env: ConfigEnv) -> Self {
        let socket_path = env
            .ward_socket
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                default_socket_path(
                    env.home.as_deref(),
                    env.xdg_runtime_dir.as_deref(),
                    env.user.as_deref(),
                )
            });

        let data_dir = env
            .ward_data_dir
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| default_data_dir(env.home.as_deref()));

        let log_level = env.ward_log_level.unwrap_or_else(|| "info".to_string());

        let max_sandboxes = env
            .ward_max_sandboxes
            .as_deref()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);

        let max_volumes = env
            .ward_max_volumes
            .as_deref()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);

        let max_cached_images = env
            .ward_max_cached_images
            .as_deref()
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
    /// SEC-002/003/004: every daemon-managed directory is forced to
    /// mode 0700 via `fchmod` on a file descriptor opened with
    /// `O_NOFOLLOW | O_DIRECTORY`. This closes the TOCTOU window that
    /// the previous `symlink_metadata` + `set_permissions` pattern
    /// left open — `O_NOFOLLOW` errors on a symlink leaf, and
    /// `fchmod` operates on the fd, not the path, so even an attacker
    /// who races a symlink swap after the open call cannot redirect
    /// the chmod to the symlink's target (e.g. `/etc`).
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        if let Some(parent) = self.socket_path.parent() {
            create_dir_owner_only(parent)?;
        }
        create_dir_owner_only(&self.data_dir)?;
        for sub in ["images", "sandboxes", "snapshots", "volumes"] {
            create_dir_owner_only(&self.data_dir.join(sub))?;
        }
        Ok(())
    }
}

/// Create `path` recursively with owner-only permissions (0700) and
/// no-symlink-follow semantics on Unix. On non-Unix targets falls back
/// to `create_dir_all` without the chmod step.
///
/// Algorithm:
///   1. `create_dir_all` materialises any missing intermediate dirs.
///   2. Open the leaf with `O_NOFOLLOW | O_DIRECTORY` — errors if the
///      leaf is a symlink. Holding the fd defeats post-open path swaps.
///   3. `fchmod(fd, 0o700)` — operates on the fd, not the path, so a
///      racing symlink swap cannot redirect the chmod.
fn create_dir_owner_only(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, open};
        let fd = open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .map_err(|e: rustix::io::Errno| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "refusing to operate on path that is a symlink or non-directory: {} ({e})",
                    path.display()
                ),
            )
        })?;
        rustix::fs::fchmod(&fd, Mode::from_bits_truncate(0o700)).map_err(
            |e: rustix::io::Errno| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("fchmod 0o700 on {}: {e}", path.display()),
                )
            },
        )?;
    }
    Ok(())
}

fn default_socket_path(
    home: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    user: Option<&str>,
) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let _ = (xdg_runtime_dir, user);
        home_dir(home).join(".ward").join("ward.sock")
    }

    #[cfg(target_os = "linux")]
    {
        let _ = home;
        // Prefer XDG_RUNTIME_DIR, fall back to /tmp/ward-$USER.
        if let Some(xdg) = xdg_runtime_dir {
            PathBuf::from(xdg).join("ward").join("ward.sock")
        } else {
            let user = user.unwrap_or("ward");
            PathBuf::from(format!("/tmp/ward-{user}")).join("ward.sock")
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (xdg_runtime_dir, user);
        home_dir(home).join(".ward").join("ward.sock")
    }
}

fn default_data_dir(home: Option<&str>) -> PathBuf {
    home_dir(home).join(".ward").join("data")
}

/// Resolve the user's home directory. Panics if HOME is unset rather than
/// falling back to a world-writable location like /tmp, which would place
/// daemon state (socket, data) in an insecure directory.
fn home_dir(home: Option<&str>) -> PathBuf {
    PathBuf::from(
        home.expect("HOME environment variable must be set; cannot determine safe default paths"),
    )
}

// ---------------------------------------------------------------------------
// Tests
//
// BDD names with AAA bodies. Every test calls `Config::from_values()` with
// an explicit `ConfigEnv` — no process-env mutation, fully parallel-safe.
//
// Platform-dependent defaults (socket path on macOS vs Linux) are gated
// with #[cfg(target_os = ...)] so each test runs only on its native
// platform. macOS CI ignores Linux tests and vice versa.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Minimum ConfigEnv with HOME populated. Other fields default to None
    /// so each test only sets what it cares about, mirroring the way users
    /// rarely set every WARD_* var.
    fn env_with_home() -> ConfigEnv {
        ConfigEnv {
            home: Some("/home/test".to_string()),
            ..Default::default()
        }
    }

    // ----- WARD_SOCKET override ------------------------------------------

    #[test]
    fn given_ward_socket_set_when_from_values_then_uses_explicit_path() {
        // Arrange: explicit override + a HOME that would otherwise drive
        // the platform default. The override must win.
        let env = ConfigEnv {
            ward_socket: Some("/custom/path/ward.sock".into()),
            ..env_with_home()
        };

        // Act
        let cfg = Config::from_values(env);

        // Assert
        assert_eq!(cfg.socket_path, PathBuf::from("/custom/path/ward.sock"));
    }

    // ----- WARD_DATA_DIR override ----------------------------------------

    #[test]
    fn given_ward_data_dir_set_when_from_values_then_uses_explicit_path() {
        // Arrange
        let env = ConfigEnv {
            ward_data_dir: Some("/var/lib/ward".into()),
            ..env_with_home()
        };

        // Act
        let cfg = Config::from_values(env);

        // Assert
        assert_eq!(cfg.data_dir, PathBuf::from("/var/lib/ward"));
    }

    #[test]
    fn given_no_ward_data_dir_when_from_values_then_uses_home_ward_data() {
        // Arrange: no override, HOME drives the default.
        let env = env_with_home();

        // Act
        let cfg = Config::from_values(env);

        // Assert
        assert_eq!(cfg.data_dir, PathBuf::from("/home/test/.ward/data"));
    }

    // ----- WARD_LOG_LEVEL ------------------------------------------------

    #[test]
    fn given_no_log_level_when_from_values_then_defaults_to_info() {
        // Arrange
        let env = env_with_home();

        // Act
        let cfg = Config::from_values(env);

        // Assert: "info" is the documented default — flipping it silently
        // would dump terabytes of debug output on production daemons.
        assert_eq!(cfg.log_level, "info");
    }

    #[test]
    fn given_log_level_set_when_from_values_then_uses_provided_value() {
        // Arrange
        let env = ConfigEnv {
            ward_log_level: Some("ward=trace,tonic=debug".into()),
            ..env_with_home()
        };

        // Act
        let cfg = Config::from_values(env);

        // Assert: complex filter strings pass through verbatim.
        assert_eq!(cfg.log_level, "ward=trace,tonic=debug");
    }

    // ----- WARD_MAX_SANDBOXES -------------------------------------------

    #[test]
    fn given_no_max_sandboxes_when_from_values_then_defaults_to_256() {
        // Arrange
        let env = env_with_home();

        // Act
        let cfg = Config::from_values(env);

        // Assert: 256 is the documented default. Lowering this silently
        // would break users running large workloads.
        assert_eq!(cfg.max_sandboxes, 256);
    }

    #[test]
    fn given_max_sandboxes_set_to_42_when_from_values_then_uses_42() {
        // Arrange
        let env = ConfigEnv {
            ward_max_sandboxes: Some("42".into()),
            ..env_with_home()
        };

        // Act
        let cfg = Config::from_values(env);

        // Assert
        assert_eq!(cfg.max_sandboxes, 42);
    }

    #[test]
    fn given_max_sandboxes_unparseable_when_from_values_then_falls_back_to_default() {
        // Arrange: an env var set to a non-integer value. We do not want
        // the daemon to crash; falling back to the default is the right
        // behaviour for a config that the user probably typo'd.
        let env = ConfigEnv {
            ward_max_sandboxes: Some("not-a-number".into()),
            ..env_with_home()
        };

        // Act
        let cfg = Config::from_values(env);

        // Assert: fell back to 256 rather than panicking or returning 0.
        assert_eq!(cfg.max_sandboxes, 256);
    }

    // ----- WARD_MAX_VOLUMES ---------------------------------------------

    #[test]
    fn given_no_max_volumes_when_from_values_then_defaults_to_256() {
        let env = env_with_home();
        let cfg = Config::from_values(env);
        assert_eq!(cfg.max_volumes, 256);
    }

    #[test]
    fn given_max_volumes_set_when_from_values_then_uses_provided_value() {
        let env = ConfigEnv {
            ward_max_volumes: Some("17".into()),
            ..env_with_home()
        };
        let cfg = Config::from_values(env);
        assert_eq!(cfg.max_volumes, 17);
    }

    // ----- WARD_MAX_CACHED_IMAGES ---------------------------------------

    #[test]
    fn given_no_max_cached_images_when_from_values_then_defaults_to_64() {
        let env = env_with_home();
        let cfg = Config::from_values(env);
        assert_eq!(cfg.max_cached_images, 64);
    }

    #[test]
    fn given_max_cached_images_set_when_from_values_then_uses_provided_value() {
        let env = ConfigEnv {
            ward_max_cached_images: Some("128".into()),
            ..env_with_home()
        };
        let cfg = Config::from_values(env);
        assert_eq!(cfg.max_cached_images, 128);
    }

    // ----- Platform-specific socket defaults -----------------------------

    #[cfg(target_os = "macos")]
    #[test]
    fn given_macos_with_home_when_from_values_then_socket_under_home_ward() {
        // Arrange
        let env = env_with_home();

        // Act
        let cfg = Config::from_values(env);

        // Assert
        assert_eq!(cfg.socket_path, PathBuf::from("/home/test/.ward/ward.sock"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn given_linux_with_xdg_runtime_dir_when_from_values_then_socket_in_xdg() {
        // Arrange
        let env = ConfigEnv {
            xdg_runtime_dir: Some("/run/user/1000".into()),
            ..env_with_home()
        };

        // Act
        let cfg = Config::from_values(env);

        // Assert: XDG_RUNTIME_DIR is preferred on Linux — it's user-only
        // and securely permissioned by systemd-logind.
        assert_eq!(
            cfg.socket_path,
            PathBuf::from("/run/user/1000/ward/ward.sock")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn given_linux_without_xdg_with_user_when_from_values_then_socket_in_tmp_with_user() {
        // Arrange: XDG missing but USER set.
        let env = ConfigEnv {
            user: Some("alice".into()),
            ..env_with_home()
        };

        // Act
        let cfg = Config::from_values(env);

        // Assert
        assert_eq!(cfg.socket_path, PathBuf::from("/tmp/ward-alice/ward.sock"));
    }

    // ----- HOME panic guard ----------------------------------------------

    #[test]
    #[should_panic(expected = "HOME environment variable")]
    fn given_no_home_when_from_values_then_panics_rather_than_using_tmp() {
        // Arrange: HOME is unset AND no WARD_DATA_DIR / WARD_SOCKET to
        // override. The daemon must NOT silently fall back to /tmp
        // because that places daemon state in a world-writable directory.
        let env = ConfigEnv::default();

        // Act: should panic during default_data_dir computation.
        let _cfg = Config::from_values(env);

        // Assert: panic expected by #[should_panic] attribute.
    }

    #[test]
    fn given_no_home_but_explicit_socket_and_data_dir_when_from_values_then_no_panic() {
        // Arrange: when HOME is unset but the user provided both
        // overrides explicitly, the daemon should not panic — there's no
        // need to compute a default path that requires HOME.
        let env = ConfigEnv {
            ward_socket: Some("/custom/ward.sock".into()),
            ward_data_dir: Some("/custom/data".into()),
            ..Default::default()
        };

        // Act
        let cfg = Config::from_values(env);

        // Assert
        assert_eq!(cfg.socket_path, PathBuf::from("/custom/ward.sock"));
        assert_eq!(cfg.data_dir, PathBuf::from("/custom/data"));
    }
}
