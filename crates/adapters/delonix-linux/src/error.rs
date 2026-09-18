//! `delonix-linux`'s own failures, grouped by what went wrong, and the
//! dictionary number of each group (ADR-0043).
//!
//! **The messages are a contract with what was there before.** Each variant
//! wraps the exact text the call site built by hand and converts into the same
//! shared class it already used — so the CLI prints byte for byte what it
//! printed and exits with the same code as before this type existed. Nothing
//! here changes CLASS (`Invalid` stays `Invalid`, `NotRunning` stays
//! `NotRunning`, `Conflict` stays `Conflict`): this crate is the lowest layer
//! that actually performs `clone`/`mount`/`setns`/cgroup writes, and its
//! failures already used the right shared class before ADR-0043 gave classes
//! numbers — there was nothing to correct on the way, only something to name.
//!
//! **Grouped, not enumerated one-to-one.** The dozens of sites that shell a
//! kernel operation out through `Error::Runtime{context,message}` (`clone`,
//! `mount`, `setns`, a `/proc`/cgroup write, `busctl`, `waitpid`…) share one
//! shape and one caller reaction — read the errno the message carries — and
//! collapse into one variant, [`Error::Syscall`], with one number. The same
//! reasoning already used by `delonix-vm`'s `Command` variant for `virsh`/
//! `qemu-img` failures. A handful of near-identical "unsafe/missing mount
//! path" and "live-mount helper failed" pairs (mount vs. unmount) collapse the
//! same way.
//!
//! **Two domains, chosen by what the failure is ABOUT, not which crate wrote
//! it** — the same rule `delonix-truenas` already set (its own failures live
//! under [`delonix_model::codes::Domain::Volume`], not a "TrueNAS" domain,
//! because a provisioning failure is still about a volume). A failure that
//! names a specific container — it is not running, it already is, its
//! `--memory`/`--cpus`/`--device` argument does not parse, its live mount
//! helper failed — is [`delonix_model::codes::Domain::Container`]. A failure
//! that is about THIS HOST having (or not having) a capability — AppArmor, a
//! CDI spec, the kernel call itself — is
//! [`delonix_model::codes::Domain::Host`], claimed here for the first time
//! (`Domain::Host` had zero entries before this crate).
//!
//! **[`Error::NotRunning`]/[`Error::AlreadyRunning`] are not reclassified
//! variants of the shared class** the way `delonix-vm`'s `NotRunningForOp`
//! is — they mean exactly what
//! [`delonix_model::Error::NotRunning`]/[`delonix_model::Error::Conflict`]
//! already mean (a container that is/is not running), so they convert
//! straight into that class with their own, more specific number
//! (`container.not_running`/`container.already_running`) rather than
//! inventing new wording for an old meaning.

use thiserror::Error;

/// A failure of `delonix-linux`.
#[derive(Debug, Error)]
pub enum Error {
    // ---- invalid argument, about a specific container ---------------------
    /// `--memory`/a Kubernetes `resources.limits.memory` that does not parse
    /// as a byte count.
    #[error("{0}")]
    InvalidMemoryLimit(String),

    /// `--cpus`/a Kubernetes `resources.limits.cpu` that does not parse as a
    /// number of cores.
    #[error("{0}")]
    InvalidCpuLimit(String),

    /// `--cpu-weight`/`--io-weight` outside the cgroup v2 `1..=10000` range.
    #[error("{0}")]
    InvalidCgroupWeight(String),

    /// `--cpuset` that does not parse as a CPU list.
    #[error("{0}")]
    InvalidCpuset(String),

    /// A command/entrypoint argument that cannot become a `CString` (an
    /// embedded NUL byte).
    #[error("{0}")]
    InvalidCommandArgv(String),

    /// No command to run at all: neither the image's `ENTRYPOINT`/`CMD` nor an
    /// explicit one.
    #[error("{0}")]
    EmptyCommand(String),

    /// A per-process operation (`renice`) found no live process in the
    /// container.
    #[error("{0}")]
    NoProcesses(String),

    /// A live bind-mount/unmount target that resolves outside the container's
    /// own filesystem, or is otherwise refused by `mount_target_safe`.
    #[error("{0}")]
    UnsafeMountPath(String),

    /// A live bind-mount's host source path does not exist.
    #[error("{0}")]
    MountSourceMissing(String),

    /// A live mount/unmount was asked of a container with no private mount
    /// namespace to change.
    #[error("{0}")]
    SharesHostMountNamespace(String),

    /// The forked helper that performs a live bind-mount inside the running
    /// container's namespaces failed or was interrupted.
    #[error("{0}")]
    LiveMountFailed(String),

    /// The forked helper that performs a live unmount inside the running
    /// container's namespaces did not succeed.
    #[error("{0}")]
    LiveUnmountFailed(String),

