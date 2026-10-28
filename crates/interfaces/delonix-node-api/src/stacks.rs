//! `StackService`: `PlanStack`, `ApplyStack`, `DestroyStack`.
//!
//! All three take a multi-document manifest (bytes, YAML or JSON) and run the
//! `delonix stack` verb that composes them in a fresh process (re-exec; the CLI
//! orders the kinds Network→Volume→Image→Vm→Container and owns them by the
//! `delonix.io/stack` label). The manifest is written to a temp file for the
//! `-f` flag and removed after.
//!
//! `PlanStack` parses the CLI's `-o json` into a `StackPlan` (it changes
//! nothing). `ApplyStack`/`DestroyStack` return a terminal `Operation` through
//! the operation ledger (replay/begin/finish), the same as the other mutations.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use tonic::Status;

use crate::operations::{self, Begun};
use crate::proto::v1::{
    ApplyStackRequest, ChangeAction, DestroyStackRequest, Operation, PlanStackRequest,
    PlannedChange, StackPlan,
};

/// Writes the manifest to a temp file (`-f` takes a path). The caller removes it.
fn write_manifest(manifest: &[u8]) -> Result<PathBuf, Status> {
    let path = std::env::temp_dir().join(format!(
        "delonix-node-stack-{}-{}.yaml",
        std::process::id(),
        delonix_node::generate_id()
    ));
    let mut f = std::fs::File::create(&path)
        .map_err(|e| Status::internal(format!("preparing the manifest: {e}")))?;
    f.write_all(manifest)
        .map_err(|e| Status::internal(format!("writing the manifest: {e}")))?;
    Ok(path)
}

/// Runs `delonix <args>` to completion, capturing stdout and stderr. The store
/// root and the grouped-commands barrier are set the same way the other
/// re-execs do.
fn run(root: &Path, args: &[String]) -> std::io::Result<std::process::Output> {
    Command::new(delonix_node::dispatch::cli_bin())
        .env("DELONIX_ROOT", root)
        .env("DELONIX_INTERNAL", "1")
        .env("NO_COLOR", "1")
        .args(args)
        .output()
}

fn action_of(s: &str) -> ChangeAction {
    match s {
        "create" => ChangeAction::Create,
        "update" => ChangeAction::Update,
        "replace" => ChangeAction::Replace,
        "delete" => ChangeAction::Delete,
        "adopt" => ChangeAction::Adopt,
        "conflict" => ChangeAction::Conflict,
        "noop" | "no-op" | "none" | "unchanged" => ChangeAction::NoOp,
        _ => ChangeAction::Unspecified,
    }
}

/// One row of `stack plan -o json` (`{kind,name,action,diffs,changed}`) → a
/// `PlannedChange` (`target = "Kind/name"`, the fields are the diffs).
fn to_change(v: &serde_json::Value) -> PlannedChange {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default();
    let fields = v
        .get("diffs")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .map(|d| {
                    d.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| d.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    PlannedChange {
        target: format!("{}/{}", s("kind"), s("name")),
        action: action_of(s("action")) as i32,
        fields,
        reason: String::new(),
    }
}

/// `PlanStack` — what an apply WOULD change, touching nothing.
pub fn plan_in(root: &Path, req: &PlanStackRequest) -> Result<StackPlan, Status> {
    if req.manifest.is_empty() {
        return Err(Status::invalid_argument("PlanStack: manifest is required"));
    }
    let manifest = write_manifest(&req.manifest)?;
    let mut args = vec![
        "stack".to_string(),
        "plan".to_string(),
        "-f".to_string(),
        manifest.display().to_string(),
        "-o".to_string(),
        "json".to_string(),
    ];
    if req.prune {
        args.push("--prune".to_string());
    }
    let out = run(root, &args);
    let _ = std::fs::remove_file(&manifest);
    let out =
        out.map_err(|e| Status::internal(format!("could not run `delonix stack plan`: {e}")))?;
    // `stack plan -o json` prints the plan array on stdout; `--detailed-exitcode`
    // makes it exit non-zero WHEN THERE ARE CHANGES, so the exit code is not a
    // failure — the JSON is. Parse from the first `[`.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let start = stdout.find('[').ok_or_else(|| {
        let why = String::from_utf8_lossy(&out.stderr);
        Status::internal(format!("PlanStack: {}", why.trim()))
    })?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&stdout[start..])
        .map_err(|e| Status::internal(format!("PlanStack: parsing the plan: {e}")))?;
    let changes: Vec<PlannedChange> = items.iter().map(to_change).collect();
    let has_changes = changes.iter().any(|c| {
        c.action != ChangeAction::NoOp as i32 && c.action != ChangeAction::Unspecified as i32
    });
    Ok(StackPlan {
        changes,
        has_changes,
    })
}

