//! The `git(1)` adapter of the pull reconciler (ADR-0021 decision 2).
//!
//! Git is a subprocess, never a library: `gix` and `git2` are both large
//! additions to the dependency graph of a container runtime. What that costs is
//! that every invocation has to be built so a repository cannot steer it, and
//! this module is the one place that builds one. Nothing else in the engine
//! starts `git`.
//!
//! Every process starts from `env_clear()` and gets back only what is listed in
//! [`Git::command`]: no `~/.gitconfig`, no `/etc/gitconfig`, no inherited
//! `GIT_*` variable, no hooks, no prompt, no transport other than `file`,
//! `https` and `ssh`, and a wall-clock ceiling. The reasons are the threat list
//! in the ADR's «Security pass».

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use delonix_model::{Error, Result};

/// The ceiling on one git invocation. A fetch that has not finished by then is
/// killed and the pass fails with a timeout — a hung remote must not hold the
/// lock until the next fire.
pub(crate) const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// The `-c` options every invocation carries, in this order.
///
/// A constant and not a builder so a test can read it: each line closes a
/// known vector, and removing one is a security regression the test names.
pub(crate) const FIXED_CONFIG: &[&str] = &[
    // A repository must not execute code on the node by being fetched: the
    // `reference-transaction` hook runs on a bare `fetch`.
    "core.hooksPath=/dev/null",
    // Only the three transports a source may name. `ext::` runs a command and
    // `fd::` reads a descriptor; both stay off.
    "protocol.allow=never",
    "protocol.file.allow=always",
    "protocol.https.allow=always",
    "protocol.ssh.allow=always",
    // A malformed tree (`..`, `.git` entries) is refused at the door.
    "fetch.fsckObjects=true",
    "transfer.fsckObjects=true",
    "submodule.recurse=false",
    // A redirect to another host is a different source.
    "http.followRedirects=false",
    "http.sslVerify=true",
    // No credential helper and no askpass: a credential comes from a named
    // `kind: Secret` or not at all.
    "credential.helper=",
    "core.askPass=",
    "core.fsmonitor=false",
    "gc.auto=0",
    "maintenance.auto=false",
];

/// The pinned key and known hosts of an `ssh://` source, as private files that
/// live for one pass.
#[derive(Debug, Clone)]
pub(crate) struct SshFiles {
    pub key: PathBuf,
    pub known_hosts: PathBuf,
}

/// One configured `git` binary.
#[derive(Debug, Clone)]
pub(crate) struct Git {
    bin: PathBuf,
    /// The private `HOME` every invocation runs with.
    home: PathBuf,
    /// The caller's `PATH`, so `ssh` and `ssh-keygen` resolve; nothing else of
    /// the caller's environment survives.
    path_env: Option<std::ffi::OsString>,
    ssh: Option<SshFiles>,
    timeout: Duration,
}

/// The package that carries `git` on the common distributions — the remedy a
/// bare `ENOENT` would not name.
const GIT_PACKAGE_HINT: &str = "install the `git` package (apt install git / dnf install git)";

impl Git {
    /// Finds `git` on `PATH`. A missing binary is DX-6601 and names the
    /// package, never an `ENOENT` that reads as a missing file.
    pub(crate) fn locate(home: &Path) -> Result<Git> {
        let path_env = std::env::var_os("PATH");
        let bin = path_env
            .as_ref()
            .and_then(|p| {
                std::env::split_paths(p)
                    .map(|d| d.join("git"))
                    .find(|c| c.is_file())
            })
            .ok_or_else(|| {
                Error::coded(
                    6601,
                    Error::Unavailable(super::po::tf(
                        "gitops: `git` is not installed on this node — {hint}",
                        &[("hint", GIT_PACKAGE_HINT)],
                    )),
                )
            })?;
        Ok(Git {
            bin,
            home: home.to_path_buf(),
            path_env,
            ssh: None,
            timeout: GIT_TIMEOUT,
        })
    }

    /// The same binary, speaking `ssh` with these pinned files.
    pub(crate) fn with_ssh(mut self, files: SshFiles) -> Git {
        self.ssh = Some(files);
        self
    }

    /// A shorter ceiling, for tests that prove the timeout fires.
    #[cfg(test)]
    pub(crate) fn with_timeout(mut self, t: Duration) -> Git {
        self.timeout = t;
        self
    }

