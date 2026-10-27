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
use crate::proto::v1::{DeleteImageRequest, Operation, PullImageRequest};

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
    let err_path = std::env::temp_dir().join(format!(
        "delonix-node-pull-{}-{}.err",
        std::process::id(),
        delonix_node::generate_id()
    ));
    let err_file = std::fs::File::create(&err_path)
        .map_err(|e| delonix_model::Error::Registry(format!("preparing the pull: {e}")))?;
    let mut args = vec!["pull".to_string(), req.reference.clone()];
    if !req.platform.is_empty() {
        args.push("--platform".to_string());
        args.push(req.platform.clone());
    }
    let status = Command::new(delonix_node::dispatch::cli_bin())
        .env("DELONIX_ROOT", root)
        // Machine-to-machine delegation past the grouped-commands barrier, the
        // same top-level form the CRI uses.
        .env("DELONIX_INTERNAL", "1")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file))
        .status();
    let outcome = match status {
        Ok(s) if s.success() => {
            delonix_node::events::emit(root, "image", "pull", &req.reference, &req.reference, None);
            Ok(())
        }
        Ok(s) => {
            let why = std::fs::read_to_string(&err_path).unwrap_or_default();
            let why = why.trim();
            Err(delonix_model::Error::Registry(if why.is_empty() {
                format!("`delonix pull {}` exited {s}", req.reference)
            } else {
                why.to_string()
            }))
        }
        Err(e) => Err(delonix_model::Error::Registry(format!(
            "could not run `delonix pull {}`: {e}",
            req.reference
        ))),
    };
    let _ = std::fs::remove_file(&err_path);
    outcome
}
