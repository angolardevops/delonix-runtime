//! Parser for a real **kind** config file (`kind.x-k8s.io/v1alpha4`), the one
//! the upstream `kind` CLI reads with `--config`.
//!
//! This is NOT the generic `kind:` field of a delonix manifest — it is the
//! Kubernetes-IN-Docker project's own `Cluster` schema. A delonix manifest is
//! `apiVersion: delonix.io/v1`, `kind: KubernetesCluster`, `spec: {...}` and is
//! loaded by [`super::manifest`]; a kind config is `apiVersion:
//! kind.x-k8s.io/v1alpha4`, `kind: Cluster`, with `nodes:`/`networking:` at the
//! top level. The two never share a loader, which is why this module exists
//! beside [`super::kindmode`] rather than folding into the manifest path.
//!
//! # The field policy, and why it is refusal and not warn-and-ignore
//!
//! delonix's native kind mode ([`super::kindmode`]) boots `kindest/node`
//! directly on its own engine and writes its OWN rootless kubeadm/containerd
//! config. Several kind fields have no equivalent here, and the house rule
//! (AGENTS.md: "reports success, does nothing" is the bug this project refuses)
//! says a field that reads as applied and was silently dropped is worse than an
//! error. So every field is classified:
//!
//! * **Honoured** — `name`, `nodes[].role`, `nodes[].image`, and
//!   `networking.{podSubnet,serviceSubnet,apiServerPort,disableDefaultCNI}`,
//!   plus the networking knobs whose ONLY accepted value is what delonix already
//!   does (`ipFamily: ipv4`, `apiServerAddress: 127.0.0.1`, `kubeProxyMode:
//!   iptables`).
//! * **Refused by name** (DX-1707) — everything else, whether a recognised kind
//!   feature delonix does not implement (`extraMounts`, `extraPortMappings`,
//!   `kubeadmConfigPatches*`, `containerdConfigPatches*`, `featureGates`,
//!   `runtimeConfig`, a divergent `kubeProxyMode`/`ipFamily`/`apiServerAddress`)
//!   or an unrecognised key (a typo). Unlike the manifest loader
//!   ([`super::manifest::warn_unknown_fields_in`], which warns), this path
//!   refuses with a non-zero exit.

use delonix_model::{Error, Result};
use serde::Deserialize;
use serde_yaml::Value;

/// The code dictionary entry every refusal in this module carries (ADR-0043).
const FIELD_CODE: u16 = 1707;

/// kind's own defaults, used when the config omits the field (so a config that
/// sets nothing behaves like `kind create cluster` with no `--config`).
const DEFAULT_POD_SUBNET: &str = "10.244.0.0/16";
const DEFAULT_SERVICE_SUBNET: &str = "10.96.0.0/12";

/// The accepted top-level keys. Everything else is refused by name.
const TOP_LEVEL_OK: &[&str] = &["kind", "apiVersion", "name", "nodes", "networking"];
/// The accepted keys of a `nodes[]` entry.
const NODE_OK: &[&str] = &["role", "image"];
/// The accepted keys of `networking`.
const NETWORKING_OK: &[&str] = &[
    "podSubnet",
    "serviceSubnet",
    "apiServerPort",
    "disableDefaultCNI",
    "ipFamily",
    "apiServerAddress",
    "kubeProxyMode",
];

/// A kind config, reduced to what delonix's kind mode can act on.
///
/// Every field is an `Option`/count so the caller can tell "the config set it"
/// from "the config was silent" and fill the silent ones with delonix's own
/// defaults (which match kind's).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct KindConfig {
    /// `name:` at the top level (kind's cluster name). `None` = none given.
    pub name: Option<String>,
    /// A uniform `nodes[].image`, when the config pins one. `None` = use the
    /// engine default (`kindmode::DEFAULT_NODE_IMAGE`).
    pub image: Option<String>,
    /// Count of `role: control-plane` nodes (defaults to 1 for an omitted
    /// `nodes:`, like kind).
    pub control_planes: u32,
    /// Count of `role: worker` nodes.
    pub workers: u32,
    pub pod_subnet: Option<String>,
    pub service_subnet: Option<String>,
    /// `networking.apiServerPort`. `0` in the file means "pick a free one" and
    /// is normalised to `None` here (same meaning as the absent flag).
    pub api_port: Option<u16>,
    /// `networking.disableDefaultCNI` — `true` maps to `cni: none`.
    pub disable_default_cni: bool,
}