    /// A `--device`/`--gpus` value naming a CDI-qualified device that is not
    /// `vendor.com/class=name`.
    #[error("{0}")]
    InvalidCdiDeviceName(String),

    /// A CRI `RunPodSandbox` named a kubelet cgroup parent, but this process
    /// has no privilege to place a container in the host's cgroup hierarchy
    /// (rootless, or already inside a user namespace).
    #[error("{0}")]
    KubeCgroupNeedsRoot(String),

    // ---- not running / already running -------------------------------------
    /// The container exists but is not running, and the operation needs it
    /// running. Converts straight into the shared
    /// [`delonix_model::Error::NotRunning`] — see the module doc comment.
    #[error("container is not running: {0}")]
    NotRunning(String),

    // ---- not found ----------------------------------------------------
    /// A CDI device name is not declared by any discovered CDI spec.
    #[error("{0}")]
    CdiDeviceNotFound(String),

    // ---- conflict -------------------------------------------------------
    /// The container is running, and the caller asked to remove it without
    /// `--force`.
    #[error("{0}")]
    AlreadyRunning(String),

    // ---- invalid argument, about the HOST's own capability -----------------
    /// The AppArmor confinement asked for could not be set up: AppArmor is not
    /// enabled on this host, `apparmor_parser` is not on `PATH`, or it refused
    /// the profile. Grouped: the three sites already shared one caller
    /// reaction (fix AppArmor on this host, or ask for `unconfined`).
    #[error("{0}")]
    ApparmorUnavailable(String),

    /// A `--gpus`/`--device` request named a CDI-qualified device, but this
    /// host has no generated CDI spec at all (as opposed to
    /// [`Error::CdiDeviceNotFound`], where a spec exists and simply does not
    /// declare that device).
    #[error("{0}")]
    NoCdiSpec(String),

    // ---- system failure -----------------------------------------------
    /// A kernel operation this crate performs to build or manage a container
    /// (`clone`, `mount`, `setns`, a cgroup or `/proc` write, `waitpid`,
    /// `busctl`…) failed. The message carries the operation and the
    /// underlying error, exactly as the pre-ADR-0043 shared
    /// `Error::Runtime{context,message}` did — this IS that shape, with a
    /// number of its own.
    #[error("system call `{context}` failed: {message}")]
    Syscall {
        /// The operation that failed.
        context: &'static str,
        /// The underlying error's own text.
        message: String,
    },

    /// A failure of the layers underneath (filesystem, JSON, state), with its
    /// own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Engine(e.into())
    }
}

impl From<delonix_state::Error> for Error {
    fn from(e: delonix_state::Error) -> Self {
        Error::Engine(e.into())
    }
}

/// The shared class each `delonix-linux` failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043). A failure of the
    /// layers underneath keeps its own; the match is exhaustive on purpose,
    /// like [`delonix_model::Error::number`]'s.
    pub fn number(&self) -> u16 {
        match self {
            Error::InvalidMemoryLimit(_) => 1102,
            Error::InvalidCpuLimit(_) => 1103,
            Error::InvalidCgroupWeight(_) => 1104,
            Error::InvalidCpuset(_) => 1105,
            Error::InvalidCommandArgv(_) => 1106,
            Error::EmptyCommand(_) => 1107,
            Error::NoProcesses(_) => 1108,
            Error::UnsafeMountPath(_) => 1109,
            Error::MountSourceMissing(_) => 1110,
            Error::SharesHostMountNamespace(_) => 1111,
            Error::LiveMountFailed(_) => 1112,
            Error::LiveUnmountFailed(_) => 1113,
            Error::InvalidCdiDeviceName(_) => 1114,
            Error::KubeCgroupNeedsRoot(_) => 1115,
            Error::NotRunning(_) => 3101,
            Error::CdiDeviceNotFound(_) => 4102,
            Error::AlreadyRunning(_) => 5101,
            Error::ApparmorUnavailable(_) => 1901,
            Error::NoCdiSpec(_) => 1902,
            Error::Syscall { .. } => 9901,
            Error::Engine(e) => e.number(),
        }
    }

    /// «Exists but is not running».
    pub fn is_not_running(&self) -> bool {
        self.number() / 1000 == 3
    }

    /// «No such resource».
    pub fn is_not_found(&self) -> bool {
        self.number() / 1000 == 4
    }

    /// «Already exists»/«already running».
    pub fn is_conflict(&self) -> bool {
        self.number() / 1000 == 5
    }
}

