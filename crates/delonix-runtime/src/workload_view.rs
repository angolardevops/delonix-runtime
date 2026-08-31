//! What each running workload ASKED FOR, and what the host actually enforced.
//!
//! `resource_advice` answers «can this host enforce anything», once, for the
//! whole machine. This answers the question after it: «and did MY container's
//! limits land?» — which is not the same question, and on a mixed host not the
//! same answer. A workload started before a delegation was fixed keeps running
//! without the ceiling it asked for, and nothing said so.
//!
//! The comparison is against what the ENGINE would have written, computed by
//! the engine's own converters, not against a value re-derived here. A second
//! implementation of «what does `--memory 512M` mean in cgroup terms» would
//! drift, and the first sign of the drift would be this report calling a
//! correct host broken.

/// One limit the user asked for, and what became of it.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitCheck {
    /// The flag the user typed.
    pub flag: &'static str,
    /// What they typed after it.
    pub requested: String,
    /// What the engine would write into the cgroup for that request.
    pub expected: String,
    /// What the cgroup file says now. `None` when the file is not there — which
    /// on a rootless host is how a missing controller looks.
    pub actual: Option<String>,
}

impl LimitCheck {
    /// `true` if the request is in force.
    pub fn applied(&self) -> bool {
        self.actual.as_deref() == Some(self.expected.as_str())
    }
    /// Why it is not, in the words the operator needs. Empty when it IS.
    pub fn diagnosis(&self) -> &'static str {
        match self.actual.as_deref() {
            Some(a) if a == self.expected => "",
            // The file is not there at all: the controller was never delegated,
            // so the flag was parsed, accepted and dropped.
            None => "controller not delegated — the flag was accepted and ignored",
            // The file is there and says something else. Either somebody wrote
            // it by hand, or the regulator is holding it down.
            Some(_) => "in force with a different value (regulated, or set by hand)",
        }
    }
}