/// Does this text look like a kind config rather than a delonix manifest?
///
/// Cheap and syntactic on purpose: it runs on the `cluster apply` path BEFORE
/// the delonix manifest loader (which would reject `apiVersion:
/// kind.x-k8s.io/...` as an unknown group) gets the document. A line that
/// declares an `apiVersion` under the `kind.x-k8s.io` group is the signal —
/// the same shape of check `manifest::is_delonix_manifest` uses for its own
/// group, so a commented-out or quoted mention elsewhere does not trip it.
pub(crate) fn looks_like_kind_config(text: &str) -> bool {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("apiVersion:"))
        .any(|v| {
            v.trim()
                .trim_matches(['"', '\''])
                .starts_with("kind.x-k8s.io/")
        })
}

/// Parses a kind config, honouring what delonix can and refusing the rest by
/// name (DX-1707). `label` is what an error calls the source (a path).
pub(crate) fn parse(text: &str, label: &str) -> Result<KindConfig> {
    // A kind config is a SINGLE document. More than one is either a stream the
    // kind schema does not define or a delonix manifest that slipped in here.
    let mut docs = serde_yaml::Deserializer::from_str(text);
    let first = docs.next().ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "{path}: empty kind config (no YAML document)",
            &[("path", label)],
        ))
    })?;
    let root = Value::deserialize(first).map_err(|e| {
        Error::Invalid(super::po::tf(
            "{path}: invalid kind config: {err}",
            &[("path", label), ("err", &e.to_string())],
        ))
    })?;
    if docs.next().is_some() {
        return Err(Error::Invalid(super::po::tf(
            "{path}: a kind config is a single `Cluster` document, but this file has more than one",
            &[("path", label)],
        )));
    }
    parse_value(&root)
}

/// The refusal every unsupported/unknown field raises: coded (DX-1707),
/// non-zero exit, and it NAMES the field so the operator knows what to remove.
fn refuse(field: &str) -> Error {
    Error::coded(
        FIELD_CODE,
        Error::Invalid(super::po::tf(
            "kind config: field '{field}' is not supported by delonix's kind mode — remove it \
             (for a feature delonix does not implement, create with `disableDefaultCNI: true` / \
             `--cni none` and apply your own configuration), see `delonix explain DX-1707`",
            &[("field", field)],
        )),
    )
}

/// The same refusal for a value delonix cannot honour on a field it otherwise
/// reads (e.g. `kubeProxyMode: ipvs`) — names the field AND the value.
fn refuse_value(field: &str, value: &str, only: &str) -> Error {
    Error::coded(
        FIELD_CODE,
        Error::Invalid(super::po::tf(
            "kind config: '{field}: {value}' is not supported by delonix's kind mode (only \
             '{only}' is) — see `delonix explain DX-1707`",
            &[("field", field), ("value", value), ("only", only)],
        )),
    )
}

fn as_mapping<'a>(v: &'a Value, what: &str) -> Result<&'a serde_yaml::Mapping> {
    v.as_mapping().ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "kind config: '{what}' must be a mapping",
            &[("what", what)],
        ))
    })
}

fn parse_value(root: &Value) -> Result<KindConfig> {
    let map = as_mapping(root, "the document")?;

    // `kind:`/`apiVersion:` are identity, not config — validate, do not store.
    if let Some(k) = map.get("kind").and_then(Value::as_str) {
        if k != "Cluster" {
            return Err(Error::Invalid(super::po::tf(
                "kind config: expected `kind: Cluster`, found '{k}'",
                &[("k", k)],
            )));
        }
    }

    let mut cfg = KindConfig::default();

    for (key, val) in map {
        let Some(key) = key.as_str() else {
            return Err(refuse("<non-string key>"));
        };
        match key {
            "kind" | "apiVersion" => {} // identity, handled above
            "name" => {
                cfg.name = Some(require_str(val, "name")?.to_string());
            }
            "nodes" => parse_nodes(val, &mut cfg)?,
            "networking" => parse_networking(val, &mut cfg)?,
            other if TOP_LEVEL_OK.contains(&other) => unreachable!("covered above"),
            other => return Err(refuse(other)),
        }
    }

    // An omitted `nodes:` is kind's single control-plane node.
    if cfg.control_planes == 0 && cfg.workers == 0 {
        cfg.control_planes = 1;
    }
    // Nodes present, but none is a control-plane: kubeadm has nothing to init.
    if cfg.control_planes == 0 {
        return Err(Error::Invalid(super::po::t(
            "kind config: no control-plane node — a cluster needs at least one `role: control-plane`",
        ).into()));
    }
    Ok(cfg)
}

