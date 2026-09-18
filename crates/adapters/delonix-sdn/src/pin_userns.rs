//! The pin creates its own namespaces, and its parent writes the id maps.
//!
//! The pin used to be started as `unshare --user --map-auto --map-root-user --net
//! --mount -- delonix netns pin`, so the process that CREATED the user namespace was
//! `/usr/bin/unshare`. With `kernel.apparmor_restrict_unprivileged_userns=1` that only
//! works while the caller carries a profile with `userns`: a profile flagged
//! `unconfined` is inherited across `exec`, so `unshare(1)` ran under the caller's.
//! From a caller WITHOUT one — the split ADR-0040 D2.4 asks for, with the profile on
//! the holder's path and not on the CLI's — `unshare(1)` is moved into the
//! `unprivileged_userns` profile and its `mount` is denied. Measured on Ubuntu 24.04:
//! the audit shows `userns_create … comm="unshare" target="unprivileged_userns"`, and
//! the old caller saw only `timeout waiting for the netns holder` five seconds later.
//!
//! So the roles are swapped, and nothing else changes:
//!
//! 1. the caller spawns `delonix netns pin` directly, with two pipes;
//! 2. the pin calls `unshare(USER|NET|NS)` itself and says so on the first pipe;
//! 3. the caller — still OUTSIDE the new user namespace — writes the maps,
//!    the same ones `unshare(1)` wrote: `0 <euid> 1` plus the user's range from
//!    `/etc/subuid`/`/etc/subgid` through `newuidmap`/`newgidmap`, or the single
//!    uid with `setgroups=deny` where no range can be had (a nested user
//!    namespace, no helpers, no entry);
//! 4. the caller says "go" on the second pipe, and the pin makes `/` private
//!    (what `unshare --mount` does by default) and carries on as before.
//!
//! The pin is only asked to do this when [`SYNC_ENV`] is set. A pin started by
//! `nsenter` into namespaces that already exist (the adoption path) must not
//! create new ones, and it never gets the variable.

use crate::{Error, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Carries `<read-fd>,<write-fd>` of the pin's ends of the two pipes.
pub(crate) const SYNC_ENV: &str = "DELONIX_PIN_SYNC";

/// Byte the pin writes once it is inside its new namespaces.
const UNSHARED: u8 = b'u';
/// Byte the caller writes once the maps are in place.
const GO: u8 = b'g';

/// The two pipes of the handshake. The caller keeps `parent_*`; the pin inherits
/// `child_*` (they are made inheritable in the forked child only).
pub(crate) struct SyncPipes {
    pub(crate) parent_read: OwnedFd,
    pub(crate) parent_write: OwnedFd,
    pub(crate) child_read: OwnedFd,
    pub(crate) child_write: OwnedFd,
}

impl SyncPipes {
    pub(crate) fn new() -> Result<Self> {
        let (child_read, parent_write) = pipe()?;
        let (parent_read, child_write) = pipe()?;
        Ok(Self {
            parent_read,
            parent_write,
            child_read,
            child_write,
        })
    }

    /// The value of [`SYNC_ENV`] for the child.
    pub(crate) fn env_value(&self) -> String {
        format!(
            "{},{}",
            self.child_read.as_raw_fd(),
            self.child_write.as_raw_fd()
        )
    }
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a valid two-element array for the duration of the call.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(Error::Command {
            context: "pipe",
            message: std::io::Error::last_os_error().to_string(),
        });
    }
    // SAFETY: `pipe2` succeeded, so both descriptors are open and owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Clears `FD_CLOEXEC` on `fd`. Runs between `fork` and `exec`, so it only makes
/// a raw syscall.
pub(crate) fn make_inheritable(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: `fcntl(F_SETFD)` on an integer descriptor has no memory preconditions
    // and is async-signal-safe.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn read_byte(fd: RawFd) -> Option<u8> {
    let mut b = [0u8; 1];
    loop {
        // SAFETY: `b` is a valid one-byte buffer for the duration of the call.
        let n = unsafe { libc::read(fd, b.as_mut_ptr().cast(), 1) };
        if n == 1 {
            return Some(b[0]);
        }
        if n == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return None;
    }
}

fn write_byte(fd: RawFd, b: u8) -> bool {
    loop {
        // SAFETY: `b` lives on the stack for the duration of the call.
        let n = unsafe { libc::write(fd, (&b as *const u8).cast(), 1) };
        if n == 1 {
            return true;
        }
        if n == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return false;
    }
}

// ---------------------------------------------------------------- the pin's side

/// Parses `<read-fd>,<write-fd>`. PURE.
fn parse_sync(value: &str) -> Option<(RawFd, RawFd)> {
    let (r, w) = value.split_once(',')?;
    let r: RawFd = r.trim().parse().ok()?;
    let w: RawFd = w.trim().parse().ok()?;
    (r > 2 && w > 2 && r != w).then_some((r, w))
}

/// Run by the pin when [`SYNC_ENV`] is set: enter new user, network and mount
/// namespaces, wait for the maps, make `/` private. `Err` carries the reason for
/// the status file; the caller of the pin reads it from there.
///
/// Must run while the process is still single-threaded — `unshare(CLONE_NEWUSER)`
/// refuses a multithreaded caller with `EINVAL`.
pub(crate) fn enter_namespaces(sync: &str) -> std::result::Result<(), String> {
    let (r, w) = parse_sync(sync).ok_or_else(|| format!("invalid {SYNC_ENV}={sync:?}"))?;
    // SAFETY: the caller of the pin passed us these two descriptors and nothing else
    // in this process owns them.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(r), OwnedFd::from_raw_fd(w)) };

    let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWNS;
    // SAFETY: `unshare` takes only flags; it does not touch memory.
    if unsafe { libc::unshare(flags) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(refusal("unshare(user, net, mount)", &e));
    }
    if !write_byte(w.as_raw_fd(), UNSHARED) {
        return Err("the caller went away before the user namespace was mapped".into());
    }
    drop(w);
    if read_byte(r.as_raw_fd()) != Some(GO) {
        return Err("the caller did not map the user namespace".into());
    }
    drop(r);

    let root = c"/";
    // SAFETY: `root` is a NUL-terminated literal; the other pointers are null, which
    // `mount(2)` accepts for a propagation change.
    let rc = unsafe {
        libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        return Err(refusal("making / private in the new mount namespace", &e));
    }
    Ok(())
}

/// The message for a refused step. PURE.
///
/// The AppArmor hint goes on BOTH steps because of where the restriction bites,
/// measured on Ubuntu 24.04 with the sysctl at 1: `unshare` itself succeeds for an
/// unprofiled executable — the kernel moves it into the `unprivileged_userns`
/// profile — and it is the `mount` right after that is denied (`EACCES`, audit
/// `profile="unprivileged_userns" operation="mount"`).
fn refusal(step: &str, e: &std::io::Error) -> String {
    let hint = match e.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => {
            " — on a host with kernel.apparmor_restrict_unprivileged_userns=1 the \
             delonix executable needs an AppArmor profile with `userns` at the path \
             it runs from (install.sh writes one for the installed binary)"
        }
        Some(libc::ENOSPC) => " — user.max_user_namespaces is exhausted",
        _ => "",
    };
    format!("{step}: {e}{hint}")
}

