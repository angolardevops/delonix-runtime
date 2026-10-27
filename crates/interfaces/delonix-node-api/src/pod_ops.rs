//! `PodService`, the write half: `CreatePod` and `DeletePod`.
//!
//! A pod has no store of its own — it is the labelled members a `kind: Pod`
//! manifest builds (shared netns + IPC/UTS). So both verbs re-exec the CLI that
//! owns that construction in a fresh, fork-safe process (the same seam
//! `StartContainer`/`Exec`/`PullImage` use, never an in-process fork):
//!
//! - **CreatePod** renders the request's [`PodSpec`] back to a `kind: Pod`
//!   manifest and runs `delonix pod create -f <manifest>`. What the manifest
//!   form cannot carry faithfully is refused BEFORE the operation exists
//!   (`UNIMPLEMENTED`), never dropped in silence — the full-fidelity path is
//!   `ApplyStack` / `delonix pod create -f` with a hand-written document.
//! - **DeletePod** runs `delonix delete pod <name> [--force]`, removing every
//!   member and the shared netns.
//!
//! Both answer with a terminal [`Operation`] through the ledger
//! (replay/begin/finish, idempotent on `request_id`), like the other mutations.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tonic::Status;

use crate::operations::{self, Begun};
use crate::proto::v1::{
    ContainerSpec, CreatePodRequest, DeletePodRequest, Operation, PodSpec, Protocol,
};

/// The node.v1 pod name is the internal id `pod-<name>`; the CLI verbs take the
/// bare `<name>` (its `metadata.name`). Accept either form.
fn bare_name(name: &str) -> &str {
    name.strip_prefix("pod-").unwrap_or(name)
}

fn target_of(name: &str) -> String {
    format!("Pod/{}", bare_name(name))
}

fn pod_href(namespace: &str, name: &str) -> String {
    let ns = if namespace.is_empty() {
        "default"
    } else {
        namespace
    };
    // The read side exposes the pod under its `pod-<name>` id.
    format!("/v1/namespaces/{ns}/pods/pod-{}", bare_name(name))
}

/// Refuses a `ContainerSpec` field the `kind: Pod` manifest form here cannot
/// carry, so nothing is accepted and then silently dropped. The rich shape
/// belongs to `ApplyStack` / a hand-written `kind: Pod` document.
fn reject_unsupported(member: &str, spec: &ContainerSpec) -> Result<(), Status> {
    let unimpl = |field: &str| {
        Err(Status::unimplemented(format!(
            "CreatePod: container '{member}' sets `{field}`, which the node API does not \
             serve on this path yet — use ApplyStack or `delonix pod create -f` with a \
             `kind: Pod` document for the full container shape"
        )))
    };
    if !spec.secrets.is_empty() {
        return unimpl("secrets");
    }
    if !spec.user.is_empty() {
        return unimpl("user");
    }
    if !spec.hostname.is_empty() {
        return unimpl("hostname");
    }
    if spec.resources.is_some() {
        return unimpl("resources");
    }
    if !spec.mounts.is_empty() {
        return unimpl("mounts");
    }
    if !spec.networks.is_empty() {
        return unimpl("networks");
    }
    if spec.security.is_some() {
        return unimpl("security");
    }
    if spec.restart.is_some() {
        return unimpl("restart");
    }
    if spec.health_check.is_some() {
        return unimpl("health_check");
    }
    if !spec.devices.is_empty() {
        return unimpl("devices");
    }
    if !spec.tmpfs.is_empty() {
        return unimpl("tmpfs");
    }
    if spec.read_only_rootfs {
        return unimpl("read_only_rootfs");
    }
    if !spec.sysctls.is_empty() {
        return unimpl("sysctls");
    }
    if !spec.log_format.is_empty() {
        return unimpl("log_format");
    }
    if !spec.pod.is_empty() {
        // The pod a member joins comes from this request, not a self-reference.
        return unimpl("pod");
    }
    Ok(())
}

