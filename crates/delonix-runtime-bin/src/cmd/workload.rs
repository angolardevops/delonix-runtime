//! `kind: Workload` — a thin lowering layer over the existing compute Kinds
//! (`Container`/`Vm`). See `docs/adr/0001-workload-kind-schema.md`.
//!
//! A `Workload` is sugar: it does NOT survive [`super::manifest::load`]. It is
//! rewritten into a synthetic `kind: Container`/`kind: Vm` doc (inheriting the
//! Workload's `metadata`) that then flows through the normal per-Kind apply —
//! exactly like a `kind: Stack` child. Nothing downstream (`apply`, per-Kind
//! `apply -f`, `stack apply`, `--dry-run`, `ls`, `describe`) needs new wiring.
//!
//! The block that carries the underlying spec is named after the type
//! (`spec.container` / `spec.vm`) and is deserialized by the SAME typed structs
//! the standalone Kinds use (`ContainerSpec`/`VmSpec`) — the Workload spec cannot
//! drift from the spec it wraps, because it does not redefine a single field.

use clap::Subcommand;
use delonix_runtime_core::{Error, Result};
use serde::Deserialize;

use super::manifest::ManifestDoc;

/// Top-level `spec` keys a `kind: Workload` accepts (drives the unknown-field warning).
pub const WORKLOAD_SPEC_FIELDS: &[&str] = &["type", "container", "vm"];

/// The `spec` of a `kind: Workload`: a `type` discriminator plus the single
/// type-named block that holds the underlying spec, kept raw until the target
/// Kind re-deserializes it.
#[derive(Debug, Deserialize)]
struct WorkloadSpec {
    #[serde(rename = "type", default)]
    workload_type: String,
    #[serde(default)]
    container: Option<serde_yaml::Value>,
    #[serde(default)]
    vm: Option<serde_yaml::Value>,
}

/// Lowers a `kind: Workload` doc into its underlying `kind: Container`/`kind: Vm`
/// doc. Fail-closed: an unsupported/reserved `type`, a missing block, or a block
/// that does not match the `type` is an explicit error — never silently ignored,
/// never defaulted (guardrail: no silent failure).
pub fn lower_workload(doc: &ManifestDoc) -> Result<ManifestDoc> {
    super::manifest::warn_unknown_fields(doc, WORKLOAD_SPEC_FIELDS);
    let spec: WorkloadSpec = super::manifest::spec_of(doc)?;
    let name = doc.metadata.name.clone();
    let ty = spec.workload_type.trim().to_ascii_lowercase();

    let (child_kind, block) = match ty.as_str() {
        "container" => {
            if spec.vm.is_some() {
                return Err(mismatch(&name, "container", "vm"));
            }
            (
                "Container",
                spec.container.ok_or_else(|| missing_block(&name, "container"))?,
            )
        }
        "vm" => {
            if spec.container.is_some() {
                return Err(mismatch(&name, "vm", "container"));
            }
            ("Vm", spec.vm.ok_or_else(|| missing_block(&name, "vm"))?)
        }
        "" => {
            return Err(Error::Invalid(super::po::tf(
                "workload '{name}': spec.type is required (container | vm)",
                &[("name", &name)],
            )))
        }
        // Reserved: recognized by name so the error is targeted ("use kind: Pod")
        // instead of a generic "unknown type". Each is a future ADR, never a
        // silent acceptance.
        "pod" => {
            return Err(Error::Invalid(super::po::tf(
                "workload '{name}': type: pod not yet supported — use kind: Pod for multi-container pods",
                &[("name", &name)],
            )))
        }
        "microvm" => {
            return Err(Error::Invalid(super::po::tf(
                "workload '{name}': type: microvm not yet supported — use type: vm",
                &[("name", &name)],
            )))
        }
        other => {
            return Err(Error::Invalid(super::po::tf(
                "workload '{name}': unknown type '{type}' (supported: container, vm)",
                &[("name", &name), ("type", other)],
            )))
        }
    };

    Ok(ManifestDoc {
        api_version: doc.api_version.clone(),
        kind: child_kind.to_string(),
        // Inherit name/namespace/labels/annotations from the Workload.
        metadata: doc.metadata.clone(),
        spec: block,
    })
}

fn missing_block(name: &str, ty: &str) -> Error {
    Error::Invalid(super::po::tf(
        "workload '{name}': type: {type} requires a '{type}:' block",
        &[("name", name), ("type", ty)],
    ))
}

fn mismatch(name: &str, ty: &str, other: &str) -> Error {
    Error::Invalid(super::po::tf(
        "workload '{name}': type: {type} must not carry a '{other}:' block",
        &[("name", name), ("type", ty), ("other", other)],
    ))
}

// ======================================================================
// Unified compute layer (ADR-0002, Phase 2a) — `delonix workload` commands
// ======================================================================