/// `nodes:` — a sequence; each entry's `role` counts toward the CP/worker
/// totals, and any field beyond `role`/`image` is refused by name.
fn parse_nodes(val: &Value, cfg: &mut KindConfig) -> Result<()> {
    let seq = val.as_sequence().ok_or_else(|| {
        Error::Invalid(super::po::t("kind config: 'nodes' must be a list").into())
    })?;
    for (i, node) in seq.iter().enumerate() {
        let m = as_mapping(node, "nodes[]")?;
        for (k, _) in m {
            let Some(k) = k.as_str() else {
                return Err(refuse("nodes[].<non-string key>"));
            };
            if !NODE_OK.contains(&k) {
                return Err(refuse(&format!("nodes[].{k}")));
            }
        }
        // kind defaults an absent role to control-plane.
        let role = match m.get("role") {
            Some(v) => require_str(v, "nodes[].role")?,
            None => "control-plane",
        };
        match role {
            "control-plane" => cfg.control_planes += 1,
            "worker" => cfg.workers += 1,
            other => return Err(Error::Invalid(super::po::tf(
                "kind config: nodes[{i}].role '{role}' invalid — only 'control-plane' or 'worker'",
                &[("i", &i.to_string()), ("role", other)],
            ))),
        }
        // A per-node image is honoured only if every node that pins one agrees:
        // delonix boots every node from a SINGLE image, so divergent images
        // could not both be honoured — refusing beats silently picking one.
        if let Some(img) = m.get("image") {
            let img = require_str(img, "nodes[].image")?.to_string();
            match &cfg.image {
                Some(prev) if *prev != img => {
                    return Err(Error::coded(
                        FIELD_CODE,
                        Error::Invalid(
                            super::po::t(
                                "kind config: nodes[].image differs between nodes — delonix boots \
                             every node from one image; use the same image on all nodes",
                            )
                            .into(),
                        ),
                    ))
                }
                _ => cfg.image = Some(img),
            }
        }
    }
    Ok(())
}

/// `networking:` — the honoured CIDRs/port/CNI toggle, plus the knobs whose
/// only accepted value is what delonix already does.
fn parse_networking(val: &Value, cfg: &mut KindConfig) -> Result<()> {
    let m = as_mapping(val, "networking")?;
    for (k, v) in m {
        let Some(k) = k.as_str() else {
            return Err(refuse("networking.<non-string key>"));
        };
        if !NETWORKING_OK.contains(&k) {
            return Err(refuse(&format!("networking.{k}")));
        }
        match k {
            "podSubnet" => {
                let s = require_str(v, "networking.podSubnet")?;
                cfg.pod_subnet = Some(valid_cidr(s, "networking.podSubnet")?);
            }
            "serviceSubnet" => {
                let s = require_str(v, "networking.serviceSubnet")?;
                cfg.service_subnet = Some(valid_cidr(s, "networking.serviceSubnet")?);
            }
            "apiServerPort" => cfg.api_port = parse_api_port(v)?,
            "disableDefaultCNI" => {
                cfg.disable_default_cni = v.as_bool().ok_or_else(|| {
                    Error::Invalid(
                        super::po::t(
                            "kind config: networking.disableDefaultCNI must be true or false",
                        )
                        .into(),
                    )
                })?;
            }
            // Honoured only at the value delonix already produces.
            "ipFamily" => {
                let s = require_str(v, "networking.ipFamily")?;
                if s != "ipv4" {
                    return Err(refuse_value("networking.ipFamily", s, "ipv4"));
                }
            }
            "apiServerAddress" => {
                let s = require_str(v, "networking.apiServerAddress")?;
                if s != "127.0.0.1" && s != "localhost" {
                    return Err(refuse_value("networking.apiServerAddress", s, "127.0.0.1"));
                }
            }
            "kubeProxyMode" => {
                let s = require_str(v, "networking.kubeProxyMode")?;
                if s != "iptables" {
                    return Err(refuse_value("networking.kubeProxyMode", s, "iptables"));
                }
            }
            _ => unreachable!("covered by NETWORKING_OK"),
        }
    }
    Ok(())
}

