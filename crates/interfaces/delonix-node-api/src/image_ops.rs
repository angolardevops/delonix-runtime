//! `ImageService`, the write half: `PullImage` and `DeleteImage`.
//!
//! Each answers with an [`Operation`] persisted before the work starts (the
//! same ledger the network/volume/VM mutations use); what can be decided
//! without touching anything is refused BEFORE an operation exists.
//!
//! **Delete** removes the image from the OCI store in-process
//! (`ImageStore::remove`, the same `delonix rmi` makes) — a delete that reports
//! success has removed the reference (an untag when other tags remain, the
//! blobs when the last tag goes).
//!
//! **Pull** re-execs `delonix pull <reference>` in a fresh process: a pull is
//! registry I/O and the CLI already composes the credential resolution and the
//! store write — the same fork-safe re-exec seam `StartContainer` uses, not an
//! in-process fork. It blocks until the pull finishes, so the Operation is
//! returned already `Succeeded`/`Failed`. (Streaming the pull's progress is a
//! later addition; the Operation is terminal here.)

use std::path::Path;
use std::process::{Command, Stdio};

use delonix_oci::ImageStore;
use tonic::Status;

use crate::operations::{self, Begun};
use crate::proto::v1::{
    BuildImageRequest, DeleteImageRequest, Operation, PullImageRequest, PushImageRequest,
    ScanImageRequest,
};

/// Images are node-wide; the operation target is the reference.
fn target_of(reference: &str) -> String {
    format!("Image/{reference}")
}

/// Where `GetImage` would read the result back.
fn image_href(reference: &str) -> String {
    format!("/v1/images:get?reference={reference}")
}