/// One row in the unified `delonix workload ls` — the small common denominator
/// across compute types. Each engine's adapter (`cmd::container`/`cmd::vm`) fills it.
pub struct WorkloadRow {
    /// `"container"` | `"vm"`.
    pub kind: &'static str,
    pub name: String,
    /// Human, already-reconciled status (`Up 3m`, `Running`, `Exited (0)`…).
    pub status: String,
    /// Type-specific one-liner: the image (container) or `"2 vCPU, 4G"` (vm).
    pub info: String,
}

/// A compute backend the Workload layer drives uniformly (ADR-0002). The
/// container and vm engines each provide an adapter; `delonix workload` routes
/// across them BY NAME. Minimal by design — every method has a real caller
/// (`ls` uses `list`; `stop`/`rm` use `owns` + `stop`/`remove`). Grows a verb at
/// a time, never ahead of a caller.
pub trait ComputeDriver {
    /// Enumerate this backend's workloads for the unified `ls`.
    fn list(&self) -> Result<Vec<WorkloadRow>>;
    /// `true` if a workload with this exact name lives on this backend.
    fn owns(&self, name: &str) -> Result<bool>;
    /// Stop it (delegates to the engine's own stop, unchanged).
    fn stop(&self, name: &str) -> Result<()>;
    /// Remove it (delegates to the engine's own rm, unchanged).
    fn remove(&self, name: &str, force: bool) -> Result<()>;
    /// Print its full description (delegates to the engine's own `describe`).
    fn describe(&self, name: &str) -> Result<()>;
}

struct ContainerDriver;
struct VmDriver;

impl ComputeDriver for ContainerDriver {
    fn list(&self) -> Result<Vec<WorkloadRow>> {
        super::container::workload_rows()
    }
    fn owns(&self, name: &str) -> Result<bool> {
        super::container::workload_owns(name)
    }
    fn stop(&self, name: &str) -> Result<()> {
        super::container::workload_stop(name)
    }
    fn remove(&self, name: &str, force: bool) -> Result<()> {
        super::container::workload_remove(name, force)
    }
    fn describe(&self, name: &str) -> Result<()> {
        super::container::workload_describe(name)
    }
}

impl ComputeDriver for VmDriver {
    fn list(&self) -> Result<Vec<WorkloadRow>> {
        super::vm::workload_rows()
    }
    fn owns(&self, name: &str) -> Result<bool> {
        super::vm::workload_owns(name)
    }
    fn stop(&self, name: &str) -> Result<()> {
        super::vm::workload_stop(name)
    }
    fn remove(&self, name: &str, force: bool) -> Result<()> {
        super::vm::workload_remove(name, force)
    }
    fn describe(&self, name: &str) -> Result<()> {
        super::vm::workload_describe(name)
    }
}

/// The compute backends, in table/priority order (container before vm).
fn drivers() -> Vec<Box<dyn ComputeDriver>> {
    vec![Box::new(ContainerDriver), Box::new(VmDriver)]
}

/// Finds the SINGLE backend that owns `name`. Fail-closed: zero owners →
/// "no such workload"; two owners (a container AND a vm share the name) →
/// ambiguous, point at the type-specific command instead of guessing. Pure over
/// the driver slice, so the routing is unit-testable with fake drivers.
fn owner<'a>(ds: &'a [Box<dyn ComputeDriver>], name: &str) -> Result<&'a dyn ComputeDriver> {
    let mut found: Option<&dyn ComputeDriver> = None;
    for d in ds {
        if d.owns(name)? {
            if found.is_some() {
                return Err(Error::Invalid(super::po::tf(
                    "workload '{name}' is ambiguous (both a container and a vm) — use `delonix container` or `delonix vm` directly",
                    &[("name", name)],
                )));
            }
            found = Some(d.as_ref());
        }
    }
    found
        .ok_or_else(|| Error::Invalid(super::po::tf("no such workload: {name}", &[("name", name)])))
}

/// `delonix workload` — one surface over both compute types (ADR-0002, Phase 2a).
/// Creation stays declarative (`kind: Workload` via `stack apply`, ADR-0001);
/// this group is the imperative day-2 side (list/stop/rm).
#[derive(Debug, Subcommand)]
pub enum WorkloadCmd {
    /// List all workloads — containers AND VMs — in one table.
    Ls,
    /// Describe a workload by name (routed to the owning backend, kubectl-style).
    Describe {
        /// Workload name.
        name: String,
    },
    /// Stop a workload by name (routed to the owning backend).
    Stop {
        /// Workload name.
        name: String,
    },
    /// Remove a workload by name.
    Rm {
        /// Workload name.
        name: String,
        /// Force removal even if running / if backend cleanup refuses.
        #[arg(short, long)]
        force: bool,
    },
}

pub fn run(action: WorkloadCmd) -> Result<()> {
    match action {
        WorkloadCmd::Ls => ls(),
        WorkloadCmd::Describe { name } => owner(&drivers(), &name)?.describe(&name),
        // The adapters delegate to each engine's own stop/rm, which already emit
        // the success line (container → id, vm → name) — mirroring what the
        // native `container stop`/`vm stop` print. No extra println here, so the
        // output stays a single line.
        WorkloadCmd::Stop { name } => owner(&drivers(), &name)?.stop(&name),
        WorkloadCmd::Rm { name, force } => owner(&drivers(), &name)?.remove(&name, force),
    }
}