/// `apiServerPort`: an integer in `1..=65535`; `0` means "pick a free one"
/// (kind's own meaning) and becomes `None`.
fn parse_api_port(v: &Value) -> Result<Option<u16>> {
    let n = v.as_u64().ok_or_else(|| {
        Error::Invalid(
            super::po::t("kind config: networking.apiServerPort must be a number").into(),
        )
    })?;
    if n == 0 {
        return Ok(None);
    }
    if n > u64::from(u16::MAX) {
        return Err(Error::Invalid(super::po::tf(
            "kind config: networking.apiServerPort '{n}' is out of range (1-65535)",
            &[("n", &n.to_string())],
        )));
    }
    Ok(Some(n as u16))
}

fn require_str<'a>(v: &'a Value, what: &str) -> Result<&'a str> {
    v.as_str().ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "kind config: '{what}' must be a string",
            &[("what", what)],
        ))
    })
}

/// A CIDR that parses AND is canonical (its host bits are zero), so the
/// manifest and the cluster cannot disagree about what was asked — the same
/// strictness as DX-1704 on the delonix spec.
fn valid_cidr(s: &str, what: &str) -> Result<String> {
    let c = delonix_sdn::Cidr::parse(s).ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "kind config: '{what}' is not an IPv4 CIDR: '{s}'",
            &[("what", what), ("s", s)],
        ))
    })?;
    let canon = format!("{}/{}", delonix_sdn::Cidr::fmt_u32(c.base), c.len);
    if canon != s {
        return Err(Error::Invalid(super::po::tf(
            "kind config: '{what}' '{s}' has bits set outside its /{len} prefix (did you mean '{canon}'?)",
            &[("what", what), ("s", s), ("len", &c.len.to_string()), ("canon", &canon)],
        )));
    }
    Ok(canon)
}

impl KindConfig {
    /// The pod subnet to apply (the config's, else kind's default).
    pub(crate) fn pod_subnet_or_default(&self) -> String {
        self.pod_subnet
            .clone()
            .unwrap_or_else(|| DEFAULT_POD_SUBNET.to_string())
    }

    /// The service subnet to apply (the config's, else kind's default).
    pub(crate) fn service_subnet_or_default(&self) -> String {
        self.service_subnet
            .clone()
            .unwrap_or_else(|| DEFAULT_SERVICE_SUBNET.to_string())
    }

