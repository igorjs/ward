// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Socket path resolution for the ward CLI.
//!
//! Lives in its own module so the resolution logic can be unit-tested
//! without spinning up clap or a tokio runtime.
//!
//! The pure decision logic is in `resolve()`, which takes its environment
//! inputs as parameters. The thin wrapper `default_socket()` reads the
//! real env. This split avoids the env-var race that would otherwise
//! happen when multiple tests run in parallel and mutate process globals.

/// Build the platform-default socket path from parameterised env inputs.
///
/// Precedence:
///   1. `ward_socket_override` (the `WARD_SOCKET` env var if set)
///   2. macOS: `<home>/.ward/ward.sock`
///   3. Linux with XDG_RUNTIME_DIR: `<xdg>/ward/ward.sock`
///   4. Linux without XDG_RUNTIME_DIR: `/tmp/ward-<user>/ward.sock`
///   5. Other platforms: `/tmp/ward.sock`
pub fn resolve(
    ward_socket_override: Option<&str>,
    home: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    user: Option<&str>,
) -> String {
    if let Some(s) = ward_socket_override {
        return s.to_string();
    }

    #[cfg(target_os = "macos")]
    {
        let _ = (xdg_runtime_dir, user); // silence unused warning on this branch
        let home = home.unwrap_or("/tmp");
        format!("{home}/.ward/ward.sock")
    }

    #[cfg(target_os = "linux")]
    {
        let _ = home; // silence unused warning on this branch
        if let Some(xdg) = xdg_runtime_dir {
            format!("{xdg}/ward/ward.sock")
        } else {
            let user = user.unwrap_or("ward");
            format!("/tmp/ward-{user}/ward.sock")
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (home, xdg_runtime_dir, user);
        "/tmp/ward.sock".to_string()
    }
}

/// Resolve the daemon's Unix socket path from the process environment.
pub fn default_socket() -> String {
    let ward_socket = std::env::var("WARD_SOCKET").ok();
    let home = std::env::var("HOME").ok();
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    let user = std::env::var("USER").ok();
    resolve(
        ward_socket.as_deref(),
        home.as_deref(),
        xdg.as_deref(),
        user.as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Tests
//
// BDD/AAA style: function names read as `given_X_when_Y_then_Z`, bodies
// have explicit Arrange / Act / Assert markers.
//
// Tests call the pure `resolve()` directly, never `default_socket()`, so
// they do not touch process-global env vars and can run in parallel.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn given_ward_socket_override_when_resolve_then_returns_override() {
        // Arrange
        let override_val = Some("/custom/path/ward.sock");

        // Act
        let result = resolve(override_val, Some("/home/x"), Some("/run/x"), Some("x"));

        // Assert: explicit override wins over every other input
        assert_eq!(result, "/custom/path/ward.sock");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn given_macos_with_home_when_resolve_then_returns_home_ward_sock() {
        // Arrange
        let home = Some("/Users/test");

        // Act
        let result = resolve(None, home, None, None);

        // Assert
        assert_eq!(result, "/Users/test/.ward/ward.sock");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn given_macos_without_home_when_resolve_then_falls_back_to_tmp() {
        // Arrange: HOME missing — should not panic, should produce something
        // workable rather than blow up the CLI.

        // Act
        let result = resolve(None, None, None, None);

        // Assert
        assert_eq!(result, "/tmp/.ward/ward.sock");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn given_linux_with_xdg_runtime_dir_when_resolve_then_uses_xdg() {
        // Arrange
        let xdg = Some("/run/user/1000");

        // Act
        let result = resolve(None, Some("/home/x"), xdg, Some("x"));

        // Assert: XDG_RUNTIME_DIR is preferred over /tmp on Linux
        assert_eq!(result, "/run/user/1000/ward/ward.sock");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn given_linux_without_xdg_with_user_when_resolve_then_uses_tmp_with_user() {
        // Arrange: XDG missing but USER set

        // Act
        let result = resolve(None, Some("/home/x"), None, Some("alice"));

        // Assert
        assert_eq!(result, "/tmp/ward-alice/ward.sock");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn given_linux_without_xdg_and_without_user_when_resolve_then_uses_tmp_with_ward() {
        // Arrange: both XDG and USER missing — fall back to a fixed name.

        // Act
        let result = resolve(None, None, None, None);

        // Assert
        assert_eq!(result, "/tmp/ward-ward/ward.sock");
    }
}
