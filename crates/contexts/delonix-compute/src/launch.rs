//! Starting the workload (`docs/discovery/54_P2_COMPUTE_RUN.md`, step 3c).
//!
//! [`Launch`] is the intent of a start — where the root filesystem is, which
//! network the process joins or creates, which namespaces it inherits — and
//! [`WorkloadRuntime`] is the port that turns it into a running process. The
//! decision of who becomes the process's parent (a supervisor, or the caller)
//! and the cleanup of a start that never happened are the use case, [`start`].

use crate::{Container, Mount};
use delonix_model::records::Status;
use delonix_model::Result;

use crate::RunOpts;

/// What a start needs besides the container record.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    pub rootfs: String,
    pub mounts: Vec<Mount>,
    pub detach: bool,
    /// The re-exec's pass inside a custom network's or a pod's namespace.
    pub second_pass: bool,
    /// `--net none`.
    pub net_none: bool,
    /// The custom network the container is on, if any.
    pub custom_net: Option<String>,
    /// A pod member.
    pub pod: bool,
    /// The address on the custom network or the pod.
    pub attached_ip: Option<String>,
    /// Ports published through the container's own slirp (see [`slirp_ports`]).
    pub slirp_ports: Vec<String>,
    /// The pod infra container whose IPC/UTS a member joins.
    pub pod_infra_pid: Option<i32>,
    /// The AppArmor profile to apply, `unconfined` included.
    pub apparmor: Option<String>,
    /// Where the process's output goes, if anywhere.
    pub log_path: Option<String>,
}

impl Launch {
    /// A netns of its own: `--net none`, or ports through its own slirp — never on
    /// the re-exec's pass, which already runs inside the right one.
    pub fn new_netns(&self) -> bool {
        !self.second_pass && (self.net_none || !self.slirp_ports.is_empty())
    }

    /// On the re-exec's pass the process inherits the holder's user namespace
    /// instead of creating its own.
    pub fn userns(&self, container: &Container) -> bool {
        container.userns && !self.second_pass
    }

    pub fn inherit_userns(&self) -> bool {
        self.second_pass
    }

    /// Pod IPC/UTS sharing only means something on the re-exec's pass, already in
    /// the holder's user namespace where `setns` has privilege.
    pub fn pod_infra_pid(&self) -> Option<i32> {
        if self.second_pass {
            self.pod_infra_pid
        } else {
            None
        }
    }
}

/// The ports published through the container's own slirp. PURE.
///
/// Only without a custom network and outside a pod. A pod member was once
/// included: its netns belongs to the pod, so a SECOND slirp claimed a host port
/// the ingress had already published, and traffic reached neither (measured).
pub fn slirp_ports(custom_net: bool, pod: bool, ports: &[String]) -> Vec<String> {
    if custom_net || pod {
        Vec::new()
    } else {
        ports.to_vec()
    }
}

/// A restart policy that keeps a supervisor around. PURE.
pub fn policy_supervised(policy: &str) -> bool {
    matches!(
        policy.split(':').next().unwrap_or(""),
        "always" | "unless-stopped" | "on-failure"
    )
}

/// Every detached start is supervised when the caller can fork: only the real
/// parent can collect the exit code, whatever the restart policy. PURE.
pub fn should_supervise(_policy: &str, detach: bool, forkable: bool) -> bool {
    detach && forkable
}

/// Runs the workload.
pub trait WorkloadRuntime {
    /// Starts the process with the caller as its parent. In the foreground it
    /// returns once the process has exited, with its status.
    fn create(&self, container: &mut Container, launch: &Launch) -> Result<Status>;
    /// Starts the process under a detached supervisor that becomes its parent.
    fn supervise(&self, container: &mut Container, launch: &Launch, policy: &str) -> Result<()>;
    /// Removes what a start that never happened left behind (rootfs, directory),
    /// unless a record of the container exists.
    fn discard_unstarted(&self, id: &str);
}

/// How a start ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Started {
    /// A supervisor owns the process.
    Supervised,
    /// The caller owned it; in the foreground, this is its final status.
    Created(Status),
}

/// Starts the container: under a supervisor for a detached start the caller can
/// fork for, otherwise as the caller's child. A refused start discards what it
/// left behind.
pub fn start<W: WorkloadRuntime>(
    o: &RunOpts,
    container: &mut Container,
    launch: &Launch,
    runtime: &W,
) -> Result<Started> {
    container.health = o.health.clone();
    if should_supervise(&o.restart, o.detach, !o.no_supervisor) {
        if policy_supervised(&o.restart) {
            container.restart_policy = Some(o.restart.clone());
        }
        if let Err(e) = runtime.supervise(container, launch, &o.restart) {
            runtime.discard_unstarted(&container.id);
            return Err(e);
        }
        return Ok(Started::Supervised);
    }
    match runtime.create(container, launch) {
        Ok(status) => Ok(Started::Created(status)),
        Err(e) => {
            runtime.discard_unstarted(&container.id);
            Err(e)
        }
    }
}