    /// `cni` string for [`super::kindmode::KindCluster`]: `none` when the
    /// config disabled the default CNI, else `default` (kindnet).
    pub(crate) fn cni(&self) -> &'static str {
        if self.disable_default_cni {
            "none"
        } else {
            "default"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Result<KindConfig> {
        parse(text, "test.yaml")
    }

    const HEADER: &str = "kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\n";

    #[test]
    fn looks_like_kind_config_only_on_the_group() {
        assert!(looks_like_kind_config(HEADER));
        assert!(looks_like_kind_config(
            "apiVersion: \"kind.x-k8s.io/v1alpha4\"\nkind: Cluster\n"
        ));
        assert!(!looks_like_kind_config(
            "apiVersion: delonix.io/v1\nkind: KubernetesCluster\n"
        ));
        // A mention that is not an apiVersion declaration must not trip it.
        assert!(!looks_like_kind_config(
            "# see kind.x-k8s.io/v1alpha4\nspec: {}\n"
        ));
    }

    #[test]
    fn empty_config_is_a_single_control_plane() {
        let c = p(HEADER).unwrap();
        assert_eq!(c.control_planes, 1);
        assert_eq!(c.workers, 0);
        assert_eq!(c.cni(), "default");
        assert_eq!(c.pod_subnet_or_default(), DEFAULT_POD_SUBNET);
    }

    #[test]
    fn honours_roles_networking_and_name() {
        let text = format!(
            "{HEADER}name: lab\n\
             nodes:\n- role: control-plane\n- role: worker\n- role: worker\n\
             networking:\n  podSubnet: 10.10.0.0/16\n  serviceSubnet: 10.20.0.0/16\n  \
             apiServerPort: 6500\n  disableDefaultCNI: true\n"
        );
        let c = p(&text).unwrap();
        assert_eq!(c.name.as_deref(), Some("lab"));
        assert_eq!(c.control_planes, 1);
        assert_eq!(c.workers, 2);
        assert_eq!(c.pod_subnet.as_deref(), Some("10.10.0.0/16"));
        assert_eq!(c.service_subnet.as_deref(), Some("10.20.0.0/16"));
        assert_eq!(c.api_port, Some(6500));
        assert!(c.disable_default_cni);
        assert_eq!(c.cni(), "none");
    }

    #[test]
    fn node_without_role_defaults_to_control_plane() {
        let c = p(&format!("{HEADER}nodes:\n- {{}}\n")).unwrap();
        assert_eq!(c.control_planes, 1);
        assert_eq!(c.workers, 0);
    }

    #[test]
    fn api_server_port_zero_means_pick_one() {
        let c = p(&format!("{HEADER}networking:\n  apiServerPort: 0\n")).unwrap();
        assert_eq!(c.api_port, None);
    }

    #[test]
    fn two_control_planes_are_counted_so_create_can_refuse() {
        // The HA refusal lives in kindmode::create; here we only prove the
        // count reaches it.
        let c = p(&format!(
            "{HEADER}nodes:\n- role: control-plane\n- role: control-plane\n"
        ))
        .unwrap();
        assert_eq!(c.control_planes, 2);
    }

    #[test]
    fn unsupported_top_level_fields_are_refused_by_name() {
        for field in [
            "featureGates",
            "runtimeConfig",
            "kubeadmConfigPatches",
            "containerdConfigPatches",
        ] {
            let text = format!("{HEADER}{field}: [x]\n");
            let e = p(&text).unwrap_err();
            assert_eq!(e.number(), FIELD_CODE, "{field}: {e}");
            assert!(e.to_string().contains(field), "{field}: {e}");
        }
    }

    #[test]
    fn unsupported_node_fields_are_refused_by_name() {
        for field in ["extraMounts", "extraPortMappings", "kubeadmConfigPatches"] {
            let text = format!("{HEADER}nodes:\n- role: worker\n  {field}: [x]\n");
            let e = p(&text).unwrap_err();
            assert_eq!(e.number(), FIELD_CODE, "{field}: {e}");
            assert!(e.to_string().contains(field), "{field}: {e}");
        }
    }

    #[test]
    fn a_typo_is_refused_not_ignored() {
        let e = p(&format!("{HEADER}netwrking:\n  podSubnet: 10.0.0.0/16\n")).unwrap_err();
        assert_eq!(e.number(), FIELD_CODE);
        assert!(e.to_string().contains("netwrking"), "{e}");
    }

    #[test]
    fn divergent_networking_values_are_refused() {
        for (field, line) in [
            ("networking.ipFamily", "ipFamily: ipv6"),
            ("networking.kubeProxyMode", "kubeProxyMode: ipvs"),
            ("networking.apiServerAddress", "apiServerAddress: 1.2.3.4"),
        ] {
            let e = p(&format!("{HEADER}networking:\n  {line}\n")).unwrap_err();
            assert_eq!(e.number(), FIELD_CODE, "{field}: {e}");
            assert!(e.to_string().contains(field), "{field}: {e}");
        }
    }

    #[test]
    fn consistent_no_op_networking_values_are_honoured() {
        let c = p(&format!(
            "{HEADER}networking:\n  ipFamily: ipv4\n  apiServerAddress: 127.0.0.1\n  kubeProxyMode: iptables\n"
        ))
        .unwrap();
        assert_eq!(c.control_planes, 1);
    }

    #[test]
    fn non_canonical_cidr_is_refused() {
        let e = p(&format!(
            "{HEADER}networking:\n  podSubnet: 10.244.0.1/16\n"
        ))
        .unwrap_err();
        assert!(e.to_string().contains("10.244.0.0/16"), "{e}");
    }

    #[test]
    fn a_uniform_node_image_is_honoured_but_divergent_is_refused() {
        let ok = p(&format!(
            "{HEADER}nodes:\n- role: control-plane\n  image: foo:1\n- role: worker\n  image: foo:1\n"
        ))
        .unwrap();
        assert_eq!(ok.image.as_deref(), Some("foo:1"));
        let e = p(&format!(
            "{HEADER}nodes:\n- role: control-plane\n  image: foo:1\n- role: worker\n  image: bar:2\n"
        ))
        .unwrap_err();
        assert_eq!(e.number(), FIELD_CODE, "{e}");
    }

    #[test]
    fn all_workers_no_control_plane_is_refused() {
        let e = p(&format!("{HEADER}nodes:\n- role: worker\n")).unwrap_err();
        assert!(e.to_string().contains("control-plane"), "{e}");
    }

    #[test]
    fn two_documents_are_refused() {
        let e = p(&format!("{HEADER}---\n{HEADER}")).unwrap_err();
        assert!(e.to_string().contains("single"), "{e}");
    }
}
