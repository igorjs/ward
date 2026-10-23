// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Resolves a sandbox's main process from the request and the image config,
//! the way `docker run` does.

use std::collections::HashMap;

use crate::backend::image::ImageConfig;

/// What the main process starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    /// Entrypoint followed by the arguments. Empty means no main process.
    pub command: Vec<String>,
    /// How many leading entries of `command` came from an Entrypoint.
    pub entrypoint_len: usize,
    pub workdir: String,
    /// The user to run as: the request's, else the image's `User`.
    pub user: String,
    pub env: HashMap<String, String>,
}

/// The request fields that override the image config.
pub(crate) struct Overrides<'a> {
    /// `Some("")` clears the image Entrypoint.
    pub entrypoint: Option<&'a str>,
    pub args: &'a [String],
    pub workdir: &'a str,
    pub user: &'a str,
    pub env: &'a HashMap<String, String>,
}

/// Docker's rules: an explicit entrypoint replaces the image's and drops the
/// image Cmd, and arguments replace the image Cmd.
pub(crate) fn resolve(image: &ImageConfig, given: Overrides<'_>) -> Launch {
    let entrypoint: Vec<String> = match given.entrypoint {
        Some("") => Vec::new(),
        Some(program) => vec![program.to_string()],
        None => image.entrypoint.clone(),
    };
    let cmd = if !given.args.is_empty() {
        given.args.to_vec()
    } else if given.entrypoint.is_some() {
        Vec::new()
    } else {
        image.cmd.clone()
    };
    let entrypoint_len = entrypoint.len();
    let command = [entrypoint, cmd].concat();

    let workdir = if given.workdir.is_empty() {
        image.working_dir.clone()
    } else {
        given.workdir.to_string()
    };
    let mut env: HashMap<String, String> = image
        .env
        .iter()
        .filter_map(|entry| entry.split_once('='))
        // An empty key or a NUL byte would make every exec in the sandbox fail.
        .filter(|(key, value)| !key.is_empty() && !key.contains('\0') && !value.contains('\0'))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    env.extend(given.env.clone());
    let user = if given.user.is_empty() {
        image.user.clone()
    } else {
        given.user.to_string()
    };

    Launch {
        command,
        entrypoint_len,
        workdir,
        user,
        env,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn image(entrypoint: &[&str], cmd: &[&str]) -> ImageConfig {
        ImageConfig {
            entrypoint: strings(entrypoint),
            cmd: strings(cmd),
            ..ImageConfig::default()
        }
    }

    fn resolve_command(
        image: &ImageConfig,
        entrypoint: Option<&str>,
        args: &[&str],
    ) -> (Vec<String>, usize) {
        let args = strings(args);
        let launch = resolve(
            image,
            Overrides {
                entrypoint,
                args: &args,
                workdir: "",
                user: "",
                env: &HashMap::new(),
            },
        );
        (launch.command, launch.entrypoint_len)
    }

    #[rstest::rstest]
    #[case::cmd_only(&[], &["/bin/sh"], None, &[], &["/bin/sh"], 0)]
    #[case::args_replace_cmd(&[], &["/bin/sh"], None, &["ls"], &["ls"], 0)]
    #[case::entrypoint_and_cmd(&["/e"], &["a"], None, &[], &["/e", "a"], 1)]
    #[case::args_replace_cmd_after_entrypoint(&["/e"], &["a"], None, &["b"], &["/e", "b"], 1)]
    #[case::override_drops_image_cmd(&["/e"], &["a"], Some("/x"), &[], &["/x"], 1)]
    #[case::override_keeps_given_args(&["/e"], &["a"], Some("/x"), &["b"], &["/x", "b"], 1)]
    #[case::empty_override_clears_entrypoint(&["/e"], &["a"], Some(""), &["ls"], &["ls"], 0)]
    #[case::empty_override_drops_image_cmd(&["/e"], &["a"], Some(""), &[], &[], 0)]
    #[case::nothing_at_all(&[], &[], None, &[], &[], 0)]
    fn given_image_config_when_resolving_then_argv_follows_docker(
        #[case] entrypoint: &[&str],
        #[case] cmd: &[&str],
        #[case] override_entrypoint: Option<&str>,
        #[case] args: &[&str],
        #[case] expected: &[&str],
        #[case] expected_entrypoint_len: usize,
    ) {
        // Arrange
        let config = image(entrypoint, cmd);

        // Act
        let (command, entrypoint_len) = resolve_command(&config, override_entrypoint, args);

        // Assert
        assert_eq!(command, strings(expected));
        assert_eq!(entrypoint_len, expected_entrypoint_len);
    }

    #[test]
    fn given_no_workdir_when_resolving_then_image_working_dir_applies() {
        // Arrange
        let config = ImageConfig {
            working_dir: "/app".to_string(),
            ..ImageConfig::default()
        };
        let none = HashMap::new();
        let request = |workdir| Overrides {
            entrypoint: None,
            args: &[],
            workdir,
            user: "",
            env: &none,
        };

        // Act
        let defaulted = resolve(&config, request(""));
        let given = resolve(&config, request("/tmp"));

        // Assert
        assert_eq!(defaulted.workdir, "/app");
        assert_eq!(given.workdir, "/tmp");
    }

    #[test]
    fn given_image_env_when_resolving_then_request_env_wins() {
        // Arrange
        let config = ImageConfig {
            env: strings(&["A=image", "B=image", "malformed"]),
            ..ImageConfig::default()
        };
        let given = HashMap::from([("B".to_string(), "user".to_string())]);

        // Act
        let launch = resolve(
            &config,
            Overrides {
                entrypoint: None,
                args: &[],
                workdir: "",
                user: "",
                env: &given,
            },
        );

        // Assert
        assert_eq!(
            launch.env,
            HashMap::from([
                ("A".to_string(), "image".to_string()),
                ("B".to_string(), "user".to_string()),
            ])
        );
    }

    #[test]
    fn given_image_env_with_unusable_keys_when_resolving_then_they_are_dropped() {
        // Arrange
        let config = ImageConfig {
            env: strings(&["=empty", "BAD\0KEY=x", "GOOD=1"]),
            ..ImageConfig::default()
        };
        let none = HashMap::new();

        // Act
        let launch = resolve(
            &config,
            Overrides {
                entrypoint: None,
                args: &[],
                workdir: "",
                user: "",
                env: &none,
            },
        );

        // Assert
        assert_eq!(
            launch.env,
            HashMap::from([("GOOD".to_string(), "1".to_string())])
        );
    }

    #[test]
    fn given_image_user_when_resolving_then_launch_carries_it() {
        // Arrange
        let config = ImageConfig {
            user: "app".to_string(),
            ..ImageConfig::default()
        };
        let none = HashMap::new();

        // Act
        let launch = resolve(
            &config,
            Overrides {
                entrypoint: None,
                args: &[],
                workdir: "",
                user: "",
                env: &none,
            },
        );

        // Assert
        assert_eq!(launch.user, "app");
    }

    #[rstest::rstest]
    #[case::request_wins("app", "1000:1000", "1000:1000")]
    #[case::image_default("app", "", "app")]
    #[case::none("", "", "")]
    fn given_a_requested_user_when_resolving_then_it_overrides_the_image(
        #[case] image_user: &str,
        #[case] requested: &str,
        #[case] expected: &str,
    ) {
        // Arrange
        let config = ImageConfig {
            user: image_user.to_string(),
            ..ImageConfig::default()
        };
        let none = HashMap::new();

        // Act
        let launch = resolve(
            &config,
            Overrides {
                entrypoint: None,
                args: &[],
                workdir: "",
                user: requested,
                env: &none,
            },
        );

        // Assert
        assert_eq!(launch.user, expected);
    }
}
