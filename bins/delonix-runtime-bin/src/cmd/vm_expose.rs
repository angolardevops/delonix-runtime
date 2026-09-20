//! `VirtualMachine.spec.expose` — publish HTTP/S services listening INSIDE a VM
//! by name (ADR-0046, D1).
//!
//! **Sugar, and it does not survive [`super::manifest::load`].** A VM that declares
//! `expose:` keeps its own document (minus the `expose` key, so the VM reconciler
//! never reports it as a field it "did not compare") and gains ONE synthetic
//! `kind: HTTPRoute` named `<vm>-expose`. Everything downstream — `apply`, `plan`,
//! drift, `--prune`, the proxy — sees an ordinary route. There is no second proxy
//! path for VMs.
//!
//! Pure: no store, no IPs. The backend names the VM, and the address is resolved
//! when the route is applied (`httproute::vm_ips`), which is also why this can run at
//! load time, long before any VM exists.

use super::kinds as k;
use super::manifest::{ManifestDoc, Metadata};
use delonix_model::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

/// One published service of a VM.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct VmExposeSpec {
    /// Host name to match (`app.example.pt`).
    pub host: String,
    /// Port the service listens on INSIDE the guest.
    pub port: u16,
    /// Path prefix. Default `/`.
    #[serde(default = "default_path")]
    pub path: String,
    /// TLS termination, the same block as `kind: HTTPRoute` (`mode: selfSigned`
    /// or `mode: secretRef` + `secretRef`). The proxy has ONE certificate per
    /// route set, so every entry that sets it must set the same one.
    #[serde(default)]
    #[schemars(with = "Option<super::httproute::TlsSpec>")]
    pub tls: Option<Value>,
    /// Where to publish `host` so it resolves: `[host]` puts it in a delimited block
    /// of the operator host's `/etc/hosts` (root). The route publishes ONE list for
    /// all its names, so every entry must say the same.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// `kind: IPPool` the route takes its address from (ADR-0046 D3). One address per
    /// route, so every entry must name the same pool.
    #[serde(default)]
    pub pool: Option<String>,
}

fn default_path() -> String {
    "/".to_string()
}

/// Suffix of the synthetic route, so the child of `web01` is `web01-expose`.
pub(crate) const ROUTE_SUFFIX: &str = "-expose";

/// Lowers every `kind: VirtualMachine` that declares `expose:` into the VM (without the key)
/// plus one `kind: HTTPRoute`. Documents that declare nothing pass through untouched.
pub(crate) fn lower_vm_expose(docs: Vec<ManifestDoc>) -> Result<Vec<ManifestDoc>> {
    let mut out = Vec::with_capacity(docs.len());
    for mut doc in docs {
        if doc.kind != k::VM {
            out.push(doc);
            continue;
        }
        let exposes = take_expose(&mut doc)?;
        if exposes.is_empty() {
            out.push(doc);
            continue;
        }
        let route = route_for(&doc, &exposes)?;
        // `<vm>-expose` is a name the user may have taken for a route of their own:
        // two documents with one (kind, name) would have the later silently win.
        if out.iter().any(|d| {
            (d.kind == k::HTTP_ROUTE || d.kind == k::INGRESS)
                && d.metadata.name == route.metadata.name
        }) {
            return Err(Error::Invalid(super::po::tf(
                "VirtualMachine/{vm}: spec.expose generates the route '{route}', but a route with that name is already declared — rename one of them",
                &[("vm", &doc.metadata.name), ("route", &route.metadata.name)],
            )));
        }
        out.push(doc);
        out.push(route);
    }
    Ok(out)
}

/// Removes and parses `spec.expose`. An empty list is the same as none.
fn take_expose(doc: &mut ManifestDoc) -> Result<Vec<VmExposeSpec>> {
    let Some(map) = doc.spec.as_mapping_mut() else {
        return Ok(Vec::new());
    };
    let Some(raw) = map.remove(Value::from("expose")) else {
        return Ok(Vec::new());
    };
    serde_yaml::from_value(raw).map_err(|e| {
        Error::Invalid(super::po::tf(
            "VirtualMachine/{name}: spec.expose: {err}",
            &[("name", &doc.metadata.name), ("err", &e.to_string())],
        ))
    })
}