/// The ports of a member as the manifest's `PodPort` list (`containerPort`
/// required; `hostPort`/`hostIP`/`protocol` when set).
fn ports_json(spec: &ContainerSpec) -> Vec<serde_json::Value> {
    spec.ports
        .iter()
        .map(|p| {
            let mut m = serde_json::Map::new();
            m.insert("containerPort".into(), p.container_port.into());
            if p.host_port != 0 {
                m.insert("hostPort".into(), p.host_port.into());
            }
            if !p.host_ip.is_empty() {
                m.insert("hostIP".into(), p.host_ip.clone().into());
            }
            match Protocol::try_from(p.protocol) {
                Ok(Protocol::Udp) => {
                    m.insert("protocol".into(), "udp".into());
                }
                Ok(Protocol::Tcp) => {
                    m.insert("protocol".into(), "tcp".into());
                }
                _ => {}
            }
            serde_json::Value::Object(m)
        })
        .collect()
}

/// `env` is a map on the wire and a `[{name,value}]` list in the manifest, in a
/// stable (sorted) order so the same request renders the same document.
fn env_json(spec: &ContainerSpec) -> Vec<serde_json::Value> {
    let mut kv: Vec<(&String, &String)> = spec.env.iter().collect();
    kv.sort();
    kv.into_iter()
        .map(|(k, v)| serde_json::json!({ "name": k, "value": v }))
        .collect()
}

/// Renders one member to its `PodContainer` manifest object. `name` is the
/// member's name; `entrypoint`→`command` and `command`→`args` (k8s spelling).
fn member_json(name: &str, spec: &ContainerSpec) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if !name.is_empty() {
        m.insert("name".into(), name.into());
    }
    m.insert("image".into(), spec.image.clone().into());
    if !spec.entrypoint.is_empty() {
        m.insert("command".into(), spec.entrypoint.clone().into());
    }
    if !spec.command.is_empty() {
        m.insert("args".into(), spec.command.clone().into());
    }
    if !spec.workdir.is_empty() {
        m.insert("workingDir".into(), spec.workdir.clone().into());
    }
    let env = env_json(spec);
    if !env.is_empty() {
        m.insert("env".into(), env.into());
    }
    let ports = ports_json(spec);
    if !ports.is_empty() {
        m.insert("ports".into(), ports.into());
    }
    serde_json::Value::Object(m)
}

/// Renders the `CreatePod` request to a `kind: Pod` manifest (YAML). Refuses
/// the pod-level and member fields this path does not carry.
fn render_manifest(req: &CreatePodRequest) -> Result<String, Status> {
    let name = bare_name(&req.name);
    if name.is_empty() {
        return Err(Status::invalid_argument("CreatePod: name is required"));
    }
    let spec: &PodSpec = req
        .spec
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("CreatePod: spec is required"))?;
    if spec.containers.is_empty() {
        return Err(Status::invalid_argument(
            "CreatePod: spec.containers must have at least one container",
        ));
    }
    if !spec.networks.is_empty() {
        return Err(Status::unimplemented(
            "CreatePod: spec.networks (pod-level) is in the contract and not served on this \
             path yet — use ApplyStack",
        ));
    }
    if !spec.ports.is_empty() {
        return Err(Status::unimplemented(
            "CreatePod: spec.ports (pod-level) is in the contract and not served on this path \
             yet — use ApplyStack",
        ));
    }
    let mut members = Vec::with_capacity(spec.containers.len());
    for nc in &spec.containers {
        let cs = nc
            .spec
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("CreatePod: a container has no spec"))?;
        if cs.image.is_empty() {
            return Err(Status::invalid_argument(
                "CreatePod: every container needs an image",
            ));
        }
        reject_unsupported(&nc.name, cs)?;
        members.push(member_json(&nc.name, cs));
    }

    let mut metadata = serde_json::Map::new();
    metadata.insert("name".into(), name.into());
    if !req.namespace.is_empty() && req.namespace != "default" {
        metadata.insert("namespace".into(), req.namespace.clone().into());
    }
    if !req.labels.is_empty() {
        metadata.insert(
            "labels".into(),
            serde_json::to_value(&req.labels).unwrap_or_default(),
        );
    }
    if !req.annotations.is_empty() {
        metadata.insert(
            "annotations".into(),
            serde_json::to_value(&req.annotations).unwrap_or_default(),
        );
    }

    let mut pod_spec = serde_json::Map::new();
    pod_spec.insert("containers".into(), members.into());
    if spec.share_process_namespace {
        pod_spec.insert("shareProcessNamespace".into(), true.into());
    }

    let doc = serde_json::json!({
        "apiVersion": "delonix.io/v1",
        "kind": "Pod",
        "metadata": serde_json::Value::Object(metadata),
        "spec": serde_json::Value::Object(pod_spec),
    });
    serde_yaml::to_string(&doc)
        .map_err(|e| Status::internal(format!("CreatePod: rendering the manifest: {e}")))
}