impl From<Error> for Dx {
    fn from(e: Error) -> Self {
        let number = e.number();
        let class = match e {
            Error::InvalidMemoryLimit(t)
            | Error::InvalidCpuLimit(t)
            | Error::InvalidCgroupWeight(t)
            | Error::InvalidCpuset(t)
            | Error::InvalidCommandArgv(t)
            | Error::EmptyCommand(t)
            | Error::NoProcesses(t)
            | Error::UnsafeMountPath(t)
            | Error::MountSourceMissing(t)
            | Error::SharesHostMountNamespace(t)
            | Error::LiveMountFailed(t)
            | Error::LiveUnmountFailed(t)
            | Error::InvalidCdiDeviceName(t)
            | Error::KubeCgroupNeedsRoot(t)
            | Error::ApparmorUnavailable(t)
            | Error::NoCdiSpec(t) => Dx::Invalid(t),
            Error::NotRunning(t) => Dx::NotRunning(t),
            Error::CdiDeviceNotFound(t) => Dx::NotFound(t),
            Error::AlreadyRunning(t) => Dx::Conflict(t),
            Error::Syscall { context, message } => Dx::Runtime { context, message },
            Error::Engine(e) => return e,
        };
        Dx::coded(number, class)
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    fn every_variant() -> Vec<Error> {
        vec![
            Error::InvalidMemoryLimit(
                "--memory 3x: not a size — use bytes (67108864) or a suffix (64M, 64Mi, 1G, 1Gi)"
                    .into(),
            ),
            Error::InvalidCpuLimit("--cpus abc: not a number of cores".into()),
            Error::InvalidCgroupWeight("--cpu-weight 0: weight must be 1–10000".into()),
            Error::InvalidCpuset("--cpuset x: not a CPU list".into()),
            Error::InvalidCommandArgv("invalid argument: \"a\\0b\"".into()),
            Error::EmptyCommand("empty command".into()),
            Error::NoProcesses("no processes in the container".into()),
            Error::UnsafeMountPath("unsafe mount target: /etc/passwd".into()),
            Error::MountSourceMissing("mount source does not exist: /nope".into()),
            Error::SharesHostMountNamespace(
                "container shares the host mnt ns — nothing to mount".into(),
            ),
            Error::LiveMountFailed(
                "failed to mount /a → /b in the live container (code 1)".into(),
            ),
            Error::LiveUnmountFailed("failed to unmount /b in the live container".into()),
            Error::InvalidCdiDeviceName(
                "invalid CDI device name: 'x' (expected vendor.com/class=name)".into(),
            ),
            Error::KubeCgroupNeedsRoot(
                "cgroup parent kubepods.slice: placing a container in the kubelet's cgroup \
                 hierarchy needs the root runtime"
                    .into(),
            ),
            Error::NotRunning("web".into()),
            Error::CdiDeviceNotFound("'nvidia.com/gpu=7': not found in any discovered CDI spec".into()),
            Error::AlreadyRunning("container web is running (use --force)".into()),
            Error::ApparmorUnavailable(
                "--apparmor delonix-default: AppArmor is not enabled on this host, so nothing \
                 would confine this container"
                    .into(),
            ),
            Error::NoCdiSpec("'nvidia.com/gpu=all': no CDI spec found (checked /etc/cdi, /var/run/cdi)".into()),
            Error::Syscall {
                context: "clone",
                message: "EPERM".into(),
            },
            Error::Engine(delonix_model::Error::Conflict("x".into())),
        ]
    }

    #[test]
    fn every_failure_keeps_its_number_through_the_conversion() {
        for e in every_variant() {
            let number = e.number();
            let shown = e.to_string();
            let converted = delonix_model::Error::from(e);
            assert_eq!(converted.number(), number, "{shown}");
            assert!(
                delonix_model::codes::lookup(number).is_some(),
                "DX-{number:04} ({shown}) has no dictionary entry"
            );
        }
    }

    /// The text the CLI printed before this type existed: the shared class
    /// wraps the local variant's message verbatim.
    #[test]
    fn the_converted_message_is_the_one_printed_before() {
        let cases = [
            (
                Error::InvalidCpuLimit("--cpus abc: not a number of cores".into()),
                "invalid argument: --cpus abc: not a number of cores",
            ),
            (Error::NotRunning("web".into()), "container is not running: web"),
            (
                Error::AlreadyRunning("container web is running (use --force)".into()),
                "conflict: container web is running (use --force)",
            ),
            (
                Error::CdiDeviceNotFound(
                    "'nvidia.com/gpu=7': not found in any discovered CDI spec".into(),
                ),
                "no such 'nvidia.com/gpu=7': not found in any discovered CDI spec",
            ),
            (
                Error::Syscall {
                    context: "clone",
                    message: "EPERM".into(),
                },
                "system call `clone` failed: EPERM",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(delonix_model::Error::from(e).to_string(), want);
        }
    }
}
