// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward pull IMAGE`: prints the daemon's progress lines like `docker pull`.

use anyhow::{Result, bail};
use ward_core::pb::PullImageRequest;

use crate::client;

/// The only OS Ward runs.
const GUEST_OS: &str = "linux";
const ARM64_VARIANT: &str = "v8";

#[derive(clap::Args, Debug)]
pub struct Flags {
    /// Platform to pull, OS/ARCH. Ward runs images for the host only: linux
    /// on this machine's architecture.
    #[arg(long, value_name = "OS/ARCH")]
    pub platform: Option<String>,
    /// Pull every tag of the repository. Ward does not support it.
    #[arg(short = 'a', long)]
    pub all_tags: bool,
}

/// This machine's architecture in OCI names.
fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

impl Flags {
    /// Refuses what Ward cannot do before anything is pulled.
    pub fn check(&self) -> Result<()> {
        if self.all_tags {
            bail!("--all-tags is not supported: pull one tag at a time");
        }
        let Some(platform) = &self.platform else {
            return Ok(());
        };
        let mut parts = platform.split('/');
        let (os, arch, variant) = (parts.next(), parts.next(), parts.next());
        let variant_ok = variant.is_none_or(|v| host_arch() == "arm64" && v == ARM64_VARIANT);
        if os != Some(GUEST_OS)
            || arch != Some(host_arch())
            || !variant_ok
            || parts.next().is_some()
        {
            bail!(
                "platform {platform} is not supported: Ward runs {GUEST_OS}/{} only",
                host_arch()
            );
        }
        Ok(())
    }
}

/// Pulls `reference`. With `quiet`, only the image reference is printed once
/// the pull is done.
pub async fn run(socket: &str, reference: &str, quiet: bool) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let request = PullImageRequest {
        reference: reference.to_string(),
    };
    let mut stream = c.pull_image(request).await?.into_inner();
    while let Some(line) = stream.message().await? {
        if !quiet {
            println!("{}", line_text(&line.id, &line.status));
        }
    }
    if quiet {
        println!("{reference}");
    }
    Ok(())
}

/// `id: status` for a line about a layer or tag, the bare status otherwise.
fn line_text(id: &str, status: &str) -> String {
    if id.is_empty() {
        status.to_string()
    } else {
        format!("{id}: {status}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(platform: Option<&str>, all_tags: bool) -> Flags {
        Flags {
            platform: platform.map(String::from),
            all_tags,
        }
    }

    #[test]
    fn given_the_host_platform_when_checked_then_it_passes() {
        let host = format!("{GUEST_OS}/{}", host_arch());

        assert!(flags(Some(&host), false).check().is_ok());
        assert!(flags(None, false).check().is_ok());
    }

    #[rstest::rstest]
    #[case::other_os(Some("windows/amd64"), false, "not supported")]
    #[case::other_arch(Some("linux/s390x"), false, "not supported")]
    #[case::garbage(Some("linux"), false, "not supported")]
    #[case::all_tags(None, true, "--all-tags")]
    fn given_an_unsupported_flag_when_checked_then_the_message_says_why(
        #[case] platform: Option<&str>,
        #[case] all_tags: bool,
        #[case] expected: &str,
    ) {
        let error = flags(platform, all_tags).check().expect_err("refused");

        assert!(error.to_string().contains(expected), "{error}");
    }
}