/// Writes the manifest to a temp file (`-f` takes a path). The caller removes it.
fn write_manifest(manifest: &str) -> Result<PathBuf, Status> {
    let path = std::env::temp_dir().join(format!(
        "delonix-node-pod-{}-{}.yaml",
        std::process::id(),
        delonix_node::generate_id()
    ));
    let mut f = std::fs::File::create(&path)
        .map_err(|e| Status::internal(format!("preparing the manifest: {e}")))?;
    f.write_all(manifest.as_bytes())
        .map_err(|e| Status::internal(format!("writing the manifest: {e}")))?;
    Ok(path)
}

/// `CreatePod` — materialises a `kind: Pod` manifest (re-exec of `delonix pod
/// create`). A terminal [`Operation`] through the ledger.
pub fn create_in(root: &Path, req: &CreatePodRequest) -> Result<Operation, Status> {
    // Everything the request itself gets wrong is refused before an operation
    // exists — the manifest render is where the validation lives.
    let manifest = render_manifest(req)?;
    let name = bare_name(&req.name).to_string();
    let target = target_of(&name);
    let fp = operations::fingerprint_of(&[("manifest", &manifest)]);
    if let Some(found) = operations::replay(root, "create", &target, &req.request_id, &fp)? {
        return Ok(operations::message(&found));
    }
    let rec = match operations::begin(root, "create", &target, &req.request_id, &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = create_work(root, &manifest, &name).map(|()| pod_href(&req.namespace, &name));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

fn create_work(root: &Path, manifest: &str, name: &str) -> delonix_model::Result<()> {
    let path = write_manifest(manifest)
        .map_err(|e| delonix_model::Error::Invalid(e.message().to_string()))?;
    let args = vec![
        "pod".to_string(),
        "create".to_string(),
        "-f".to_string(),
        path.display().to_string(),
    ];
    let outcome = reexec(root, &args, "pod create");
    let _ = std::fs::remove_file(&path);
    outcome?;
    delonix_node::events::emit(root, "pod", "create", name, name, None);
    Ok(())
}

/// `DeletePod` — removes every member and the shared netns (re-exec of `delonix
/// delete pod`). `force` stops a running pod first.
pub fn delete_in(root: &Path, req: &DeletePodRequest) -> Result<Operation, Status> {
    let name = bare_name(&req.name).to_string();
    if name.is_empty() {
        return Err(Status::invalid_argument("DeletePod: name is required"));
    }
    let target = target_of(&name);
    let fp = operations::fingerprint_of(&[("force", if req.force { "1" } else { "" })]);
    if let Some(found) = operations::replay(root, "delete", &target, &req.request_id, &fp)? {
        return Ok(operations::message(&found));
    }
    let rec = match operations::begin(root, "delete", &target, &req.request_id, &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = delete_work(root, &name, req.force).map(|()| String::new());
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

fn delete_work(root: &Path, name: &str, force: bool) -> delonix_model::Result<()> {
    let mut args = vec!["delete".to_string(), "pod".to_string(), name.to_string()];
    if force {
        args.push("--force".to_string());
    }
    reexec(root, &args, "delete pod")?;
    delonix_node::events::emit(root, "pod", "delete", name, name, None);
    Ok(())
}

/// Re-exec `delonix <args>` in a fresh process (the fork-safe seam), capturing
/// stderr for the failure message. Same contract as `image_ops::reexec`, but
/// its error class is `Invalid` (a pod mutation is not registry I/O).
fn reexec(root: &Path, args: &[String], what: &str) -> delonix_model::Result<()> {
    let err_path = std::env::temp_dir().join(format!(
        "delonix-node-pod-{}-{}.err",
        std::process::id(),
        delonix_node::generate_id()
    ));
    let err_file = std::fs::File::create(&err_path)
        .map_err(|e| delonix_model::Error::Invalid(format!("preparing `{what}`: {e}")))?;
    let status = Command::new(delonix_node::dispatch::cli_bin())
        .env("DELONIX_ROOT", root)
        .env("DELONIX_INTERNAL", "1")
        .env("NO_COLOR", "1")
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
            Err(delonix_model::Error::Invalid(if why.is_empty() {
                format!("`delonix {what}` exited {s}")
            } else {
                why.to_string()
            }))
        }
        Err(e) => Err(delonix_model::Error::Invalid(format!(
            "could not run `delonix {what}`: {e}"
        ))),
    };
    let _ = std::fs::remove_file(&err_path);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::v1::NamedContainer;

    fn member(name: &str, image: &str) -> NamedContainer {
        NamedContainer {
            name: name.into(),
            spec: Some(ContainerSpec {
                image: image.into(),
                ..Default::default()
            }),
        }
    }

    fn create_req(name: &str, members: Vec<NamedContainer>) -> CreatePodRequest {
        CreatePodRequest {
            namespace: String::new(),
            name: name.into(),
            labels: Default::default(),
            annotations: Default::default(),
            spec: Some(PodSpec {
                containers: members,
                ..Default::default()
            }),
            request_id: String::new(),
        }
    }

    #[test]
    fn renders_a_minimal_pod_manifest() {
        let mut m = member("main", "busybox:1.36");
        if let Some(s) = m.spec.as_mut() {
            s.entrypoint = vec!["sh".into(), "-c".into()];
            s.command = vec!["sleep 3600".into()];
            s.env = [("K".to_string(), "V".to_string())].into();
            s.workdir = "/app".into();
        }
        let doc = render_manifest(&create_req("web", vec![m])).unwrap();
        assert!(doc.contains("kind: Pod"));
        assert!(doc.contains("name: web"));
        assert!(doc.contains("image: busybox:1.36"));
        // entrypoint→command, command→args (k8s spelling).
        assert!(doc.contains("command:"));
        assert!(doc.contains("- sh"));
        assert!(doc.contains("args:"));
        assert!(doc.contains("workingDir: /app"));
        assert!(doc.contains("name: K"));
        assert!(doc.contains("value: V"));
    }

    #[test]
    fn the_pod_name_prefix_is_stripped_for_the_cli() {
        // The read side hands back `pod-web`; create/delete take the bare name.
        assert_eq!(bare_name("pod-web"), "web");
        assert_eq!(bare_name("web"), "web");
        let doc = render_manifest(&create_req("pod-web", vec![member("c", "img")])).unwrap();
        assert!(doc.contains("name: web"));
        assert!(!doc.contains("name: pod-web"));
    }

    #[test]
    fn an_unsupported_container_field_is_refused_not_dropped() {
        let mut m = member("main", "busybox");
        m.spec.as_mut().unwrap().user = "1000".into();
        let err = render_manifest(&create_req("web", vec![m])).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
        assert!(err.message().contains("user"));
    }

    #[test]
    fn empty_and_imageless_pods_are_refused() {
        let err = render_manifest(&create_req("web", vec![])).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        let err = render_manifest(&create_req("web", vec![member("c", "")])).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn ports_render_in_k8s_shape() {
        let mut m = member("main", "nginx");
        m.spec.as_mut().unwrap().ports = vec![crate::proto::v1::PortMapping {
            host_ip: String::new(),
            host_port: 8080,
            container_port: 80,
            protocol: Protocol::Tcp as i32,
        }];
        let doc = render_manifest(&create_req("web", vec![m])).unwrap();
        assert!(doc.contains("containerPort: 80"));
        assert!(doc.contains("hostPort: 8080"));
        assert!(doc.contains("protocol: tcp"));
    }
}
