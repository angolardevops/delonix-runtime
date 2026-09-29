//! The VM backend registry (ADR-0008, ADR-0044 D4 and P4b.4a): the table the
//! VM use cases select a backend from, and the rules a registration has to
//! pass. It lives with the port it selects between; what fills it is the
//! composition root's business — it SEEDS the backends it trusts to answer
//! `available()` locally (only those may be auto-selectable, and they stay in
//! front, in the order given), then others register behind them by name.
//!
//! This is a map populated at startup, **not a plugin system** (ADR-0008):
//! nothing loads a `.so`, and the only ways in are [`seed`] and
//! [`register_backend`], called by a process that already linked the backend's
//! crate. On the day somebody proposes loading code at runtime, that is a new ADR.

use crate::capability::Capability;
use crate::vm_backend::{BackendRegistration, VmBackend};
use crate::vm_error::{Error, Result};
use crate::Vm;

/// The table, the ids it was seeded with, and the hints for names a build
/// knows but has not registered.
#[derive(Default)]
pub struct Registry {
    entries: Vec<BackendRegistration>,
    seeded: Vec<&'static str>,
    hints: Vec<(&'static str, &'static str)>,
}

static REGISTRY: std::sync::OnceLock<std::sync::RwLock<Registry>> = std::sync::OnceLock::new();

fn registry() -> &'static std::sync::RwLock<Registry> {
    REGISTRY.get_or_init(|| std::sync::RwLock::new(Registry::default()))
}

/// Removes a registration by id. For tests that register a fake backend in
/// the process-wide table and must leave it as they found it.
#[doc(hidden)]
pub fn deregister(id: &str) {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .retain(|b| b.id != id);
}

/// Seeds the registry with the backends the composition root trusts to answer
/// `available()` without a network round trip. **Order matters**: it is the
/// preference order of the auto-detection, and the seeded entries go IN FRONT
/// of anything registered before, so registering a third backend never changes
/// what an existing host picks. `hints` name backends a build knows but that
/// need configuration, so an unknown-name error can say what to do. Only the
/// first seed counts.
pub fn seed(regs: Vec<BackendRegistration>, hints: &[(&'static str, &'static str)]) {
    let mut g = registry().write().unwrap_or_else(|e| e.into_inner());
    if !g.seeded.is_empty() {
        return;
    }
    g.seeded = regs.iter().map(|r| r.id).collect();
    g.hints = hints.to_vec();
    let rest = std::mem::take(&mut g.entries);
    g.entries = regs;
    let seeded = g.seeded.clone();
    g.entries
        .extend(rest.into_iter().filter(|r| !seeded.contains(&r.id)));
}

