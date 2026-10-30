// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Process count limit for the sandbox, enforced with a cgroup v2 pids
//! controller. Every process the agent spawns joins one cgroup, so a fork
//! past the cap fails with EAGAIN inside the guest and the VM stays up.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where the guest kernel exposes the unified cgroup hierarchy.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Child cgroup that holds every spawned process.
const SANDBOX_GROUP: &str = "ward";
const CONTROLLERS_FILE: &str = "cgroup.controllers";
const SUBTREE_CONTROL_FILE: &str = "cgroup.subtree_control";
const PROCS_FILE: &str = "cgroup.procs";
const PIDS_MAX_FILE: &str = "pids.max";
const PIDS_CONTROLLER: &str = "pids";
const ENABLE_PIDS: &str = "+pids";
/// Writing pid 0 to `cgroup.procs` moves the writing process itself.
const SELF_PID: &[u8] = b"0";

/// Why the cap could not be set up. The agent logs it and runs unlimited.
#[derive(Debug)]
pub enum CgroupError {
    /// The guest kernel has no cgroup v2 hierarchy with a pids controller.
    Unsupported(String),
    /// A cgroup file operation failed.
    Io {
        step: &'static str,
        source: io::Error,
    },
}

impl fmt::Display for CgroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(why) => write!(f, "guest kernel cannot limit pids: {why}"),
            Self::Io { step, source } => write!(f, "pids limit setup failed at {step}: {source}"),
        }
    }
}

impl std::error::Error for CgroupError {}

fn io_step(step: &'static str) -> impl FnOnce(io::Error) -> CgroupError {
    move |source| CgroupError::Io { step, source }
}

/// Handle on the sandbox cgroup. Clones share one `cgroup.procs` descriptor.
#[derive(Clone)]
pub struct PidsCgroup {
    procs: Arc<OwnedFd>,
}

impl PidsCgroup {
    /// Move the calling process into the cgroup. Safe between fork and
    /// exec: one `write(2)`, no allocation.
    pub fn join_self(&self) -> io::Result<()> {
        rustix::io::write(&*self.procs, SELF_PID)
            .map(drop)
            .map_err(io::Error::from)
    }
}

/// Mount the cgroup v2 hierarchy at `root` unless it is already there.
#[cfg(target_os = "linux")]
pub fn ensure_mounted(root: &Path) -> io::Result<()> {
    if root.join(CONTROLLERS_FILE).exists() {
        return Ok(());
    }
    std::fs::create_dir_all(root)?;
    rustix::mount::mount(
        "cgroup2",
        root,
        "cgroup2",
        rustix::mount::MountFlags::empty(),
        None,
    )
    .map_err(io::Error::from)
}

/// Create the sandbox cgroup under `root` with a cap of `pids_max`.
///
/// # Errors
///
/// [`CgroupError::Unsupported`] when `root` is not a cgroup v2 hierarchy
/// with the pids controller; [`CgroupError::Io`] when the kernel refuses a
/// step.
pub fn setup(root: &Path, pids_max: u32) -> Result<PidsCgroup, CgroupError> {
    let controllers = std::fs::read_to_string(root.join(CONTROLLERS_FILE)).map_err(|e| {
        CgroupError::Unsupported(format!("no cgroup v2 at {}: {e}", root.display()))
    })?;
    if !controllers.split_whitespace().any(|c| c == PIDS_CONTROLLER) {
        return Err(CgroupError::Unsupported(
            "the pids controller is not built in".to_string(),
        ));
    }
    write_file(&root.join(SUBTREE_CONTROL_FILE), ENABLE_PIDS)
        .map_err(io_step("enable pids controller"))?;

    let group: PathBuf = root.join(SANDBOX_GROUP);
    match std::fs::create_dir(&group) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io_step("create cgroup")(e)),
    }
    write_file(&group.join(PIDS_MAX_FILE), &pids_max.to_string())
        .map_err(io_step("write pids.max"))?;
    let procs = OpenOptions::new()
        .write(true)
        .open(group.join(PROCS_FILE))
        .map_err(io_step("open cgroup.procs"))?;
    Ok(PidsCgroup {
        procs: Arc::new(procs.into()),
    })
}

fn write_file(path: &Path, text: &str) -> io::Result<()> {
    let mut file: File = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake cgroup root: the files the kernel would provide.
    fn fake_root(controllers: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(CONTROLLERS_FILE), controllers).unwrap();
        std::fs::write(root.path().join(SUBTREE_CONTROL_FILE), "").unwrap();
        let group = root.path().join(SANDBOX_GROUP);
        std::fs::create_dir(&group).unwrap();
        std::fs::write(group.join(PROCS_FILE), "").unwrap();
        std::fs::write(group.join(PIDS_MAX_FILE), "max").unwrap();
        root
    }

    #[test]
    fn given_pids_controller_when_setup_then_cap_and_subtree_are_written() {
        let root = fake_root("cpu memory pids\n");

        let cgroup = setup(root.path(), 20);

        assert!(cgroup.is_ok());
        let group = root.path().join(SANDBOX_GROUP);
        assert_eq!(
            std::fs::read_to_string(group.join(PIDS_MAX_FILE)).unwrap(),
            "20"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(SUBTREE_CONTROL_FILE)).unwrap(),
            ENABLE_PIDS
        );
    }

    #[test]
    fn given_no_pids_controller_when_setup_then_unsupported() {
        let root = fake_root("cpu memory\n");

        let err = setup(root.path(), 20).err().expect("must fail");

        assert!(matches!(err, CgroupError::Unsupported(_)), "{err}");
        assert!(err.to_string().contains("pids controller"));
    }

    #[test]
    fn given_no_cgroup_v2_when_setup_then_unsupported() {
        let root = tempfile::tempdir().unwrap();

        let err = setup(root.path(), 20).err().expect("must fail");

        assert!(matches!(err, CgroupError::Unsupported(_)), "{err}");
    }

    #[test]
    fn given_existing_group_when_setup_twice_then_it_succeeds() {
        let root = fake_root("pids");

        assert!(setup(root.path(), 20).is_ok());
        assert!(setup(root.path(), 30).is_ok());

        let group = root.path().join(SANDBOX_GROUP);
        assert_eq!(
            std::fs::read_to_string(group.join(PIDS_MAX_FILE)).unwrap(),
            "30"
        );
    }

    #[test]
    fn given_cgroup_when_join_self_then_pid_zero_is_written_to_procs() {
        let root = fake_root("pids");
        let cgroup = setup(root.path(), 20).unwrap();

        cgroup.join_self().unwrap();

        let procs = root.path().join(SANDBOX_GROUP).join(PROCS_FILE);
        assert_eq!(std::fs::read(procs).unwrap(), SELF_PID);
    }
}