    /// The `GIT_SSH_COMMAND` for the pinned files: no config file, no agent,
    /// no host the pinned list does not name, no prompt.
    pub(crate) fn ssh_command(files: &SshFiles) -> String {
        format!(
            "ssh -F /dev/null -o BatchMode=yes -o StrictHostKeyChecking=yes \
             -o UserKnownHostsFile={} -o GlobalKnownHostsFile=/dev/null \
             -o IdentitiesOnly=yes -o IdentityAgent=none -o IdentityFile={} \
             -o ConnectTimeout=30",
            shell_quote(&files.known_hosts.to_string_lossy()),
            shell_quote(&files.key.to_string_lossy()),
        )
    }

    /// The command, before its arguments: the binary, the fixed configuration
    /// and the environment that replaces the caller's.
    pub(crate) fn command(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.env_clear();
        if let Some(p) = &self.path_env {
            c.env("PATH", p);
        }
        c.env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PROTOCOL_FROM_USER", "0")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("LC_ALL", "C");
        if let Some(files) = &self.ssh {
            c.env("GIT_SSH_COMMAND", Self::ssh_command(files));
        }
        for kv in FIXED_CONFIG {
            c.arg("-c").arg(kv);
        }
        c.stdin(Stdio::null());
        c
    }

    /// Runs `git <args>` and returns stdout, refusing more than `cap` bytes of
    /// it and killing the process when the ceiling passes.
    pub(crate) fn run(&self, args: &[&str], cap: usize) -> Result<Vec<u8>> {
        let mut c = self.command();
        c.args(args);
        run_capped(c, cap, self.timeout, args.first().copied().unwrap_or("git"))
    }

    /// `git --git-dir=<mirror> <args>`.
    pub(crate) fn in_repo(&self, repo: &Path, args: &[&str], cap: usize) -> Result<Vec<u8>> {
        let dir = format!("--git-dir={}", repo.display());
        let mut all: Vec<&str> = vec![dir.as_str()];
        all.extend_from_slice(args);
        self.run(&all, cap)
    }
}

/// Single-quotes a value for the shell that `GIT_SSH_COMMAND` goes through.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Spawns `c`, drains stdout (up to `cap`) and stderr (up to 64 KiB) on their
/// own threads, and kills it at `timeout`.
fn run_capped(mut c: Command, cap: usize, timeout: Duration, what: &str) -> Result<Vec<u8>> {
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::coded(
                6601,
                Error::Unavailable(super::po::tf(
                    "gitops: `git` is not installed on this node — {hint}",
                    &[("hint", GIT_PACKAGE_HINT)],
                )),
            )
        } else {
            Error::Io(e)
        }
    })?;
    let out = child.stdout.take().map(|s| drain(s, cap.saturating_add(1)));
    let err = child.stderr.take().map(|s| drain(s, 64 * 1024));
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::coded(
                8601,
                Error::Timeout(super::po::tf(
                    "gitops: `git {what}` did not finish within {secs}s and was stopped",
                    &[("what", what), ("secs", &timeout.as_secs().to_string())],
                )),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = err
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    if stdout.len() > cap {
        return Err(Error::coded(
            1603,
            Error::Invalid(super::po::tf(
                "gitops: `git {what}` answered more than {cap} bytes — refused",
                &[("what", what), ("cap", &cap.to_string())],
            )),
        ));
    }
    if !status.success() {
        let msg = String::from_utf8_lossy(&stderr);
        let msg = msg.trim();
        return Err(GitFailed {
            what: what.to_string(),
            stderr: msg.chars().take(2000).collect(),
        }
        .into());
    }
    Ok(stdout)
}

/// A git invocation that exited non-zero, with what it said.
#[derive(Debug)]
pub(crate) struct GitFailed {
    pub what: String,
    pub stderr: String,
}

impl From<GitFailed> for Error {
    fn from(g: GitFailed) -> Error {
        Error::Runtime {
            context: "git",
            message: format!("git {}: {}", g.what, g.stderr),
        }
    }
}