/// Fingerprint of a mutation's content (the manifest + its flags).
fn mutation_fp(manifest: &[u8], extra: &[(&str, &str)]) -> String {
    let manifest = String::from_utf8_lossy(manifest);
    let mut parts: Vec<(&str, &str)> = vec![("manifest", &manifest)];
    parts.extend_from_slice(extra);
    operations::fingerprint_of(&parts)
}

fn stack_target(stack: &str) -> String {
    format!(
        "Stack/{}",
        if stack.is_empty() {
            "(manifest)"
        } else {
            stack
        }
    )
}

fn stack_href(stack: &str) -> String {
    format!("/v1/stacks/{}", crate::page::escape(stack))
}

/// `ApplyStack` — materialises the manifest (re-exec of `delonix stack apply`).
pub fn apply_in(root: &Path, req: &ApplyStackRequest) -> Result<Operation, Status> {
    if req.manifest.is_empty() {
        return Err(Status::invalid_argument("ApplyStack: manifest is required"));
    }
    let target = stack_target(&req.stack);
    let replace = req.replace.join(",");
    let fp = mutation_fp(
        &req.manifest,
        &[
            ("prune", if req.prune { "1" } else { "" }),
            ("replace", &replace),
        ],
    );
    if let Some(found) = operations::replay(root, "apply", &target, &req.request_id, &fp)? {
        return Ok(operations::message(&found));
    }
    let rec = match operations::begin(root, "apply", &target, &req.request_id, &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = apply_work(root, req).map(|()| stack_href(&req.stack));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

fn apply_work(root: &Path, req: &ApplyStackRequest) -> delonix_model::Result<()> {
    let manifest = write_manifest(&req.manifest)
        .map_err(|e| delonix_model::Error::Invalid(format!("{}", e.message())))?;
    let mut args = vec![
        "stack".to_string(),
        "apply".to_string(),
        "-f".to_string(),
        manifest.display().to_string(),
    ];
    if req.prune {
        args.push("--prune".to_string());
    }
    for r in &req.replace {
        args.push("--replace".to_string());
        args.push(r.clone());
    }
    let outcome = finish_status(root, &args, "stack apply");
    let _ = std::fs::remove_file(&manifest);
    outcome?;
    delonix_node::events::emit(root, "stack", "apply", &req.stack, &req.stack, None);
    Ok(())
}

/// `DestroyStack` — removes everything the stack owns (re-exec of `destroy`).
pub fn destroy_in(root: &Path, req: &DestroyStackRequest) -> Result<Operation, Status> {
    if req.manifest.is_empty() {
        return Err(Status::invalid_argument(
            "DestroyStack: manifest is required",
        ));
    }
    let target = stack_target(&req.stack);
    let fp = mutation_fp(&req.manifest, &[("verb", "destroy")]);
    if let Some(found) = operations::replay(root, "destroy", &target, &req.request_id, &fp)? {
        return Ok(operations::message(&found));
    }
    let rec = match operations::begin(root, "destroy", &target, &req.request_id, &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = destroy_work(root, req).map(|()| stack_href(&req.stack));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

fn destroy_work(root: &Path, req: &DestroyStackRequest) -> delonix_model::Result<()> {
    let manifest = write_manifest(&req.manifest)
        .map_err(|e| delonix_model::Error::Invalid(format!("{}", e.message())))?;
    let args = vec![
        "stack".to_string(),
        "destroy".to_string(),
        "-f".to_string(),
        manifest.display().to_string(),
    ];
    let outcome = finish_status(root, &args, "stack destroy");
    let _ = std::fs::remove_file(&manifest);
    outcome?;
    delonix_node::events::emit(root, "stack", "destroy", &req.stack, &req.stack, None);
    Ok(())
}

/// Runs a mutation and turns a non-zero exit into a `delonix_model::Error` with
/// the captured stderr (the apply/destroy failure the caller reads).
fn finish_status(root: &Path, args: &[String], what: &str) -> delonix_model::Result<()> {
    match run(root, args) {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let why = String::from_utf8_lossy(&o.stderr);
            let why = why.trim();
            Err(delonix_model::Error::Invalid(if why.is_empty() {
                format!("`delonix {what}` exited {}", o.status)
            } else {
                why.to_string()
            }))
        }
        Err(e) => Err(delonix_model::Error::Invalid(format!(
            "could not run `delonix {what}`: {e}"
        ))),
    }
}