/// `DeleteImage` — removes a reference from the OCI store (untag or, for the
/// last tag, the image). In-process; no re-exec for a local store write.
pub fn delete_in(root: &Path, req: &DeleteImageRequest) -> Result<Operation, Status> {
    if req.reference.is_empty() {
        return Err(Status::invalid_argument(
            "DeleteImage: reference is required",
        ));
    }
    let target = target_of(&req.reference);
    let fp = operations::fingerprint_of(&[("reference", &req.reference)]);
    if let Some(found) = operations::replay(root, "delete", &target, "", &fp)? {
        return Ok(operations::message(&found));
    }
    let rec = match operations::begin(root, "delete", &target, "", &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = delete_work(root, &req.reference).map(|()| image_href(&req.reference));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

fn delete_work(root: &Path, reference: &str) -> delonix_model::Result<()> {
    let store = ImageStore::open(root).map_err(delonix_model::Error::from)?;
    store
        .remove(reference)
        .map_err(delonix_model::Error::from)?;
    delonix_node::events::emit(root, "image", "remove", reference, reference, None);
    Ok(())
}

/// `PullImage` — pulls the reference from its registry (re-exec of `delonix
/// pull`). `platform` ("linux/arm64") is forwarded; empty = the host's.
pub fn pull_in(root: &Path, req: &PullImageRequest) -> Result<Operation, Status> {
    if req.reference.is_empty() {
        return Err(Status::invalid_argument("PullImage: reference is required"));
    }
    let target = target_of(&req.reference);
    let fp =
        operations::fingerprint_of(&[("reference", &req.reference), ("platform", &req.platform)]);
    if let Some(found) = operations::replay(root, "pull", &target, &req.request_id, &fp)? {
        return Ok(operations::message(&found));
    }
    let rec = match operations::begin(root, "pull", &target, &req.request_id, &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = pull_work(root, req).map(|()| image_href(&req.reference));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

fn pull_work(root: &Path, req: &PullImageRequest) -> delonix_model::Result<()> {
    let mut args = vec!["pull".to_string(), req.reference.clone()];
    if !req.platform.is_empty() {
        args.push("--platform".to_string());
        args.push(req.platform.clone());
    }
    reexec(root, &args, &format!("pull {}", req.reference))?;
    delonix_node::events::emit(root, "image", "pull", &req.reference, &req.reference, None);
    Ok(())
}

/// `PushImage` — publishes a local image to a registry (`delonix push <ref>
/// [destination]`). `destination` empty = the image's own reference.
pub fn push_in(root: &Path, req: &PushImageRequest) -> Result<Operation, Status> {
    if req.reference.is_empty() {
        return Err(Status::invalid_argument("PushImage: reference is required"));
    }
    let target = target_of(&req.reference);
    let fp = operations::fingerprint_of(&[
        ("reference", &req.reference),
        ("destination", &req.destination),
    ]);
    if let Some(found) = operations::replay(root, "push", &target, &req.request_id, &fp)? {
        return Ok(operations::message(&found));
    }
    let rec = match operations::begin(root, "push", &target, &req.request_id, &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = push_work(root, req).map(|()| image_href(&req.reference));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

fn push_work(root: &Path, req: &PushImageRequest) -> delonix_model::Result<()> {
    let mut args = vec!["push".to_string(), req.reference.clone()];
    if !req.destination.is_empty() {
        args.push(req.destination.clone());
    }
    reexec(root, &args, &format!("push {}", req.reference))?;
    delonix_node::events::emit(root, "image", "push", &req.reference, &req.reference, None);
    Ok(())
}

/// `BuildImage` — builds an image from a context on this node (`delonix build
/// -t <tag>… [-f file] [--target …] [--platform …] [--build-arg k=v]…
/// [--no-cache] [--secret …] <context>`). At least one tag is required.
pub fn build_in(root: &Path, req: &BuildImageRequest) -> Result<Operation, Status> {
    if req.tags.is_empty() {
        return Err(Status::invalid_argument(
            "BuildImage: at least one tag is required",
        ));
    }
    if req.context_path.is_empty() {
        return Err(Status::invalid_argument(
            "BuildImage: context_path is required",
        ));
    }
    let target = format!("Image/{}", req.tags[0]);
    let args_fp = req.tags.join(",");
    let fp = operations::fingerprint_of(&[
        ("tags", &args_fp),
        ("context_path", &req.context_path),
        ("file", &req.file),
        ("target_stage", &req.target_stage),
        ("platform", &req.platform),
        ("no_cache", if req.no_cache { "1" } else { "" }),
        ("build_args", &operations::canonical_map(&req.build_args)),
    ]);
    if let Some(found) = operations::replay(root, "build", &target, &req.request_id, &fp)? {
        return Ok(operations::message(&found));
    }
    let rec = match operations::begin(root, "build", &target, &req.request_id, &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = build_work(root, req).map(|()| image_href(&req.tags[0]));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

fn build_work(root: &Path, req: &BuildImageRequest) -> delonix_model::Result<()> {
    let mut args = vec!["build".to_string()];
    for tag in &req.tags {
        args.push("-t".to_string());
        args.push(tag.clone());
    }
    if !req.file.is_empty() {
        args.push("-f".to_string());
        args.push(req.file.clone());
    }
    if !req.target_stage.is_empty() {
        args.push("--target".to_string());
        args.push(req.target_stage.clone());
    }
    if !req.platform.is_empty() {
        args.push("--platform".to_string());
        args.push(req.platform.clone());
    }
    // Build args in a stable order (the fingerprint canonicalises the same way).
    let mut kv: Vec<(&String, &String)> = req.build_args.iter().collect();
    kv.sort();
    for (k, v) in kv {
        args.push("--build-arg".to_string());
        args.push(format!("{k}={v}"));
    }
    if req.no_cache {
        args.push("--no-cache".to_string());
    }
    args.push(req.context_path.clone());
    reexec(root, &args, &format!("build {}", req.tags[0]))?;
    delonix_node::events::emit(root, "image", "build", &req.tags[0], &req.tags[0], None);
    Ok(())
}

/// `ScanImage` — SBOM + CVE scan of an image (`delonix image scan <ref>`). The
/// scan runs to completion; its findings are not carried in the Operation
/// (reported by `delonix image scan`'s own output) — the Operation is the
/// terminal did-it-run signal.
pub fn scan_in(root: &Path, req: &ScanImageRequest) -> Result<Operation, Status> {
    if req.reference.is_empty() {
        return Err(Status::invalid_argument("ScanImage: reference is required"));
    }
    let target = target_of(&req.reference);
    let fp = operations::fingerprint_of(&[("reference", &req.reference)]);
    if let Some(found) = operations::replay(root, "scan", &target, "", &fp)? {
        return Ok(operations::message(&found));
    }
    let rec = match operations::begin(root, "scan", &target, "", &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = scan_work(root, &req.reference).map(|()| image_href(&req.reference));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

fn scan_work(root: &Path, reference: &str) -> delonix_model::Result<()> {
    let args = vec![
        "image".to_string(),
        "scan".to_string(),
        reference.to_string(),
    ];
    reexec(root, &args, &format!("image scan {reference}"))?;
    delonix_node::events::emit(root, "image", "scan", reference, reference, None);
    Ok(())
}

/// Re-exec `delonix <args>` in a fresh process (the fork-safe seam), capturing
/// stderr for the failure message. `DELONIX_ROOT` keeps the store consistent;
/// `DELONIX_INTERNAL` crosses the grouped-commands barrier (as the CRI does).
fn reexec(root: &Path, args: &[String], what: &str) -> delonix_model::Result<()> {
    let err_path = std::env::temp_dir().join(format!(
        "delonix-node-image-{}-{}.err",
        std::process::id(),
        delonix_node::generate_id()
    ));
    let err_file = std::fs::File::create(&err_path)
        .map_err(|e| delonix_model::Error::Registry(format!("preparing `{what}`: {e}")))?;
    let status = Command::new(delonix_node::dispatch::cli_bin())
        .env("DELONIX_ROOT", root)
        .env("DELONIX_INTERNAL", "1")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file))
        .status();
    let outcome = match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            let why = std::fs::read_to_string(&err_path).unwrap_or_default();
            let why = why.trim();
            Err(delonix_model::Error::Registry(if why.is_empty() {
                format!("`delonix {what}` exited {s}")
            } else {
                why.to_string()
            }))
        }
        Err(e) => Err(delonix_model::Error::Registry(format!(
            "could not run `delonix {what}`: {e}"
        ))),
    };
    let _ = std::fs::remove_file(&err_path);
    outcome
}