/// Runs `f` over the registered backends. A helper because every reader needs
/// the same lock, and a poisoned lock here must not take the process down: a
/// panic in an unrelated thread is not a reason for `vm ls` to abort.
pub fn with_backends<T>(f: impl FnOnce(&[BackendRegistration]) -> T) -> T {
    let guard = registry().read().unwrap_or_else(|e| e.into_inner());
    f(&guard.entries)
}
/// Every registered backend's capability report (ADR-0050), in registry
/// order, which is the order auto-detection prefers. Each report is built
/// NOW, so a local backend probes this host and a remote one declares without
/// connecting (its own `report` factory is responsible for that).
pub fn provider_reports() -> Vec<crate::capability::ProviderReport> {
    with_backends(|regs| regs.iter().map(|r| (r.report)()).collect())
}
/// Adds a backend to the registry. Idempotent by id: registering the same id
/// twice REPLACES the entry, so a process that configures a target twice ends
/// up with the last one rather than two that shadow each other.
///
/// The caller is a process that linked the backend's crate and knows its
/// configuration — for a remote backend, that is where the endpoint and the
/// credential come from. **Nothing here does I/O**: the factory is not called,
/// so registering a node that is unreachable costs nothing until someone
/// actually selects it.
///
/// Refused, rather than accepted and left to surprise someone later:
///
/// * an id or alias that collides with a DIFFERENT backend already registered —
///   the loser would become unreachable by name, silently;
/// * a `auto_selectable: true` on a backend that is not one of this crate's
///   own. Auto-detection walks the table asking `available()`, and a remote
///   backend cannot answer that without a network round trip (ADR-0008). A
///   third-party backend is selected by name or not at all.
pub fn register_backend(reg: BackendRegistration) -> Result<()> {
    if reg.id.trim().is_empty() {
        return Err(Error::BackendRegistrationRefused(
            "a backend registration needs an id".into(),
        ));
    }
    let seeded = registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .seeded
        .clone();
    if reg.auto_selectable && !seeded.contains(&reg.id) {
        return Err(Error::BackendRegistrationRefused(format!(
            "backend '{}' cannot be auto-selectable: auto-detection asks every candidate \
             `available()`, and a backend registered from outside this crate may only be able to \
             answer that over the network. Register it with `auto_selectable: false` and select it \
             by name (`--backend {}`)",
            reg.id, reg.id
        )));
    }
    let mut reg_guard = registry().write().unwrap_or_else(|e| e.into_inner());
    let guard = &mut reg_guard.entries;
    // A name that already belongs to somebody else. Checked against every
    // OTHER entry, so re-registering the same id (a reconfigured target) is
    // fine while stealing another's alias is not.
    for name in std::iter::once(&reg.id).chain(reg.aliases.iter()) {
        let want = name.trim().to_lowercase();
        if let Some(clash) = guard
            .iter()
            .find(|b| b.id != reg.id && (b.id == want || b.aliases.contains(&want.as_str())))
        {
            return Err(Error::BackendRegistrationRefused(format!(
                "backend '{}' cannot claim the name '{}': it already belongs to '{}'",
                reg.id, name, clash.id
            )));
        }
    }
    guard.retain(|b| b.id != reg.id);
    guard.push(reg);
    Ok(())
}
/// Builds the backend `name` resolves to, or `None` if nothing does.
pub fn make_backend(name: &str) -> Option<Result<Box<dyn VmBackend>>> {
    let want = name.trim().to_lowercase();
    with_backends(|bs| {
        bs.iter()
            .find(|b| b.id == want || b.aliases.contains(&want.as_str()))
            .map(|b| (b.new)())
    })
}
/// The registered ids, for an error that names what IS accepted. Derived from
/// the table so it cannot drift from it.
pub fn registered_backend_ids() -> String {
    with_backends(|bs| {
        bs.iter()
            .map(|b| format!("'{}'", b.id))
            .collect::<Vec<_>>()
            .join(", ")
    })
}
pub fn unknown_backend(name: &str) -> Error {
    let want = name.trim().to_lowercase();
    if let Some((_, why)) = registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .hints
        .iter()
        .find(|(id, _)| *id == want)
    {
        return Error::BackendNotConfigured(format!(
            "VM backend '{}' is not available in this build: {why}",
            name.trim()
        ));
    }
    Error::UnknownBackend(format!(
        "unknown VM backend: '{}' (use {})",
        name.trim(),
        registered_backend_ids()
    ))
}
/// Normalizes any accepted alias (`ch`, `cloudhypervisor`, `kvm`, `qemu`, …)
/// to the canonical backend id. `None` for an empty string (the "no opinion"
/// case, distinct from an unknown name — callers that need to reject unknown
/// names do so themselves, since an empty string is valid here but not in
/// [`select_backend`]'s explicit-request arm).
pub fn canonical_backend_name(s: &str) -> Option<&'static str> {
    let want = s.trim().to_lowercase();
    with_backends(|bs| {
        bs.iter()
            .find(|b| b.id == want || b.aliases.contains(&want.as_str()))
            .map(|b| b.id)
    })
}
/// Selects a backend from an explicit request or by auto-detection (the first
/// registered entry that is actually installed — today cloud-hypervisor, then
/// libvirt).
pub fn select_backend(want: Option<&str>) -> Result<Box<dyn VmBackend>> {
    select_backend_requiring(want, &[])
}
/// [`select_backend`] with the caller's requirements (ADR-0050 D6): a named
/// backend is refused unless its report on this host marks every entry of
/// `required` usable, and auto-detection skips a candidate that does not.
/// With `required` empty this is exactly [`select_backend`].
pub fn select_backend_requiring(
    want: Option<&str>,
    required: &[Capability],
) -> Result<Box<dyn VmBackend>> {
    match want.map(str::trim) {
        Some(other) if !other.is_empty() => {
            let b = make_backend(other).unwrap_or_else(|| Err(unknown_backend(other)))?;
            require_capabilities(b.id(), required)?;
            Ok(b)
        }
        _ => with_backends(|regs| auto_detect(regs, required)),
    }
}
/// Refuses `backend_id` unless its report on THIS host marks every entry of
/// `required` usable (the contract's `Capability.supported`: `supported` or
/// `partial`). The refusal names each unmet entry with the provider's own
/// state and reason — the same words `provider describe` prints — so the
/// caller knows whether to pick another provider, install something on this
/// host, or drop the requirement.
pub fn require_capabilities(backend_id: &str, required: &[Capability]) -> Result<()> {
    if required.is_empty() {
        return Ok(());
    }
    let report = with_backends(|regs| report_of(regs, backend_id));
    let Some(report) = report else {
        return Err(unknown_backend(backend_id));
    };
    let missing = unmet(&report, required);
    if missing.is_empty() {
        return Ok(());
    }
    Err(capability_not_supported(&report.id, &missing))
}
pub fn report_of(
    regs: &[BackendRegistration],
    id: &str,
) -> Option<crate::capability::ProviderReport> {
    regs.iter()
        .find(|r| r.id == id || r.aliases.contains(&id))
        .map(|r| (r.report)())
}
/// `name: state — detail` for every required entry the report does not mark
/// usable. An entry the report lacks (a kind mismatch) is "not in this
/// provider's report", which is a "no" too.
pub fn unmet(report: &crate::capability::ProviderReport, required: &[Capability]) -> Vec<String> {
    required
        .iter()
        .filter_map(
            |c| match report.capabilities.iter().find(|r| r.capability == *c) {
                Some(r) if r.state.is_usable() => None,
                Some(r) => {
                    let detail = r.state.detail();
                    Some(if detail.is_empty() {
                        format!("{}: {}", c.name(), r.state.label())
                    } else {
                        format!("{}: {} — {}", c.name(), r.state.label(), detail)
                    })
                }
                None => Some(format!("{}: not in this provider's report", c.name())),
            },
        )
        .collect()
}
fn capability_not_supported(id: &str, missing: &[String]) -> Error {
    Error::CapabilityNotSupported(format!(
        "the '{id}' backend does not support what this VM requires on this host:\n  {}\nPick a \
         backend that does (`delonix provider ls`), or drop the requirement",
        missing.join("\n  ")
    ))
}
/// Auto-detection: the first registered entry that is auto-selectable AND
/// installed.
///
/// **The `auto_selectable` filter reads the REGISTRATION and runs before
/// anything is built**, and that order is the whole point. It used to be
/// `.map(|b| (b.new)()).filter(|b| b.auto_selectable())` — every candidate was
/// constructed and the wrong ones thrown away. For a local backend that is free
/// (both are unit structs), which is why nothing ever noticed; for a remote one
/// CONSTRUCTION is where authentication happens, so auto-detection made exactly
/// the network round trip the flag exists to prevent.
///
/// Takes the entries rather than reading the registry, so a test can hand it a
/// table where the skipped candidate is actually REACHED. Against the global
/// registry it never is: this host has a local backend installed, the walk
/// stops at the first one, and a test written that way passes whether the order
/// is right or wrong.
///
/// With `required` non-empty (ADR-0050 D6) the walk also asks each candidate's
/// REPORT — built here, which for a local backend probes this host and for a
/// remote one declares without connecting — and skips one that does not mark
/// every required entry usable. When every available candidate is skipped that
/// way, the refusal names what each one lacks: "no backend" would send the
/// caller to install a hypervisor it already has.
pub fn auto_detect(
    entries: &[BackendRegistration],
    required: &[Capability],
) -> Result<Box<dyn VmBackend>> {
    let mut skipped: Vec<String> = Vec::new();
    for e in entries.iter().filter(|b| b.auto_selectable) {
        // A built-in constructor is infallible; a registered one that fails is
        // not a reason to abort a walk whose next candidate may serve fine.
        if let Ok(b) = (e.new)() {
            if b.available() {
                if required.is_empty() {
                    return Ok(b);
                }
                let missing = unmet(&(e.report)(), required);
                if missing.is_empty() {
                    return Ok(b);
                }
                skipped.push(format!("{}:\n  {}", e.id, missing.join("\n  ")));
            }
        }
    }
    if skipped.is_empty() {
        return Err(Error::NoBackendAvailable(
            "no VM backend available: install 'cloud-hypervisor' or 'libvirt'+'qemu'".into(),
        ));
    }
    Err(Error::CapabilityNotSupported(format!(
        "no available VM backend supports what this VM requires on this host:\n{}\nInstall or \
         configure one that does (`delonix provider ls`), or drop the requirement",
        skipped.join("\n")
    )))
}
/// The backend that started an already-persisted VM (for liveness/stop).
///
/// Resolved through [`BACKENDS`], and **fail-closed**: it used to end in
/// `_ => CloudHypervisorBackend`, which is the worst place in this crate for a
/// silent default. Unlike [`select_backend`], which answers "what should run
/// this?", this one answers "what IS running this?" — and getting that wrong
/// does not fail, it LIES: `is_running` on a live libvirt VM through the Cloud
/// Hypervisor backend reports it stopped, and `stop` then tears down the record
/// of a guest that is still up.
///
/// `vm.backend` is written by `create` from `valid_backend_name`, so a record
/// this cannot resolve means the file was hand-edited or written by a build
/// that knew a backend this one does not. Both deserve a sentence naming the
/// VM and the value, not a guess.
pub fn backend_for(vm: &Vm) -> Result<Box<dyn VmBackend>> {
    make_backend(&vm.backend).unwrap_or_else(|| {
        Err(Error::UnregisteredBackendInRecord(format!(
            "vm '{}': its record names backend '{}', which this process does not have registered \
             (it has {})",
            vm.name,
            vm.backend,
            registered_backend_ids()
        )))
    })
}
/// Whether the backend registered as `backend_id` DECLARES `cap` (ADR-0050),
/// regardless of what this host has installed: the question about a provider's
/// nature, which is what the two predicates above ask. The registration's
/// report factory probes the host (three `which`, a `/dev/kvm` stat and one
/// `virsh uri` — ~10 ms measured), and `declared_usable` puts a declared "yes"
/// the probe narrowed to `unavailable-on-host` back; a `--require` keeps
/// asking `is_usable`, because a request has to run HERE. Unknown id: `false`,
/// never a guess — a backend nobody registered cannot supervise anything.
pub fn backend_declares(backend_id: &str, cap: Capability) -> bool {
    with_backends(|regs| report_of(regs, backend_id))
        .map(|report| {
            report
                .capabilities
                .iter()
                .any(|c| c.capability == cap && c.state.declared_usable())
        })
        .unwrap_or(false)
}
/// Does the backend that would run this VM own its own storage?
///
/// For a caller that has to decide something BEFORE `create_with` — the CLI
/// generating a NoCloud seed ISO, which is a file on this filesystem and
/// therefore meaningless to a hypervisor on another machine. Without this the
/// CLI built one anyway and handed over a path the node cannot read.
///
/// Same resolution as [`select_backend`], so the answer is about the backend
/// that will actually be used. `false` when nothing resolves: the caller then
/// keeps its old behaviour and the real error comes from `create_with`, which
/// is where it reads properly.
pub fn backend_manages_own_storage(want: Option<&str>) -> bool {
    select_backend(want)
        .map(|b| b.manages_own_storage())
        .unwrap_or(false)
}
/// Validates and normalizes a backend name for external callers (the CLI's
/// `HYPERVISOR` VMfile instruction, `vm default-backend --set`) — same
/// acceptance rules as [`select_backend`]'s explicit-request arm, without
/// needing to construct a [`VmBackend`] just to validate a string.
pub fn valid_backend_name(s: &str) -> Result<&'static str> {
    canonical_backend_name(s).ok_or_else(|| unknown_backend(s))
}
/// The primary NIC's MAC, DERIVED from the VM name — the same value both
/// backends stamp on the interface they create, and therefore the one thing
/// about the guest's network that is knowable before the guest exists.
///
/// `pub` because the seed generator needs it: a NoCloud `network-config` that
/// matches the NIC by name has to guess a name (`eth0`? `ens3`? `enp1s0`?),
/// and guessing wrong is silent. Matching by MAC is exact. One formula, one
/// caller-visible function — a second copy of this arithmetic would diverge the
/// day the vendor prefix changed, and the symptom would be a guest configuring
/// a NIC that is not there.
pub fn mac_for(name: &str) -> String {
    let h = delonix_net_rules::fnv32(name);
    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        (h >> 16) & 0xff,
        (h >> 8) & 0xff,
        h & 0xff
    )
}
