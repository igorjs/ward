// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `--user` support: resolve `name|uid[:group|gid]` against the guest's
//! `/etc/passwd` and `/etc/group` the way Docker does, then drop privileges.

use std::ffi::CStr;
use std::fmt;

/// Passwd and group fields are split on this.
const FIELD_SEPARATOR: char = ':';
/// Separates the members in a group entry.
const MEMBER_SEPARATOR: char = ',';
/// Separates the user from the group in a spec.
const SPEC_SEPARATOR: char = ':';
/// Docker's gid for a numeric uid that has no passwd entry.
const DEFAULT_GID: u32 = 0;
/// Docker's HOME for a user without a passwd home.
const DEFAULT_HOME: &str = "/";

const PASSWD_UID_FIELD: usize = 2;
const PASSWD_GID_FIELD: usize = 3;
const PASSWD_HOME_FIELD: usize = 5;
const PASSWD_MIN_FIELDS: usize = 6;
const GROUP_GID_FIELD: usize = 2;
const GROUP_MEMBERS_FIELD: usize = 3;
const GROUP_MIN_FIELDS: usize = 3;

/// Why a user spec could not be turned into an identity.
#[derive(Debug, PartialEq, Eq)]
pub enum UserError {
    Invalid(String),
    UserNotFound(String),
    GroupNotFound(String),
    /// The named database file is over the size limit.
    FileTooLarge(&'static str),
}

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(spec) => write!(f, "invalid user spec {spec:?}"),
            Self::UserNotFound(user) => write!(
                f,
                "unable to find user {user}: no matching entries in passwd file"
            ),
            Self::GroupNotFound(group) => write!(
                f,
                "unable to find group {group}: no matching entries in group file"
            ),
            Self::FileTooLarge(file) => write!(f, "{file} file too large"),
        }
    }
}

impl std::error::Error for UserError {}

/// Who a process runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    /// The gid first, then, for a spec without a group, the groups of
    /// `/etc/group` that list the user.
    pub groups: Vec<u32>,
    pub home: String,
}

/// A user or group named by number or by name.
#[derive(Debug, PartialEq, Eq)]
enum Reference<'a> {
    Id(u32),
    Name(&'a str),
}

impl<'a> Reference<'a> {
    fn parse(text: &'a str) -> Self {
        text.parse().map_or(Self::Name(text), Self::Id)
    }
}

impl fmt::Display for Reference<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(id) => write!(f, "{id}"),
            Self::Name(name) => f.write_str(name),
        }
    }
}

struct PasswdEntry<'a> {
    name: &'a str,
    uid: u32,
    gid: u32,
    home: &'a str,
}

struct GroupEntry<'a> {
    name: &'a str,
    gid: u32,
    members: &'a str,
}

fn passwd_entries(passwd: &str) -> impl Iterator<Item = PasswdEntry<'_>> {
    passwd.lines().filter_map(|line| {
        let fields: Vec<&str> = line.split(FIELD_SEPARATOR).collect();
        if fields.len() < PASSWD_MIN_FIELDS {
            return None;
        }
        Some(PasswdEntry {
            name: fields[0],
            uid: fields[PASSWD_UID_FIELD].parse().ok()?,
            gid: fields[PASSWD_GID_FIELD].parse().ok()?,
            home: fields[PASSWD_HOME_FIELD],
        })
    })
}

fn group_entries(group: &str) -> impl Iterator<Item = GroupEntry<'_>> {
    group.lines().filter_map(|line| {
        let fields: Vec<&str> = line.split(FIELD_SEPARATOR).collect();
        if fields.len() < GROUP_MIN_FIELDS {
            return None;
        }
        Some(GroupEntry {
            name: fields[0],
            gid: fields[GROUP_GID_FIELD].parse().ok()?,
            members: fields.get(GROUP_MEMBERS_FIELD).copied().unwrap_or(""),
        })
    })
}

