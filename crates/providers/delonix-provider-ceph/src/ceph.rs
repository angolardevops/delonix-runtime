//! What every Ceph backend shares: the client identity (which `ceph.conf`,
//! which cephx user), how an argv is built from it, and how a tool's answer is
//! classified.
//!
//! A driver never shells a line — it builds an argv and runs it through the
//! storage [`Run`](delonix_storage::exec::Run) port, so the tests replace the
//! cluster with a recorder and every command is checked as data (ADR-0081 D2,
//! the shape ADR-0067 gave the zfs/lvm/btrfs drivers).

use std::path::{Path, PathBuf};

use delonix_storage::allowlist::Entry;
use delonix_storage::exec::Output;

/// The cephx client the engine authenticates as when the allowlist names none.
pub const DEFAULT_USER: &str = "admin";

/// The client identity a driver reads from a pool's allowlist entry: the
/// `ceph.conf` to read and the cephx user to be. The key is never here — it is
/// in the keyring beside the configuration, which only the client library
/// reads (ADR-0081 D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CephCtx {
    pub conf: Option<PathBuf>,
    pub user: String,
}

impl CephCtx {
    /// The identity a pool's allowlist entry declares.
    pub fn from_entry(entry: &Entry) -> Self {
        Self {
            conf: entry.ceph_conf.clone(),
            user: entry
                .ceph_user
                .clone()
                .unwrap_or_else(|| DEFAULT_USER.to_string()),
        }
    }

    /// The identity's global flags, the ones every Ceph tool accepts:
    /// `-c <conf>` when a configuration is named, and `--id <user>`.
    pub fn global(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(conf) = &self.conf {
            v.push("-c".into());
            v.push(conf.display().to_string());
        }
        v.push("--id".into());
        v.push(self.user.clone());
        v
    }

    /// `[tool] + global flags + parts` — the argv a driver hands to `Run`.
    pub fn argv(&self, tool: &str, parts: &[&str]) -> Vec<String> {
        let mut v = Vec::with_capacity(1 + parts.len() + 4);
        v.push(tool.to_string());
        v.extend(self.global());
        v.extend(parts.iter().map(|s| s.to_string()));
        v
    }
}

/// What a Ceph tool's answer was, classified — the three states a probe and an
/// adopt-or-refuse need to tell apart (ADR-0067 D3.5): the thing is there, it
/// is provably not there, or the tool could not find out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The command succeeded.
    Ok(Output),
    /// The object (pool, image, subvolume, bucket) provably does not exist.
    Missing(String),
    /// The tool is not installed on this node.
    Unavailable(String),
    /// The cluster could not be reached, the keyring is missing, or the caller
    /// is not permitted — anything that is not a clean yes or no.
    Undetermined(String),
}

/// Classifies a Ceph tool's [`Output`]. A success is [`Answer::Ok`]; a failure
/// is read by the words Ceph itself prints (`librados`, `rbd`, `ceph fs`).
pub fn classify(out: &Output) -> Answer {
    if out.success() {
        return Answer::Ok(out.clone());
    }
    let all = format!("{}\n{}", out.stderr, out.stdout);
    let low = all.to_ascii_lowercase();

    // Provably absent — the object is not there.
    if low.contains("does not exist")
        || low.contains("no such file or directory")
        || low.contains("not found")
        || (low.contains("error opening") && low.contains("(2)"))
        || low.contains("enoent")
    {
        return Answer::Missing(out.why());
    }

    // The cluster is out of reach, or we may not look — undetermined, never a
    // clean "absent".
    if low.contains("timed out")
        || low.contains("monclient")
        || low.contains("unable to find keyring")
        || low.contains("permission denied")
        || low.contains("operation not permitted")
        || low.contains("access denied")
        || low.contains("errno 110")
        || low.contains("(13)")
        || low.contains("connect")
    {
        return Answer::Undetermined(out.why());
    }

    Answer::Undetermined(out.why())
}

/// Classifies the `std::io::Error` of a tool that could not even start: a
/// `NotFound` is the tool not being installed.
pub fn classify_start(tool: &str, err: &std::io::Error) -> Answer {
    if err.kind() == std::io::ErrorKind::NotFound {
        Answer::Unavailable(format!("`{tool}` is not installed on this node"))
    } else {
        Answer::Undetermined(format!("could not run `{tool}`: {err}"))
    }
}

/// Runs `argv` through `run`, mapping both a failed start and a non-zero exit
/// into an [`Answer`].
pub fn run(run: &dyn delonix_storage::exec::Run, argv: &[String]) -> Answer {
    let tool = argv.first().map(String::as_str).unwrap_or("");
    match run.run(argv) {
        Ok(out) => classify(&out),
        Err(e) => classify_start(tool, &e),
    }
}

/// The owner stamp's keys, set as metadata on the object itself (RBD image
/// metadata, CephFS subvolume metadata) so a volume is adopted or released by
/// its stamp and never by its name alone (ADR-0067 D5).
pub const META_OWNER: &str = "delonix.io/owner";
pub const META_POOL: &str = "delonix.io/pool";

/// The conf a driver passes to a native tool, for a message.
pub fn conf_label(conf: &Option<PathBuf>) -> String {
    conf.as_deref()
        .map(Path::display)
        .map(|d| d.to_string())
        .unwrap_or_else(|| "/etc/ceph/ceph.conf (default)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(user: Option<&str>, conf: Option<&str>) -> Entry {
        Entry {
            driver: "ceph-rbd".into(),
            ceph_user: user.map(str::to_string),
            ceph_conf: conf.map(PathBuf::from),
            ..Default::default()
        }
    }

    #[test]
    fn the_identity_defaults_to_admin_and_no_explicit_conf() {
        let ctx = CephCtx::from_entry(&entry(None, None));
        assert_eq!(ctx.user, "admin");
        assert_eq!(ctx.global(), vec!["--id", "admin"]);
    }

    #[test]
    fn a_named_conf_and_user_become_global_flags_on_every_argv() {
        let ctx = CephCtx::from_entry(&entry(Some("delonix"), Some("/etc/ceph/ceph.conf")));
        assert_eq!(
            ctx.argv("rbd", &["ls", "rbdpool"]),
            vec![
                "rbd",
                "-c",
                "/etc/ceph/ceph.conf",
                "--id",
                "delonix",
                "ls",
                "rbdpool"
            ]
        );
    }

    fn out(code: i32, stderr: &str) -> Output {
        Output {
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn a_missing_object_is_told_apart_from_an_unreachable_cluster() {
        assert!(matches!(
            classify(&out(
                2,
                "rbd: error opening image testimg: (2) No such file or directory"
            )),
            Answer::Missing(_)
        ));
        assert!(matches!(
            classify(&out(
                110,
                "monclient(hunting): authenticate timed out\n[errno 110] RADOS timed out"
            )),
            Answer::Undetermined(_)
        ));
        assert!(matches!(
            classify(&out(13, "Error EACCES: access denied")),
            Answer::Undetermined(_)
        ));
        assert!(matches!(classify(&out(0, "")), Answer::Ok(_)));
    }

    #[test]
    fn a_tool_that_is_not_installed_is_unavailable() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "no rbd");
        assert!(matches!(classify_start("rbd", &e), Answer::Unavailable(_)));
    }
}