// ---------------------------------------------------------------- the caller's side

/// The first `<start>:<count>` of `/etc/subuid`/`/etc/subgid` that belongs to the
/// user, by name or by numeric id — the lookup `unshare --map-auto` does. PURE.
pub(crate) fn parse_subid(content: &str, name: Option<&str>, id: u32) -> Option<(u32, u32)> {
    let id = id.to_string();
    content.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut f = line.split(':');
        let owner = f.next()?;
        let start: u32 = f.next()?.trim().parse().ok()?;
        let count: u32 = f.next()?.trim().parse().ok()?;
        if f.next().is_some() || count == 0 {
            return None;
        }
        (Some(owner) == name || owner == id).then_some((start, count))
    })
}

/// The `newuidmap`/`newgidmap` arguments after the pid: the caller's own id as 0,
/// then the delegated range from 1. PURE.
pub(crate) fn range_map(own: u32, range: (u32, u32)) -> Vec<String> {
    [0, own, 1, 1, range.0, range.1]
        .iter()
        .map(u32::to_string)
        .collect()
}

/// The login name of `uid`, for the subid lookup.
fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: an all-zero `passwd` is a valid value to be overwritten.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer refers to a live local of the stated size, and `out` is
    // only read after the call returns.
    let rc =
        unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() || pwd.pw_name.is_null() {
        return None;
    }
    // SAFETY: on success `pw_name` points to a NUL-terminated string inside `buf`.
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
    name.to_str().ok().map(str::to_owned)
}

fn helper(tool: &str) -> Option<&'static str> {
    let candidates: &[&'static str] = match tool {
        "newuidmap" => &["/usr/bin/newuidmap", "/bin/newuidmap"],
        _ => &["/usr/bin/newgidmap", "/bin/newgidmap"],
    };
    candidates
        .iter()
        .copied()
        .find(|p| std::path::Path::new(p).exists())
}

