// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Resource limits and supplementary groups that every process the agent
//! starts gets, the main process and each exec alike.

/// Argument that sets a limit: `--ulimit <name> <soft> <hard>`.
pub const ULIMIT_ARG: &str = "--ulimit";
/// Argument that adds a group: `--group-add <name|gid>`.
pub const GROUP_ADD_ARG: &str = "--group-add";

/// The value for no limit.
const UNLIMITED: i64 = -1;

/// One resource limit. `None` is unlimited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestUlimit {
    pub name: String,
    pub soft: Option<u64>,
    pub hard: Option<u64>,
}

impl GuestUlimit {
    /// Read the three words of `--ulimit`.
    ///
    /// # Errors
    ///
    /// Fails on a limit name the guest does not know or a value that is not
    /// a number.
    pub fn parse(name: &str, soft: &str, hard: &str) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        if resource_of(name).is_none() {
            return Err(format!("unknown ulimit {name:?}"));
        }
        let value = |text: &str| -> Result<Option<u64>, String> {
            let number: i64 = text
                .parse()
                .map_err(|_| format!("ulimit {name}: {text:?} is not a number"))?;
            if number == UNLIMITED {
                Ok(None)
            } else {
                u64::try_from(number)
                    .map(Some)
                    .map_err(|_| format!("ulimit {name}: {text:?} is below -1"))
            }
        };
        Ok(Self {
            name: name.to_string(),
            soft: value(soft)?,
            hard: value(hard)?,
        })
    }
}

#[cfg(target_os = "linux")]
fn resource_of(name: &str) -> Option<rustix::process::Resource> {
    use rustix::process::Resource;
    Some(match name {
        "core" => Resource::Core,
        "cpu" => Resource::Cpu,
        "data" => Resource::Data,
        "fsize" => Resource::Fsize,
        "locks" => Resource::Locks,
        "memlock" => Resource::Memlock,
        "msgqueue" => Resource::Msgqueue,
        "nice" => Resource::Nice,
        "nofile" => Resource::Nofile,
        "nproc" => Resource::Nproc,
        "rss" => Resource::Rss,
        "rtprio" => Resource::Rtprio,
        "rttime" => Resource::Rttime,
        "sigpending" => Resource::Sigpending,
        "stack" => Resource::Stack,
        _ => return None,
    })
}

/// The limits as the kernel takes them.
#[cfg(target_os = "linux")]
pub fn kernel_limits(
    limits: &[GuestUlimit],
) -> Vec<(rustix::process::Resource, rustix::process::Rlimit)> {
    limits
        .iter()
        .filter_map(|l| {
            let resource = resource_of(&l.name)?;
            let limit = rustix::process::Rlimit {
                current: l.soft,
                maximum: l.hard,
            };
            Some((resource, limit))
        })
        .collect()
}

/// Set every limit on the calling process. Safe between fork and exec: only
/// system calls, no allocation.
#[cfg(target_os = "linux")]
pub fn apply(
    limits: &[(rustix::process::Resource, rustix::process::Rlimit)],
) -> std::io::Result<()> {
    for (resource, limit) in limits {
        rustix::process::setrlimit(*resource, *limit).map_err(std::io::Error::from)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_numbers_and_minus_one_when_parsed_then_minus_one_is_unlimited() {
        // Act
        let limit = GuestUlimit::parse("nofile", "1024", "-1").unwrap();

        // Assert
        assert_eq!((limit.soft, limit.hard), (Some(1024), None));
    }

    #[rstest::rstest]
    #[case::not_a_number("nofile", "many", "1")]
    #[case::below_minus_one("nofile", "-2", "-2")]
    #[cfg_attr(target_os = "linux", case::unknown_name("files", "1", "1"))]
    fn given_a_bad_limit_when_parsed_then_it_is_refused(
        #[case] name: &str,
        #[case] soft: &str,
        #[case] hard: &str,
    ) {
        assert!(GuestUlimit::parse(name, soft, hard).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn given_every_name_the_daemon_accepts_when_mapped_then_the_guest_knows_it() {
        for name in [
            "core",
            "cpu",
            "data",
            "fsize",
            "locks",
            "memlock",
            "msgqueue",
            "nice",
            "nofile",
            "nproc",
            "rss",
            "rtprio",
            "rttime",
            "sigpending",
            "stack",
        ] {
            assert!(resource_of(name).is_some(), "{name}");
        }
    }
}