fn drain<R: Read + Send + 'static>(mut r: R, cap: usize) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() < cap {
                        let keep = n.min(cap - buf.len());
                        buf.extend_from_slice(&chunk[..keep]);
                    }
                    // Past the cap the pipe is still drained, so the child is
                    // never blocked on a full pipe while we wait for it.
                }
            }
        }
        buf
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each line closes a vector named in the ADR's threat list. Removing one
    /// is a regression this test names rather than a diff nobody reads.
    #[test]
    fn the_fixed_configuration_closes_the_named_vectors() {
        for needed in [
            "core.hooksPath=/dev/null",
            "protocol.allow=never",
            "fetch.fsckObjects=true",
            "transfer.fsckObjects=true",
            "http.followRedirects=false",
            "credential.helper=",
            "submodule.recurse=false",
        ] {
            assert!(FIXED_CONFIG.contains(&needed), "{needed} is missing");
        }
        // Only the three transports a source may name are re-allowed.
        let allowed: Vec<&&str> = FIXED_CONFIG
            .iter()
            .filter(|c| c.starts_with("protocol.") && c.ends_with("=always"))
            .collect();
        assert_eq!(allowed.len(), 3, "{allowed:?}");
        assert!(!FIXED_CONFIG.iter().any(|c| c.starts_with("protocol.ext")));
    }

    /// The caller's environment never reaches git: a `GIT_DIR`, a
    /// `GIT_CONFIG_PARAMETERS` or a `HOME` with a hostile `.gitconfig` would
    /// otherwise steer every invocation.
    #[test]
    fn the_command_starts_from_an_empty_environment() {
        let g = Git {
            bin: PathBuf::from("/usr/bin/git"),
            home: PathBuf::from("/nonexistent/home"),
            path_env: Some("/usr/bin".into()),
            ssh: None,
            timeout: GIT_TIMEOUT,
        };
        let c = g.command();
        let envs: Vec<(String, Option<String>)> = c
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let get = |k: &str| {
            envs.iter()
                .find(|(n, _)| n == k)
                .and_then(|(_, v)| v.clone())
        };
        assert_eq!(get("HOME").as_deref(), Some("/nonexistent/home"));
        assert_eq!(get("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
        assert_eq!(get("GIT_CONFIG_GLOBAL").as_deref(), Some("/dev/null"));
        assert_eq!(get("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
        assert_eq!(get("GIT_PROTOCOL_FROM_USER").as_deref(), Some("0"));
        assert!(
            get("GIT_SSH_COMMAND").is_none(),
            "no ssh without pinned files"
        );
        // `env_clear` is what drops everything else: the env list Command keeps
        // is exactly what we set.
        let names: Vec<&str> = envs.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"GIT_DIR"));
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.iter().filter(|a| *a == "-c").count(),
            FIXED_CONFIG.len()
        );
    }

    /// The pinned files are quoted for the shell `GIT_SSH_COMMAND` passes
    /// through, and host-key checking cannot be relaxed by the repository.
    #[test]
    fn the_ssh_command_pins_the_host_and_quotes_the_paths() {
        let cmd = Git::ssh_command(&SshFiles {
            key: PathBuf::from("/s/it's key"),
            known_hosts: PathBuf::from("/s/kh"),
        });
        assert!(cmd.contains("StrictHostKeyChecking=yes"), "{cmd}");
        assert!(cmd.contains("BatchMode=yes"), "{cmd}");
        assert!(cmd.contains("-F /dev/null"), "{cmd}");
        assert!(cmd.contains(r"'/s/it'\''s key'"), "{cmd}");
        assert!(cmd.contains("UserKnownHostsFile='/s/kh'"), "{cmd}");
    }

    /// A git that does not finish is killed at the ceiling and reported as a
    /// timeout (DX-8601), not left holding the pass's lock.
    #[test]
    fn a_git_that_hangs_is_killed_at_the_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("git");
        std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let g = Git {
            bin: fake,
            home: dir.path().to_path_buf(),
            path_env: None,
            ssh: None,
            timeout: GIT_TIMEOUT,
        }
        .with_timeout(Duration::from_millis(300));
        // A script written a moment ago can still be open for writing in a
        // descriptor another test thread's fork inherited: `execve` then
        // answers ETXTBSY. That is the harness, not the code under test, so it
        // is retried rather than read as a result.
        let (e, took) = loop {
            let t = Instant::now();
            let e = g.run(&["fetch"], 1024).unwrap_err();
            let busy = e.to_string().contains("Text file busy");
            if !busy {
                break (e, t.elapsed());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(took < Duration::from_secs(10), "{took:?}");
        assert_eq!(e.number(), 8601, "{e}");
    }
}
