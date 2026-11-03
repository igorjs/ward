// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Which standard streams `ward run -a` attaches, as Docker's `-a` does.

use anyhow::{Result, bail};
use ward_core::pb::AttachStream;

const STDIN: &str = "STDIN";
const STDOUT: &str = "STDOUT";
const STDERR: &str = "STDERR";

/// The streams to attach. Without `-a` all of them are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Streams {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

impl Streams {
    pub const ALL: Self = Self {
        stdin: true,
        stdout: true,
        stderr: true,
    };

    /// # Errors
    ///
    /// Fails on a name other than STDIN, STDOUT or STDERR.
    pub fn parse(names: &[String]) -> Result<Self> {
        if names.is_empty() {
            return Ok(Self::ALL);
        }
        let mut streams = Self {
            stdin: false,
            stdout: false,
            stderr: false,
        };
        for name in names {
            match name.to_ascii_uppercase().as_str() {
                STDIN => streams.stdin = true,
                STDOUT => streams.stdout = true,
                STDERR => streams.stderr = true,
                _ => bail!(
                    "invalid stream specified with -a flag. Valid streams are {STDIN}, {STDOUT}, and {STDERR}"
                ),
            }
        }
        Ok(streams)
    }

    /// Whether output from `stream` is shown.
    pub fn shows(&self, stream: AttachStream) -> bool {
        match stream {
            AttachStream::Stdout => self.stdout,
            AttachStream::Stderr => self.stderr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn given_no_names_when_parsed_then_every_stream_is_attached() {
        assert_eq!(Streams::parse(&[]).unwrap(), Streams::ALL);
    }

    #[test]
    fn given_names_when_parsed_then_only_those_streams_are_attached() {
        let streams = Streams::parse(&names(&["stdout", "STDIN"])).unwrap();

        assert_eq!(
            streams,
            Streams {
                stdin: true,
                stdout: true,
                stderr: false
            }
        );
    }

    #[test]
    fn given_unknown_name_when_parsed_then_the_error_names_the_valid_ones() {
        let err = Streams::parse(&names(&["stdlog"])).unwrap_err();

        assert!(
            err.to_string().contains("STDIN, STDOUT, and STDERR"),
            "{err}"
        );
    }

    #[test]
    fn given_stdout_only_when_stderr_output_arrives_then_it_is_hidden() {
        let streams = Streams::parse(&names(&["STDOUT"])).unwrap();

        assert!(streams.shows(AttachStream::Stdout));
        assert!(!streams.shows(AttachStream::Stderr));
    }
}