/// Everything a workload asked for, paired with the reading of its own cgroup.
#[derive(Debug, Clone)]
pub struct WorkloadView {
    pub id: String,
    pub name: String,
    pub cgroup: String,
    pub limits: Vec<LimitCheck>,
    /// The workload's OWN stall, per resource. Present for `io` even where the
    /// io controller is not delegated: the kernel accounts the stall whether or
    /// not anybody can cap it, so on a rootless host this is the only honest
    /// answer to «who is waiting for the disk».
    pub psi: Vec<(&'static str, Option<crate::Psi>)>,
}

impl WorkloadView {
    /// The limits that did NOT land — the reason this type exists.
    pub fn ignored(&self) -> Vec<&LimitCheck> {
        self.limits.iter().filter(|l| !l.applied()).collect()
    }
}

/// The requests a container carries, as `(flag, value, cgroup file, expected)`.
///
/// Pure: it turns a record into what SHOULD be on disk, and takes no reading.
/// Everything that makes this hard to get right — the memory suffixes, the
/// `cpu.max` period — is done by the same functions the container-start path
/// uses.
pub fn expected_limits(
    c: &delonix_runtime_core::Container,
) -> Vec<(&'static str, String, &'static str, String)> {
    let mut out = Vec::new();
    if !c.memory_max.is_empty() && c.memory_max != "0" {
        out.push((
            "--memory",
            c.memory_max.clone(),
            "memory.max",
            crate::mem_bytes(&c.memory_max).to_string(),
        ));
    }
    // `cpus` is mandatory in the record and defaults to the whole machine; a
    // request for everything is not a limit anybody is checking on.
    if !c.cpus.is_empty() && c.cpus != "0" {
        out.push((
            "--cpus",
            c.cpus.clone(),
            "cpu.max",
            crate::cpu_max_for(&c.cpus),
        ));
    }
    if let Some(w) = &c.cpu_weight {
        out.push(("--cpu-weight", w.clone(), "cpu.weight", w.clone()));
    }
    if let Some(s) = &c.cpuset {
        out.push(("--cpuset", s.clone(), "cpuset.cpus", s.clone()));
    }
    if let Some(w) = &c.io_weight {
        out.push(("--io-weight", w.clone(), "io.weight", w.clone()));
    }
    // No `--pids-limit` here: the container record does not carry one (the
    // ceiling lives on the cgroup GROUP, not the container), so there is no
    // request to compare a reading against. Inventing one would report a limit
    // nobody asked for.
    out
}

/// Reads one workload's cgroup and pairs every request with what is there.
pub fn view(c: &delonix_runtime_core::Container) -> WorkloadView {
    let cgroup = crate::live_cgroup(c);
    let limits = expected_limits(c)
        .into_iter()
        .map(|(flag, requested, file, expected)| LimitCheck {
            flag,
            requested,
            expected,
            actual: std::fs::read_to_string(format!("{cgroup}/{file}"))
                .ok()
                .map(|s| s.trim().to_string()),
        })
        .collect();
    let psi = ["cpu", "memory", "io"]
        .into_iter()
        .map(|r| {
            (
                r,
                crate::parse_psi_some(
                    &std::fs::read_to_string(format!("{cgroup}/{r}.pressure")).unwrap_or_default(),
                ),
            )
        })
        .collect();
    WorkloadView {
        id: c.id.clone(),
        name: c.name.clone(),
        cgroup,
        limits,
        psi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(actual: Option<&str>) -> LimitCheck {
        LimitCheck {
            flag: "--memory",
            requested: "512M".into(),
            expected: "536870912".into(),
            actual: actual.map(str::to_string),
        }
    }

    #[test]
    fn a_limit_is_in_force_only_when_the_file_says_exactly_it() {
        assert!(check(Some("536870912")).applied());
        assert!(check(Some("536870912")).diagnosis().is_empty());

        // The file is missing: on a rootless host that is what a controller
        // nobody delegated looks like, and it is the case worth a sentence.
        let missing = check(None);
        assert!(!missing.applied());
        assert!(missing.diagnosis().contains("accepted and ignored"));

        // `max` is the kernel default — the request did not land.
        let unset = check(Some("max"));
        assert!(!unset.applied());
        assert!(unset.diagnosis().contains("different value"));

        // Someone (or the regulator) holds it lower. Also not what was asked.
        assert!(!check(Some("268435456")).applied());
    }

    #[test]
    fn only_what_the_user_actually_asked_for_is_checked() {
        let mut c = delonix_runtime_core::Container::new(
            "id".into(),
            "db".into(),
            "img".into(),
            vec![],
            "512M".into(),
        );
        c.cpus = "2".into();
        let flags: Vec<&str> = expected_limits(&c).iter().map(|(f, ..)| *f).collect();
        assert_eq!(flags, vec!["--memory", "--cpus"]);

        // The optional knobs appear only when set — a report listing every knob
        // the engine HAS would bury the two the operator chose.
        c.cpuset = Some("0-3".into());
        c.io_weight = Some("50".into());
        let flags: Vec<&str> = expected_limits(&c).iter().map(|(f, ..)| *f).collect();
        assert_eq!(flags, vec!["--memory", "--cpus", "--cpuset", "--io-weight"]);
    }

    #[test]
    fn the_expected_value_comes_from_the_engines_own_converters() {
        let mut c = delonix_runtime_core::Container::new(
            "id".into(),
            "db".into(),
            "img".into(),
            vec![],
            "1G".into(),
        );
        c.cpus = "0.5".into();
        let by_flag: std::collections::HashMap<_, _> = expected_limits(&c)
            .into_iter()
            .map(|(f, _, file, exp)| (f, (file, exp)))
            .collect();
        // Not re-derived here: `1G` and `0.5` mean whatever the start path says
        // they mean, or this report calls a correct host broken.
        assert_eq!(
            by_flag["--memory"],
            ("memory.max", 1073741824u64.to_string())
        );
        assert_eq!(by_flag["--cpus"].0, "cpu.max");
        assert_eq!(by_flag["--cpus"].1, crate::cpu_max_for("0.5"));
    }

    #[test]
    fn a_view_reports_only_the_limits_that_did_not_land() {
        let v = WorkloadView {
            id: "x".into(),
            name: "db".into(),
            cgroup: "/sys/fs/cgroup/x".into(),
            limits: vec![
                check(Some("536870912")),
                LimitCheck {
                    flag: "--cpuset",
                    requested: "0-3".into(),
                    expected: "0-3".into(),
                    actual: None,
                },
            ],
            psi: vec![],
        };
        let ignored = v.ignored();
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].flag, "--cpuset");
    }
}