fn route_for(vm: &ManifestDoc, exposes: &[VmExposeSpec]) -> Result<ManifestDoc> {
    let vm_name = &vm.metadata.name;
    let bad = |what: String| {
        Error::Invalid(super::po::tf(
            "VirtualMachine/{name}: spec.expose: {what}",
            &[("name", vm_name), ("what", &what)],
        ))
    };
    if !super::httproute::valid_service(vm_name) {
        return Err(bad(format!("'{vm_name}' is not a valid backend name")));
    }
    let mut tls: Option<&Value> = None;
    let hosts = &exposes[0].hosts;
    let pool = &exposes[0].pool;
    let mut rules: Vec<Value> = Vec::new();
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for (i, e) in exposes.iter().enumerate() {
        if !super::httproute::valid_host(&e.host) {
            return Err(bad(format!(
                "[{i}].host '{}' is not a valid host name",
                e.host
            )));
        }
        if !super::httproute::valid_path_prefix(&e.path) {
            return Err(bad(format!(
                "[{i}].path '{}' is not a valid path prefix",
                e.path
            )));
        }
        if &e.pool != pool {
            return Err(bad(
                "`pool` must be the same on every entry: the route holds ONE address".into(),
            ));
        }
        if &e.hosts != hosts {
            return Err(bad(
                "`hosts` must be the same on every entry: the route publishes one list for all its names".into(),
            ));
        }
        if e.port == 0 {
            return Err(bad(format!("[{i}].port: 0 is invalid")));
        }
        // The same (host, path) twice is two answers to one question — the proxy
        // would take the first and the second would read as applied.
        if seen.contains(&(e.host.as_str(), e.path.as_str())) {
            return Err(bad(format!(
                "[{i}] repeats host '{}' path '{}'",
                e.host, e.path
            )));
        }
        seen.push((e.host.as_str(), e.path.as_str()));
        if let Some(t) = &e.tls {
            match tls {
                None => tls = Some(t),
                Some(prev) if prev == t => {}
                Some(_) => {
                    return Err(bad(
                        "the proxy terminates TLS with ONE certificate per route set — every entry that sets `tls` must set the same one".into(),
                    ))
                }
            }
        }
        let mut backend = serde_yaml::Mapping::new();
        backend.insert(Value::from("service"), Value::from(vm_name.clone()));
        backend.insert(Value::from("port"), Value::from(e.port as u64));
        let mut path = serde_yaml::Mapping::new();
        path.insert(Value::from("path"), Value::from(e.path.clone()));
        path.insert(Value::from("backend"), Value::Mapping(backend));
        let mut rule = serde_yaml::Mapping::new();
        rule.insert(Value::from("host"), Value::from(e.host.clone()));
        rule.insert(
            Value::from("paths"),
            Value::Sequence(vec![Value::Mapping(path)]),
        );
        rules.push(Value::Mapping(rule));
    }
    let mut spec = serde_yaml::Mapping::new();
    if let Some(t) = tls {
        spec.insert(Value::from("tls"), t.clone());
    }
    if !hosts.is_empty() {
        spec.insert(
            Value::from("hosts"),
            Value::Sequence(hosts.iter().cloned().map(Value::from).collect()),
        );
    }
    if let Some(p) = pool {
        spec.insert(Value::from("pool"), Value::from(p.clone()));
    }
    spec.insert(Value::from("rules"), Value::Sequence(rules));
    Ok(ManifestDoc {
        api_version: vm.api_version.clone(),
        kind: k::HTTP_ROUTE.to_string(),
        metadata: Metadata {
            name: format!("{vm_name}{ROUTE_SUFFIX}"),
            ..vm.metadata.clone()
        },
        spec: Value::Mapping(spec),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(spec: &str) -> ManifestDoc {
        ManifestDoc {
            api_version: "compute.delonix.io/v1alpha1".into(),
            kind: k::VM.into(),
            metadata: serde_yaml::from_str("name: web01").unwrap(),
            spec: serde_yaml::from_str(spec).unwrap(),
        }
    }

    #[test]
    fn a_vm_without_expose_passes_through_untouched() {
        let out = lower_vm_expose(vec![vm("disk: x")]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, k::VM);
    }

    #[test]
    fn expose_becomes_a_route_and_leaves_the_vm_without_the_key() {
        let out = lower_vm_expose(vec![vm(
            "disk: x\nexpose:\n  - {host: app.example.pt, port: 8080}\n  - {host: api.example.pt, port: 9000, path: /v1}",
        )])
        .unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].spec.get("expose").is_none());
        assert_eq!(out[1].kind, k::HTTP_ROUTE);
        assert_eq!(out[1].metadata.name, "web01-expose");
        let rules = out[1].spec.get("rules").unwrap().as_sequence().unwrap();
        assert_eq!(rules.len(), 2);
        let b = &rules[1]["paths"][0];
        assert_eq!(b["path"], "/v1");
        assert_eq!(b["backend"]["service"], "web01");
        assert_eq!(b["backend"]["port"], 9000);
    }

    #[test]
    fn a_bad_host_port_or_path_is_refused_by_name() {
        for bad in [
            "expose: [{host: 'a b', port: 80}]",
            "expose: [{host: a.pt, port: 0}]",
            "expose: [{host: a.pt, port: 80, path: 'no-slash'}]",
            "expose: [{host: a.pt, port: 80}, {host: a.pt, port: 81}]",
        ] {
            let e = lower_vm_expose(vec![vm(bad)]).unwrap_err().to_string();
            assert!(e.contains("web01"), "{bad}: {e}");
        }
    }

    #[test]
    fn two_different_certificates_in_one_route_set_are_refused() {
        let e = lower_vm_expose(vec![vm(
            "expose:\n  - {host: a.pt, port: 80, tls: {mode: selfSigned}}\n  - {host: b.pt, port: 80, tls: {mode: secretRef, secretRef: s}}",
        )])
        .unwrap_err()
        .to_string();
        assert!(e.contains("ONE certificate"), "{e}");
    }

    #[test]
    fn hosts_are_carried_to_the_route_and_must_agree_across_entries() {
        let out = lower_vm_expose(vec![vm(
            "expose:\n  - {host: a.pt, port: 80, hosts: [host]}\n  - {host: b.pt, port: 81, hosts: [host]}",
        )])
        .unwrap();
        assert_eq!(out[1].spec["hosts"][0], "host");
        let e = lower_vm_expose(vec![vm(
            "expose:\n  - {host: a.pt, port: 80, hosts: [host]}\n  - {host: b.pt, port: 81}",
        )])
        .unwrap_err()
        .to_string();
        assert!(e.contains("same on every entry"), "{e}");
    }

    #[test]
    fn pool_is_carried_to_the_route_and_must_agree_across_entries() {
        let out = lower_vm_expose(vec![vm(
            "expose:\n  - {host: a.pt, port: 80, pool: edge}\n  - {host: b.pt, port: 81, pool: edge}",
        )])
        .unwrap();
        assert_eq!(out[1].spec["pool"], "edge");
        let e = lower_vm_expose(vec![vm(
            "expose:\n  - {host: a.pt, port: 80, pool: edge}\n  - {host: b.pt, port: 81, pool: other}",
        )])
        .unwrap_err()
        .to_string();
        assert!(e.contains("`pool` must be the same"), "{e}");
    }

    #[test]
    fn the_same_tls_block_on_every_entry_is_carried_to_the_route() {
        let out = lower_vm_expose(vec![vm(
            "expose:\n  - {host: a.pt, port: 80, tls: {mode: selfSigned}}\n  - {host: b.pt, port: 80, tls: {mode: selfSigned}}",
        )])
        .unwrap();
        assert_eq!(out[1].spec["tls"]["mode"], "selfSigned");
    }
}