/// Split a spec into its user and optional group, rejecting empty parts.
fn split_spec(spec: &str) -> Result<(Reference<'_>, Option<Reference<'_>>), UserError> {
    let invalid = || UserError::Invalid(spec.to_string());
    let (user, group) = match spec.split_once(SPEC_SEPARATOR) {
        Some((user, group)) => (user, Some(group)),
        None => (spec, None),
    };
    let unusable = |part: &str| part.is_empty() || part.contains(SPEC_SEPARATOR);
    if unusable(user) || group.is_some_and(unusable) {
        return Err(invalid());
    }
    Ok((Reference::parse(user), group.map(Reference::parse)))
}

/// Resolve `spec` against the contents of `/etc/passwd` and `/etc/group`.
///
/// A numeric uid without a passwd entry is accepted, with gid 0 unless a
/// group is given, as Docker does. A name that is missing is an error.
pub fn resolve(spec: &str, passwd: &str, group: &str) -> Result<Identity, UserError> {
    let (user_ref, group_ref) = split_spec(spec)?;
    let entry = passwd_entries(passwd).find(|entry| match user_ref {
        Reference::Id(uid) => entry.uid == uid,
        Reference::Name(name) => entry.name == name,
    });
    let (uid, default_gid) = match (&entry, &user_ref) {
        (Some(entry), _) => (entry.uid, entry.gid),
        (None, Reference::Id(uid)) => (*uid, DEFAULT_GID),
        (None, Reference::Name(_)) => return Err(UserError::UserNotFound(user_ref.to_string())),
    };
    let gid = match group_ref {
        None => default_gid,
        Some(Reference::Id(gid)) => gid,
        Some(Reference::Name(name)) => group_entries(group)
            .find(|entry| entry.name == name)
            .map(|entry| entry.gid)
            .ok_or_else(|| UserError::GroupNotFound(name.to_string()))?,
    };

    // Root's groups must not leak through, so the list is always set. As in
    // Docker, the groups that list the user are added only when no group was
    // given.
    let mut groups = vec![gid];
    if let (Some(entry), None) = (&entry, &group_ref) {
        for candidate in group_entries(group) {
            let is_member = candidate
                .members
                .split(MEMBER_SEPARATOR)
                .any(|member| member == entry.name);
            if is_member && !groups.contains(&candidate.gid) {
                groups.push(candidate.gid);
            }
        }
    }
    let home = entry
        .map(|entry| entry.home)
        .filter(|home| !home.is_empty())
        .unwrap_or(DEFAULT_HOME)
        .to_string();
    Ok(Identity {
        uid,
        gid,
        groups,
        home,
    })
}

/// The system calls that change who a process is and where it starts.
pub trait PrivilegeDropper {
    fn set_groups(&self, groups: &[u32]) -> std::io::Result<()>;
    fn set_gid(&self, gid: u32) -> std::io::Result<()>;
    fn set_uid(&self, uid: u32) -> std::io::Result<()>;
    fn change_dir(&self, dir: &CStr) -> std::io::Result<()>;
}

/// Drop to `identity`, then enter `workdir`. The directory is entered last
/// so the new user's permissions decide whether it can be reached.
pub fn enter_as(
    dropper: &impl PrivilegeDropper,
    identity: &Identity,
    workdir: Option<&CStr>,
) -> std::io::Result<()> {
    drop_privileges(dropper, identity)?;
    workdir.map_or(Ok(()), |dir| {
        dropper.change_dir(dir).map_err(|e| tag(Step::ChangeDir, e))
    })
}

/// Drop to `identity`: groups first, then gid, then uid, because a process
/// that has given up root can no longer change its groups. The first failure
/// stops the sequence and the caller must not run the program.
pub fn drop_privileges(
    dropper: &impl PrivilegeDropper,
    identity: &Identity,
) -> std::io::Result<()> {
    dropper
        .set_groups(&identity.groups)
        .map_err(|e| tag(Step::SetGroups, e))?;
    dropper
        .set_gid(identity.gid)
        .map_err(|e| tag(Step::SetGid, e))?;
    dropper
        .set_uid(identity.uid)
        .map_err(|e| tag(Step::SetUid, e))
}

/// The step of the privilege drop that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    SetGroups = 1,
    SetGid,
    SetUid,
    ChangeDir,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Self::SetGroups => "setgroups",
            Self::SetGid => "setgid",
            Self::SetUid => "setuid",
            Self::ChangeDir => "chdir",
        }
    }

    const ALL: [Step; 4] = [Self::SetGroups, Self::SetGid, Self::SetUid, Self::ChangeDir];
}