fn run_helper(path: &str, pid: i32, args: &[String]) -> Result<()> {
    let out = std::process::Command::new(path)
        .arg(pid.to_string())
        .args(args)
        .output()
        .map_err(|e| Error::Command {
            context: "idmap",
            message: format!("{path}: {e}"),
        })?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Command {
        context: "idmap",
        message: format!(
            "{path} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    })
}

/// Writes the pin's maps from outside its user namespace. Same result as the
/// `unshare --map-auto --map-root-user` it replaces; where that could not map a
/// range, the single uid (what `--map-root-user` alone does).
pub(crate) fn write_maps(pid: i32) -> Result<()> {
    // SAFETY: geteuid/getegid have no preconditions.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if delonix_node::in_initial_userns() {
        if let (Some(uidmap), Some(gidmap)) = (helper("newuidmap"), helper("newgidmap")) {
            let name = user_name(euid);
            let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
            let uids = parse_subid(&read("/etc/subuid"), name.as_deref(), euid);
            let gids = parse_subid(&read("/etc/subgid"), name.as_deref(), euid);
            if let (Some(u), Some(g)) = (uids, gids) {
                run_helper(uidmap, pid, &range_map(euid, u))?;
                run_helper(gidmap, pid, &range_map(egid, g))?;
                return Ok(());
            }
        }
    }
    // A nested namespace (newuidmap checks the REAL uid against /etc/subuid and
    // refuses), no helpers, or no entry: map the one uid we hold. Writing a gid map
    // by hand requires `setgroups=deny` first.
    let write = |file: &str, body: String| {
        std::fs::write(format!("/proc/{pid}/{file}"), body).map_err(|e| Error::Command {
            context: "idmap",
            message: format!("/proc/{pid}/{file}: {e}"),
        })
    };
    write("setgroups", "deny".into())?;
    write("uid_map", format!("0 {euid} 1\n"))?;
    write("gid_map", format!("0 {egid} 1\n"))?;
    Ok(())
}

/// The caller's half of the handshake, after `spawn`: wait for the pin to be in
/// its namespaces, map them, let it go. `Err` means the pin must be killed.
pub(crate) fn handshake(pipes: SyncPipes, pid: i32, wait_ms: i32) -> Result<()> {
    let SyncPipes {
        parent_read,
        parent_write,
        child_read,
        child_write,
    } = pipes;
    // Our copies of the pin's ends must go, or a pin that dies would never read as EOF.
    drop(child_read);
    drop(child_write);
    if !crate::infra::wait_readable(parent_read.as_raw_fd(), wait_ms) {
        return Err(Error::Command {
            context: "netns pin",
            message: format!("the pin did not create its namespaces within {wait_ms} ms"),
        });
    }
    if read_byte(parent_read.as_raw_fd()) != Some(UNSHARED) {
        return Err(Error::Command {
            context: "netns pin",
            message: "the pin exited before creating its namespaces".into(),
        });
    }
    write_maps(pid)?;
    if !write_byte(parent_write.as_raw_fd(), GO) {
        return Err(Error::Command {
            context: "netns pin",
            message: "the pin exited while its namespaces were being mapped".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBUID: &str = "\
# comment
root:200000:1000
walter:100000:65536
1001:300000:65536
walter:900000:10
";

    #[test]
    fn the_first_entry_of_the_user_wins() {
        assert_eq!(
            parse_subid(SUBUID, Some("walter"), 1000),
            Some((100000, 65536))
        );
    }

    #[test]
    fn a_numeric_owner_matches_the_id() {
        assert_eq!(parse_subid(SUBUID, None, 1001), Some((300000, 65536)));
        assert_eq!(
            parse_subid(SUBUID, Some("nobody"), 1001),
            Some((300000, 65536))
        );
    }

    #[test]
    fn no_entry_or_a_broken_one_is_none() {
        assert_eq!(parse_subid(SUBUID, Some("ana"), 1002), None);
        assert_eq!(
            parse_subid("ana:x:10\nana:5:0\nana:1:2:3\n", Some("ana"), 7),
            None
        );
    }

    #[test]
    fn the_range_map_is_what_unshare_wrote() {
        // Measured on the pin started by `unshare --map-auto --map-root-user`:
        // `0 1000 1` and `1 100000 65536`.
        assert_eq!(
            range_map(1000, (100000, 65536)),
            ["0", "1000", "1", "1", "100000", "65536"]
        );
    }

    #[test]
    fn the_sync_value_needs_two_distinct_non_stdio_fds() {
        assert_eq!(parse_sync("5,6"), Some((5, 6)));
        assert_eq!(parse_sync("1,6"), None);
        assert_eq!(parse_sync("5,5"), None);
        assert_eq!(parse_sync("5"), None);
        assert_eq!(parse_sync("a,6"), None);
    }

    #[test]
    fn a_refused_step_names_apparmor_only_for_a_permission_error() {
        let eperm = std::io::Error::from_raw_os_error(libc::EPERM);
        assert!(refusal("unshare", &eperm).contains("AppArmor"));
        let eacces = std::io::Error::from_raw_os_error(libc::EACCES);
        assert!(refusal("mount", &eacces).starts_with("mount: "));
        assert!(refusal("mount", &eacces).contains("AppArmor"));
        let einval = std::io::Error::from_raw_os_error(libc::EINVAL);
        assert!(!refusal("unshare", &einval).contains("AppArmor"));
    }
}