/// Decide whether a container should be restarted, given the policy, the state
/// it died with, and how many times it's already been restarted. **Pure**
/// function — the restart state machine is tested without cloning any processes.
///
/// Docker semantics: `no` never; `on-failure[:max]` only on exit ≠ 0 (or signal),
/// up to `max` attempts (no `max` = no limit); `always`/`unless-stopped` always.
/// The real distinction between `always` and `unless-stopped` is what happens on
/// **host reboot** (`unless-stopped` doesn't resurrect a container the user
/// stopped) — without a daemon doing a boot-time reconcile, here the two behave
/// the same WHILE ALIVE; documented so as not to promise what isn't there.
pub fn should_restart(policy: &str, status: &Status, restarts: u32) -> bool {
    use Status as S;
    let failed = matches!(status, S::Failed(_) | S::Crashed);
    let (kind, max) = match policy.split_once(':') {
        Some((k, m)) => (k, m.parse::<u32>().ok()),
        None => (policy, None),
    };
    match kind {
        "always" | "unless-stopped" => true,
        "on-failure" => failed && max.map(|m| restarts < m).unwrap_or(true),
        _ => false, // "no" and anything unknown: don't restart
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn restart_policy_docker_semantics() {
        use delonix_model::records::Status as S;
        // `no` (and unknown ones): never restarts, however it died.
        for st in [S::Stopped, S::Failed(1), S::Crashed] {
            assert!(!should_restart("no", &st, 0));
            assert!(!should_restart("qualquer-coisa", &st, 0));
        }
        // `always`/`unless-stopped`: always, even on a clean exit.
        for p in ["always", "unless-stopped"] {
            assert!(should_restart(p, &S::Stopped, 0));
            assert!(should_restart(p, &S::Failed(1), 99));
            assert!(should_restart(p, &S::Crashed, 99));
        }
        // `on-failure`: only on failure; exit 0 stops.
        assert!(!should_restart("on-failure", &S::Stopped, 0));
        assert!(should_restart("on-failure", &S::Failed(2), 0));
        assert!(should_restart("on-failure", &S::Crashed, 0));
        // `on-failure:max` respects the cap (the `max` counts RESTARTS already done).
        assert!(should_restart("on-failure:3", &S::Failed(1), 2));
        assert!(!should_restart("on-failure:3", &S::Failed(1), 3));
        assert!(!should_restart("on-failure:0", &S::Failed(1), 0));
        // `on-failure` without `max` has no cap.
        assert!(should_restart("on-failure", &S::Failed(1), 10_000));
    }

    use super::*;
    use delonix_model::Error;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeRuntime {
        calls: RefCell<Vec<String>>,
        fail: bool,
    }

    impl WorkloadRuntime for FakeRuntime {
        fn create(&self, c: &mut Container, _: &Launch) -> Result<Status> {
            self.calls.borrow_mut().push(format!("create {}", c.id));
            if self.fail {
                return Err(Error::Invalid("clone failed".into()));
            }
            Ok(Status::Stopped)
        }
        fn supervise(&self, c: &mut Container, _: &Launch, policy: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("supervise {} {policy}", c.id));
            if self.fail {
                return Err(Error::Invalid("clone failed".into()));
            }
            Ok(())
        }
        fn discard_unstarted(&self, id: &str) {
            self.calls.borrow_mut().push(format!("discard {id}"));
        }
    }

    fn container() -> Container {
        Container::new(
            "c1".into(),
            "web".into(),
            "img".into(),
            vec!["sh".into()],
            "64M".into(),
        )
    }

    #[test]
    fn a_detached_start_is_supervised_and_records_a_supervised_policy() {
        let rt = FakeRuntime::default();
        let o = RunOpts {
            detach: true,
            restart: "on-failure:3".into(),
            ..Default::default()
        };
        let mut c = container();
        assert_eq!(
            start(&o, &mut c, &Launch::default(), &rt).unwrap(),
            Started::Supervised
        );
        assert_eq!(c.restart_policy.as_deref(), Some("on-failure:3"));
        assert_eq!(*rt.calls.borrow(), ["supervise c1 on-failure:3"]);

        let o = RunOpts {
            detach: true,
            restart: "no".into(),
            ..Default::default()
        };
        let mut c = container();
        start(&o, &mut c, &Launch::default(), &FakeRuntime::default()).unwrap();
        assert_eq!(c.restart_policy, None);
    }

    #[test]
    fn foreground_or_unforkable_is_created_by_the_caller() {
        let rt = FakeRuntime::default();
        let mut c = container();
        let o = RunOpts {
            restart: "always".into(),
            ..Default::default()
        };
        assert_eq!(
            start(&o, &mut c, &Launch::default(), &rt).unwrap(),
            Started::Created(Status::Stopped)
        );
        let o = RunOpts {
            detach: true,
            no_supervisor: true,
            ..Default::default()
        };
        start(&o, &mut container(), &Launch::default(), &rt).unwrap();
        assert_eq!(*rt.calls.borrow(), ["create c1", "create c1"]);
    }

    #[test]
    fn a_refused_start_discards_what_it_left() {
        for detach in [true, false] {
            let rt = FakeRuntime {
                fail: true,
                ..Default::default()
            };
            let o = RunOpts {
                detach,
                ..Default::default()
            };
            assert!(start(&o, &mut container(), &Launch::default(), &rt).is_err());
            assert_eq!(
                rt.calls.borrow().last().map(String::as_str),
                Some("discard c1")
            );
        }
    }

    #[test]
    fn the_namespaces_follow_the_pass() {
        let first = Launch {
            slirp_ports: vec!["8080:80".into()],
            pod_infra_pid: Some(7),
            ..Default::default()
        };
        let mut c = container();
        c.userns = true;
        assert!(first.new_netns() && first.userns(&c) && !first.inherit_userns());
        assert_eq!(first.pod_infra_pid(), None);
        let second = Launch {
            second_pass: true,
            ..first
        };
        assert!(!second.new_netns() && !second.userns(&c) && second.inherit_userns());
        assert_eq!(second.pod_infra_pid(), Some(7));
        assert!(Launch {
            net_none: true,
            ..Default::default()
        }
        .new_netns());
    }

    #[test]
    fn slirp_ports_only_without_a_network_or_a_pod() {
        let p = vec!["8080:80".to_string()];
        assert_eq!(slirp_ports(false, false, &p), p);
        assert!(slirp_ports(true, false, &p).is_empty());
        assert!(slirp_ports(false, true, &p).is_empty());
    }
}