fn ls() -> Result<()> {
    let mut t = super::output::Table::new(&["TYPE", "NAME", "STATUS", "INFO"]);
    for d in drivers() {
        for r in d.list()? {
            t.row(vec![r.kind.to_string(), r.name, r.status, r.info]);
        }
    }
    t.print();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::manifest::{ManifestDoc, Metadata};

    fn wl(spec_yaml: &str) -> ManifestDoc {
        ManifestDoc {
            api_version: "delonix.io/v1".into(),
            kind: "Workload".into(),
            metadata: Metadata {
                name: "app".into(),
                namespace: Some("prod".into()),
                labels: Default::default(),
                annotations: Default::default(),
            },
            spec: serde_yaml::from_str(spec_yaml).unwrap(),
        }
    }

    #[test]
    fn lowers_container_and_inherits_metadata() {
        let child = lower_workload(&wl(
            "type: container\ncontainer: { image: nginx:alpine, ports: [\"8080:80\"] }",
        ))
        .unwrap();
        assert_eq!(child.kind, "Container");
        assert_eq!(child.metadata.name, "app");
        assert_eq!(child.metadata.namespace.as_deref(), Some("prod"));
        // The block is passed through verbatim as the child's spec.
        assert_eq!(
            child.spec.get("image").unwrap().as_str(),
            Some("nginx:alpine")
        );
    }

    #[test]
    fn lowers_vm() {
        let child = lower_workload(&wl("type: vm\nvm: { disk: golden.qcow2, vcpus: 2 }")).unwrap();
        assert_eq!(child.kind, "Vm");
        assert_eq!(child.spec.get("vcpus").unwrap().as_u64(), Some(2));
    }

    #[test]
    fn type_is_case_insensitive_and_trimmed() {
        assert_eq!(
            lower_workload(&wl("type: '  Container  '\ncontainer: { image: x }"))
                .unwrap()
                .kind,
            "Container"
        );
        assert_eq!(
            lower_workload(&wl("type: VM\nvm: { disk: x }"))
                .unwrap()
                .kind,
            "Vm"
        );
    }

    #[test]
    fn missing_type_is_an_error() {
        let e = lower_workload(&wl("container: { image: x }"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("spec.type is required"), "{e}");
    }

    #[test]
    fn missing_block_is_an_error() {
        let e = lower_workload(&wl("type: container"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("requires a 'container:' block"), "{e}");
    }

    #[test]
    fn block_must_match_type() {
        // type: container but a vm: block present — fail closed, never silently ignore.
        let e = lower_workload(&wl(
            "type: container\ncontainer: { image: x }\nvm: { disk: y }",
        ))
        .unwrap_err()
        .to_string();
        assert!(e.contains("must not carry a 'vm:' block"), "{e}");
    }

    #[test]
    fn reserved_types_fail_closed_with_targeted_hint() {
        let pod = lower_workload(&wl("type: pod\ncontainer: { image: x }"))
            .unwrap_err()
            .to_string();
        assert!(pod.contains("use kind: Pod"), "{pod}");
        let mv = lower_workload(&wl("type: microvm\nvm: { disk: x }"))
            .unwrap_err()
            .to_string();
        assert!(mv.contains("use type: vm"), "{mv}");
    }

    #[test]
    fn unknown_type_is_rejected() {
        let e = lower_workload(&wl("type: wasm\ncontainer: { image: x }"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown type 'wasm'"), "{e}");
    }

    // ---- routing (ADR-0002 Phase 2a): owner() fail-closed logic ----

    struct FakeDriver {
        owns: bool,
    }
    impl ComputeDriver for FakeDriver {
        fn list(&self) -> Result<Vec<WorkloadRow>> {
            Ok(vec![])
        }
        fn owns(&self, _: &str) -> Result<bool> {
            Ok(self.owns)
        }
        fn stop(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn remove(&self, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        fn describe(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    fn ds(a: bool, b: bool) -> Vec<Box<dyn ComputeDriver>> {
        vec![
            Box::new(FakeDriver { owns: a }),
            Box::new(FakeDriver { owns: b }),
        ]
    }

    #[test]
    fn owner_resolves_the_single_owner() {
        assert!(owner(&ds(true, false), "x").is_ok());
        assert!(owner(&ds(false, true), "x").is_ok());
    }

    #[test]
    fn owner_none_is_no_such_workload() {
        // `&dyn ComputeDriver` (the Ok type) is not Debug, so `.err().unwrap()`
        // rather than `.unwrap_err()`.
        let e = owner(&ds(false, false), "ghost").err().unwrap().to_string();
        assert!(e.contains("no such workload"), "{e}");
    }

    #[test]
    fn owner_two_owners_is_ambiguous() {
        // A container AND a vm with the same name — never guess; fail closed.
        let e = owner(&ds(true, true), "clash").err().unwrap().to_string();
        assert!(e.contains("ambiguous"), "{e}");
    }
}