/// The step number lives above the errno bits of the error code.
const STEP_SHIFT: u32 = 16;
const ERRNO_MASK: i32 = (1 << STEP_SHIFT) - 1;

/// Mark `err` with the failed `step`. The standard library hands a
/// `pre_exec` failure to the parent as its bare OS error code, so the step
/// rides in the high bits of that code and costs no allocation in the child.
fn tag(step: Step, err: std::io::Error) -> std::io::Error {
    match err.raw_os_error() {
        Some(errno) => std::io::Error::from_raw_os_error(((step as i32) << STEP_SHIFT) | errno),
        None => err,
    }
}

/// Undo [`tag`]: the failed step, if marked, and the plain OS error.
pub fn untag(err: std::io::Error) -> (Option<Step>, std::io::Error) {
    let Some(code) = err.raw_os_error() else {
        return (None, err);
    };
    let step = Step::ALL
        .into_iter()
        .find(|s| *s as i32 == code >> STEP_SHIFT);
    match step {
        Some(step) => (
            Some(step),
            std::io::Error::from_raw_os_error(code & ERRNO_MASK),
        ),
        None => (None, err),
    }
}

/// The real calls, on Linux. Elsewhere there is nothing safe to do, so
/// every call fails and the process never starts.
pub struct SystemDropper;

#[cfg(target_os = "linux")]
impl PrivilegeDropper for SystemDropper {
    fn set_groups(&self, groups: &[u32]) -> std::io::Result<()> {
        // SAFETY: the pointer and length describe a live slice.
        check(unsafe { libc::setgroups(groups.len(), groups.as_ptr()) })
    }

    fn set_gid(&self, gid: u32) -> std::io::Result<()> {
        // SAFETY: setgid takes a plain integer.
        check(unsafe { libc::setgid(gid) })
    }

    fn set_uid(&self, uid: u32) -> std::io::Result<()> {
        // SAFETY: setuid takes a plain integer.
        check(unsafe { libc::setuid(uid) })
    }

    fn change_dir(&self, dir: &CStr) -> std::io::Result<()> {
        // SAFETY: `dir` is a valid NUL-terminated string.
        check(unsafe { libc::chdir(dir.as_ptr()) })
    }
}

#[cfg(target_os = "linux")]
fn check(status: libc::c_int) -> std::io::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
impl PrivilegeDropper for SystemDropper {
    fn set_groups(&self, _groups: &[u32]) -> std::io::Result<()> {
        Err(unsupported())
    }

    fn set_gid(&self, _gid: u32) -> std::io::Result<()> {
        Err(unsupported())
    }

    fn set_uid(&self, _uid: u32) -> std::io::Result<()> {
        Err(unsupported())
    }

    fn change_dir(&self, _dir: &CStr) -> std::io::Result<()> {
        Err(unsupported())
    }
}

#[cfg(not(target_os = "linux"))]
fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "switching user needs a Linux guest",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::CString;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\n\
                          app:x:1000:1000::/home/app:/bin/sh\n\
                          nohome:x:2000:3000:::/bin/sh\n\
                          # comment\n\
                          broken\n";
    const GROUP: &str = "root:x:0:root\n\
                         app:x:1000:\n\
                         staff:x:50:app,other\n\
                         video:x:44:other\n\
                         ops:x:60:nohome,app\n";

    fn identity(uid: u32, gid: u32, groups: &[u32], home: &str) -> Identity {
        Identity {
            uid,
            gid,
            groups: groups.to_vec(),
            home: home.to_string(),
        }
    }

    #[rstest::rstest]
    #[case::uid("1000", Reference::Id(1000), None)]
    #[case::uid_gid("1000:2000", Reference::Id(1000), Some(Reference::Id(2000)))]
    #[case::name("app", Reference::Name("app"), None)]
    #[case::name_group("app:staff", Reference::Name("app"), Some(Reference::Name("staff")))]
    #[case::name_gid("app:50", Reference::Name("app"), Some(Reference::Id(50)))]
    fn given_valid_spec_when_split_then_user_and_group_are_parsed(
        #[case] spec: &str,
        #[case] user: Reference<'_>,
        #[case] group: Option<Reference<'_>>,
    ) {
        // Act
        let parsed = split_spec(spec);

        // Assert
        assert_eq!(parsed, Ok((user, group)));
    }

    #[rstest::rstest]
    #[case::empty("")]
    #[case::empty_user(":1000")]
    #[case::empty_group("1000:")]
    #[case::three_parts("a:b:c")]
    #[case::only_colon(":")]
    fn given_invalid_spec_when_split_then_it_is_rejected(#[case] spec: &str) {
        // Act
        let parsed = split_spec(spec);

        // Assert
        assert_eq!(parsed, Err(UserError::Invalid(spec.to_string())));
    }

    #[rstest::rstest]
    #[case::uid_with_entry("1000", identity(1000, 1000, &[1000, 50, 60], "/home/app"))]
    #[case::uid_gid("1000:1000", identity(1000, 1000, &[1000], "/home/app"))]
    #[case::name("app", identity(1000, 1000, &[1000, 50, 60], "/home/app"))]
    #[case::name_group("app:staff", identity(1000, 50, &[50], "/home/app"))]
    #[case::name_gid("app:44", identity(1000, 44, &[44], "/home/app"))]
    #[case::root("root", identity(0, 0, &[0], "/root"))]
    #[case::uid_without_entry("4242", identity(4242, 0, &[0], "/"))]
    #[case::uid_without_entry_with_gid("4242:77", identity(4242, 77, &[77], "/"))]
    #[case::empty_home_falls_back("nohome", identity(2000, 3000, &[3000, 60], "/"))]
    fn given_passwd_and_group_when_resolved_then_identity_follows_docker(
        #[case] spec: &str,
        #[case] expected: Identity,
    ) {
        // Act
        let resolved = resolve(spec, PASSWD, GROUP);

        // Assert
        assert_eq!(resolved, Ok(expected));
    }

    #[test]
    fn given_non_root_user_when_resolved_then_group_zero_is_absent() {
        // Act
        let resolved = resolve("app", PASSWD, GROUP).unwrap();

        // Assert
        assert!(!resolved.groups.contains(&0), "{:?}", resolved.groups);
    }

    #[rstest::rstest]
    #[case::unknown_name("ghost", UserError::UserNotFound("ghost".into()))]
    #[case::unknown_name_with_group("ghost:staff", UserError::UserNotFound("ghost".into()))]
    #[case::unknown_group("app:ghosts", UserError::GroupNotFound("ghosts".into()))]
    #[case::too_large_uid("99999999999", UserError::UserNotFound("99999999999".into()))]
    fn given_unknown_name_when_resolved_then_error_names_it(
        #[case] spec: &str,
        #[case] expected: UserError,
    ) {
        // Act
        let resolved = resolve(spec, PASSWD, GROUP);

        // Assert
        assert_eq!(resolved, Err(expected));
    }

    #[test]
    fn given_unknown_user_when_displayed_then_wording_matches_docker() {
        // Act
        let message = UserError::UserNotFound("ghost".into()).to_string();

        // Assert
        assert_eq!(
            message,
            "unable to find user ghost: no matching entries in passwd file"
        );
    }

    /// Records the calls and fails the chosen one.
    struct Recorder {
        calls: RefCell<Vec<String>>,
        fail_on: Option<&'static str>,
    }

    impl Recorder {
        fn new(fail_on: Option<&'static str>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                fail_on,
            }
        }

        fn record(&self, call: &'static str, detail: String) -> std::io::Result<()> {
            self.calls.borrow_mut().push(format!("{call} {detail}"));
            if self.fail_on == Some(call) {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            Ok(())
        }
    }

    impl PrivilegeDropper for Recorder {
        fn change_dir(&self, dir: &CStr) -> std::io::Result<()> {
            self.record("chdir", dir.to_string_lossy().into_owned())
        }

        fn set_groups(&self, groups: &[u32]) -> std::io::Result<()> {
            self.record("setgroups", format!("{groups:?}"))
        }

        fn set_gid(&self, gid: u32) -> std::io::Result<()> {
            self.record("setgid", gid.to_string())
        }

        fn set_uid(&self, uid: u32) -> std::io::Result<()> {
            self.record("setuid", uid.to_string())
        }
    }

    #[test]
    fn given_identity_when_dropping_then_groups_gid_uid_run_in_order() {
        // Arrange
        let dropper = Recorder::new(None);

        // Act
        let result = drop_privileges(&dropper, &identity(1000, 1000, &[1000, 50], "/"));

        // Assert
        assert!(result.is_ok());
        assert_eq!(
            *dropper.calls.borrow(),
            ["setgroups [1000, 50]", "setgid 1000", "setuid 1000"]
        );
    }

    #[rstest::rstest]
    #[case::groups_fail("setgroups", 1)]
    #[case::gid_fails("setgid", 2)]
    #[case::uid_fails("setuid", 3)]
    fn given_a_failing_call_when_dropping_then_it_errors_and_nothing_later_runs(
        #[case] failing: &'static str,
        #[case] calls_made: usize,
    ) {
        // Arrange
        let dropper = Recorder::new(Some(failing));

        // Act
        let result = drop_privileges(&dropper, &identity(1000, 1000, &[1000], "/"));

        // Assert
        assert!(result.is_err());
        assert_eq!(dropper.calls.borrow().len(), calls_made);
    }

    const EACCES: i32 = 13;
    const ENOENT: i32 = 2;

    #[rstest::rstest]
    #[case::groups(Step::SetGroups)]
    #[case::gid(Step::SetGid)]
    #[case::uid(Step::SetUid)]
    #[case::dir(Step::ChangeDir)]
    fn given_a_tagged_os_error_when_untagged_then_step_and_errno_come_back(#[case] step: Step) {
        // Arrange
        let tagged = tag(step, std::io::Error::from_raw_os_error(EACCES));

        // Act
        let (found, plain) = untag(tagged);

        // Assert
        assert_eq!(found, Some(step));
        assert_eq!(plain.raw_os_error(), Some(EACCES));
    }

    #[test]
    fn given_an_untagged_os_error_when_untagged_then_no_step_is_found() {
        // Arrange
        let err = std::io::Error::from_raw_os_error(ENOENT);

        // Act
        let (found, plain) = untag(err);

        // Assert
        assert_eq!(found, None);
        assert_eq!(plain.raw_os_error(), Some(ENOENT));
    }

    #[test]
    fn given_workdir_when_entering_then_chdir_runs_after_the_privilege_drop() {
        // Arrange
        let dropper = Recorder::new(None);
        let dir = CString::new("/root/private").unwrap();

        // Act
        let result = enter_as(&dropper, &identity(1000, 1000, &[1000], "/"), Some(&dir));

        // Assert
        assert!(result.is_ok());
        assert_eq!(
            *dropper.calls.borrow(),
            [
                "setgroups [1000]",
                "setgid 1000",
                "setuid 1000",
                "chdir /root/private"
            ]
        );
    }

    #[test]
    fn given_a_failed_drop_when_entering_then_the_directory_is_never_entered() {
        // Arrange
        let dropper = Recorder::new(Some("setuid"));
        let dir = CString::new("/work").unwrap();

        // Act
        let result = enter_as(&dropper, &identity(1000, 1000, &[1000], "/"), Some(&dir));

        // Assert
        assert!(result.is_err());
        assert_eq!(dropper.calls.borrow().len(), 3);
    }

    #[test]
    fn given_no_workdir_when_entering_then_only_the_drop_runs() {
        // Arrange
        let dropper = Recorder::new(None);

        // Act
        let result = enter_as(&dropper, &identity(1000, 1000, &[1000], "/"), None);

        // Assert
        assert!(result.is_ok());
        assert_eq!(dropper.calls.borrow().len(), 3);
    }

    #[test]
    fn given_file_too_large_when_displayed_then_it_names_the_file() {
        // Act
        let message = UserError::FileTooLarge("passwd").to_string();

        // Assert
        assert_eq!(message, "passwd file too large");
    }
}
