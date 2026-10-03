//! `NetworkService`, the read half: `GetNetwork` and `ListNetworks`
//! (ADR-0042 step E, first wave).
//!
//! A network of this engine belongs to the node, not to an isolation
//! namespace: every one of them is reported in `default`, `*` lists the same
//! set, and any other namespace has none — an empty list, and `NOT_FOUND` for
//! a name. What a network IS comes from the declarative record
//! (`NetworkStore`); whether it is realized comes from the dataplane record
//! (`NetDef`), never from the document that asked for it.

use std::collections::BTreeSet;
use std::path::Path;

use delonix_sdn::infra;
use tonic::Status;

use crate::proto::v1::{
    Condition, ConditionStatus, GetNetworkRequest, ListNetworksRequest, ListNetworksResponse,
    Network, NetworkSpec, NetworkTopology, OverlayPeer, OverlaySpec, PageResponse,
    ProviderExtensions, ResourceMeta,
};
use crate::selector::Selector;

/// The namespace every network is reported in.
pub const NAMESPACE: &str = "default";
/// `page_size` when the caller sends 0, and the most one page carries.
pub const DEFAULT_PAGE: usize = 100;
pub const MAX_PAGE: usize = 1000;

/// Whether `namespace` is one the node's networks are listed under.
fn has_networks(namespace: &str) -> bool {
    matches!(namespace, "" | NAMESPACE | "*")
}

/// The declarative records under `root`, by name. A state root where no
/// network was ever created has no `networks/` directory — that is an empty
/// list, and reading must not create it.
fn records(root: &Path) -> Result<Vec<delonix_sdn::Network>, Status> {
    if !root.join("networks").is_dir() {
        return Ok(Vec::new());
    }
    let store = delonix_sdn::NetworkStore::open(root)
        .map_err(|e| Status::internal(format!("opening the network store: {e}")))?;
    let mut all = store
        .list()
        .map_err(|e| Status::internal(format!("listing networks: {e}")))?;
    all.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(all)
}

/// The names that have a dataplane record under `root`.
fn realized(root: &Path) -> BTreeSet<String> {
    infra::network_list_in(root)
        .into_iter()
        .map(|d| d.name)
        .collect()
}

fn text(s: &str) -> pbjson_types::Value {
    pbjson_types::Value {
        kind: Some(pbjson_types::value::Kind::StringValue(s.to_string())),
    }
}

/// FNV-1a, 64 bits: the etag is an opaque version of what the caller can see,
/// stable across processes (a `std` hasher is not).
fn fnv(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// One record as the contract's `Network`.
pub fn message(n: &delonix_sdn::Network, realized: bool) -> Network {
    let path = format!("/v1/namespaces/{NAMESPACE}/networks/{}", n.name);
    let overlay = (n.driver == "overlay").then(|| OverlaySpec {
        vni: n.vni.unwrap_or_default(),
        encrypted: n.wg_ip.is_some(),
        peers: n
            .peers
            .iter()
            .map(|p| {
                let (node_ip, wg) = delonix_sdn::parse_overlay_peer(p);
                let (public_key, tunnel_ip) = wg.unwrap_or_default();
                OverlayPeer {
                    node_ip,
                    public_key,
                    tunnel_ip,
                }
            })
            .collect(),
    });
    // The contract names a topology; HOW the node realizes it is the
    // provider's, and is reported under the provider's own key.
    let mut native = std::collections::HashMap::from([
        ("driver".to_string(), text(&n.driver)),
        ("bridge".to_string(), text(&n.bridge)),
        ("gateway".to_string(), text(&n.gateway)),
    ]);
    if let Some(parent) = &n.parent {
        native.insert("parent".to_string(), text(parent));
    }
    let condition = match (n.driver.as_str(), realized) {
        ("bridge" | "overlay", true) => Condition {
            r#type: "Realized".into(),
            status: ConditionStatus::True as i32,
            reason: "DataplaneRecorded".into(),
            message: "the node has a dataplane record for this network".into(),
            last_transition_time: None,
        },
        ("bridge" | "overlay", false) => Condition {
            r#type: "Realized".into(),
            status: ConditionStatus::False as i32,
            reason: "DataplaneRecordMissing".into(),
            message: "the network is declared and the node has no dataplane record for it".into(),
            last_transition_time: None,
        },
        _ => Condition {
            r#type: "Realized".into(),
            status: ConditionStatus::False as i32,
            reason: "DriverNotImplemented".into(),
            message: format!(
                "the '{}' driver is recorded and not realized by this engine",
                n.driver
            ),
            last_transition_time: None,
        },
    };
    Network {
        meta: Some(ResourceMeta {
            name: n.name.clone(),
            namespace: NAMESPACE.to_string(),
            labels: n.labels.clone().into_iter().collect(),
            annotations: n.annotations.clone().into_iter().collect(),
            etag: fnv(&format!("{n:?}|{}", condition.reason)),
            ..Default::default()
        }),
        spec: Some(NetworkSpec {
            topology: if overlay.is_some() {
                NetworkTopology::Overlay as i32
            } else {
                NetworkTopology::Bridge as i32
            },
            ipv4_cidr: n.subnet.clone(),
            overlay,
            extensions: Some(ProviderExtensions {
                by_provider: std::collections::HashMap::from([(
                    "linux".to_string(),
                    pbjson_types::Struct { fields: native },
                )]),
            }),
        }),
        conditions: vec![condition],
        links: vec![
            crate::node::link("self", &path),
            crate::node::link(
                "collection",
                &format!("/v1/namespaces/{NAMESPACE}/networks"),
            ),
        ],
    }
}

/// `GetNetwork` under `root`.
pub fn get_in(root: &Path, req: &GetNetworkRequest) -> Result<Network, Status> {
    let missing = || {
        Status::not_found(format!(
            "network '{}' in namespace '{}'",
            req.name,
            if req.namespace.is_empty() {
                NAMESPACE
            } else {
                &req.namespace
            }
        ))
    };
    if !has_networks(&req.namespace) || req.namespace == "*" {
        return Err(missing());
    }
    let record = records(root)?
        .into_iter()
        .find(|n| n.name == req.name)
        .ok_or_else(missing)?;
    Ok(message(&record, realized(root).contains(&record.name)))
}

/// `ListNetworks` under `root`: filtered by `label_selector`, ordered by
/// name, one page at a time.
pub fn list_in(root: &Path, req: &ListNetworksRequest) -> Result<ListNetworksResponse, Status> {
    let selector = Selector::parse(&req.label_selector).map_err(Status::invalid_argument)?;
    let page = req.page.clone().unwrap_or_default();
    let size = match page.page_size {
        0 => DEFAULT_PAGE,
        n if n < 0 => {
            return Err(Status::invalid_argument(format!(
                "page_size {n}: has to be 0 (the default, {DEFAULT_PAGE}) or more"
            )))
        }
        n => (n as usize).min(MAX_PAGE),
    };
    let after = match page.page_token.as_str() {
        "" => None,
        token => Some(decode_token(token).ok_or_else(|| {
            Status::invalid_argument(format!("page_token '{token}' was not issued by this list"))
        })?),
    };
    let ns = if req.namespace.is_empty() {
        NAMESPACE
    } else {
        &req.namespace
    };
    let all = if has_networks(&req.namespace) {
        records(root)?
    } else {
        Vec::new()
    };
    let live = realized(root);
    let mut matching = all
        .iter()
        .filter(|n| selector.matches(&n.labels))
        .filter(|n| after.as_ref().is_none_or(|a| n.name.as_str() > a.as_str()));
    let networks: Vec<Network> = matching
        .by_ref()
        .take(size)
        .map(|n| message(n, live.contains(&n.name)))
        .collect();
    let next = match (matching.next(), networks.last()) {
        (Some(_), Some(last)) => encode_token(
            last.meta
                .as_ref()
                .map(|m| m.name.as_str())
                .unwrap_or_default(),
        ),
        _ => String::new(),
    };
    let href = |token: &str| {
        let mut q = Vec::new();
        if !req.label_selector.is_empty() {
            q.push(format!(
                "label_selector={}",
                query_escape(&req.label_selector)
            ));
        }
        if page.page_size != 0 {
            q.push(format!("page.page_size={}", page.page_size));
        }
        if !token.is_empty() {
            q.push(format!("page.page_token={token}"));
        }
        let base = format!("/v1/namespaces/{}/networks", query_escape(ns));
        if q.is_empty() {
            base
        } else {
            format!("{base}?{}", q.join("&"))
        }
    };
    let mut links = vec![
        crate::node::link("self", &href(&page.page_token)),
        crate::node::link("root", "/v1"),
    ];
    if !next.is_empty() {
        links.push(crate::node::link("next", &href(&next)));
    }
    Ok(ListNetworksResponse {
        networks,
        page: Some(PageResponse {
            next_page_token: next,
        }),
        links,
    })
}

/// A page token is the last name of the page before, in hex: opaque to the
/// caller, and nothing the server has to remember.
fn encode_token(name: &str) -> String {
    name.bytes().map(|b| format!("{b:02x}")).collect()
}

fn decode_token(token: &str) -> Option<String> {
    if !token.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..token.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(token.get(i..i + 2)?, 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

/// Percent-encodes what a query value or a path segment cannot carry as is.
fn query_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'*' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::v1::PageRequest;

    /// A state root with three declared networks; `lab-a` and `lab-b` also
    /// have a dataplane record, `lab-c` does not.
    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let nets = dir.path().join("networks");
        std::fs::create_dir_all(&nets).unwrap();
        std::fs::write(
            nets.join("lab-a"),
            "base=231\nlabel.app=web\nlabel.tier=front\n",
        )
        .unwrap();
        std::fs::write(nets.join("lab-b"), "base=232\nlabel.app=db\n").unwrap();
        std::fs::write(
            nets.join("lab-c"),
            "driver=overlay\nbase=233\nvni=42\npeers=192.0.2.7,192.0.2.8=cHVia2V5cHVia2V5cHVia2V5cHVia2V5cHVia2V5cHU9=10.99.0.2\nwgip=10.99.0.1\n",
        )
        .unwrap();
        let defs = dir.path().join("ingress").join("networks");
        std::fs::create_dir_all(&defs).unwrap();
        for (name, base) in [("lab-a", 231), ("lab-b", 232)] {
            let def = serde_json::json!({
                "name": name, "bridge": delonix_sdn::bridge_name(name),
                "prefix": format!("10.{base}"), "gateway": format!("10.{base}.0.1"),
            });
            std::fs::write(defs.join(format!("{name}.json")), def.to_string()).unwrap();
        }
        dir
    }

    fn list(root: &Path, selector: &str, size: i32, token: &str) -> ListNetworksResponse {
        list_in(
            root,
            &ListNetworksRequest {
                namespace: String::new(),
                label_selector: selector.into(),
                page: Some(PageRequest {
                    page_size: size,
                    page_token: token.into(),
                }),
            },
        )
        .unwrap()
    }

    fn names(r: &ListNetworksResponse) -> Vec<String> {
        r.networks
            .iter()
            .map(|n| n.meta.as_ref().unwrap().name.clone())
            .collect()
    }

    #[test]
    fn a_network_is_reported_from_its_record_and_its_dataplane_record() {
        let dir = root();
        let a = get_in(
            dir.path(),
            &GetNetworkRequest {
                name: "lab-a".into(),
                namespace: String::new(),
            },
        )
        .unwrap();
        let meta = a.meta.as_ref().unwrap();
        assert_eq!(
            (meta.name.as_str(), meta.namespace.as_str()),
            ("lab-a", "default")
        );
        assert_eq!(meta.labels["app"], "web");
        let spec = a.spec.as_ref().unwrap();
        assert_eq!(spec.topology, NetworkTopology::Bridge as i32);
        assert_eq!(spec.ipv4_cidr, "10.231.0.0/16");
        assert!(spec.overlay.is_none());
        assert_eq!(a.conditions[0].reason, "DataplaneRecorded");
        assert_eq!(a.conditions[0].status, ConditionStatus::True as i32);
        assert!(a
            .links
            .iter()
            .any(|l| l.rel == "self" && l.href == "/v1/namespaces/default/networks/lab-a"));

        // Declared, never realized: the condition says so, read from the node.
        let c = get_in(
            dir.path(),
            &GetNetworkRequest {
                name: "lab-c".into(),
                namespace: "default".into(),
            },
        )
        .unwrap();
        assert_eq!(c.conditions[0].reason, "DataplaneRecordMissing");
        assert_eq!(c.conditions[0].status, ConditionStatus::False as i32);
        let overlay = c.spec.as_ref().unwrap().overlay.as_ref().unwrap();
        assert_eq!((overlay.vni, overlay.encrypted), (42, true));
        assert_eq!(overlay.peers.len(), 2);
        assert_eq!(overlay.peers[0].node_ip, "192.0.2.7");
        assert!(overlay.peers[0].public_key.is_empty());
        assert_eq!(overlay.peers[1].tunnel_ip, "10.99.0.2");
    }

    #[test]
    fn the_etag_follows_what_the_caller_can_see() {
        let dir = root();
        let get = || {
            get_in(
                dir.path(),
                &GetNetworkRequest {
                    name: "lab-b".into(),
                    namespace: String::new(),
                },
            )
            .unwrap()
        };
        let first = get().meta.unwrap().etag;
        assert_eq!(first.len(), 16);
        assert_eq!(
            first,
            get().meta.unwrap().etag,
            "unchanged record, unchanged etag"
        );
        std::fs::write(
            dir.path().join("networks/lab-b"),
            "base=232\nlabel.app=cache\n",
        )
        .unwrap();
        let relabelled = get().meta.unwrap().etag;
        assert_ne!(first, relabelled);
        std::fs::remove_file(dir.path().join("ingress/networks/lab-b.json")).unwrap();
        assert_ne!(
            relabelled,
            get().meta.unwrap().etag,
            "losing the dataplane record is a change"
        );
    }

    #[test]
    fn another_namespace_has_no_networks() {
        let dir = root();
        let other = list_in(
            dir.path(),
            &ListNetworksRequest {
                namespace: "prod".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(other.networks.is_empty());
        let e = get_in(
            dir.path(),
            &GetNetworkRequest {
                name: "lab-a".into(),
                namespace: "prod".into(),
            },
        )
        .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        let e = get_in(
            dir.path(),
            &GetNetworkRequest {
                name: "nope".into(),
                namespace: String::new(),
            },
        )
        .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        let all = list_in(
            dir.path(),
            &ListNetworksRequest {
                namespace: "*".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(names(&all), ["lab-a", "lab-b", "lab-c"]);
        // A root where nothing was ever created: empty, and still not created.
        let empty = tempfile::tempdir().unwrap();
        assert!(list_in(empty.path(), &ListNetworksRequest::default())
            .unwrap()
            .networks
            .is_empty());
        assert!(!empty.path().join("networks").exists());
    }

    #[test]
    fn the_list_filters_by_label_and_pages_by_name() {
        let dir = root();
        assert_eq!(names(&list(dir.path(), "app=web", 0, "")), ["lab-a"]);
        assert_eq!(
            names(&list(dir.path(), "app!=web", 0, "")),
            ["lab-b", "lab-c"]
        );
        assert_eq!(names(&list(dir.path(), "tier", 0, "")), ["lab-a"]);

        let first = list(dir.path(), "", 2, "");
        assert_eq!(names(&first), ["lab-a", "lab-b"]);
        let token = first.page.as_ref().unwrap().next_page_token.clone();
        assert!(!token.is_empty());
        let next = first
            .links
            .iter()
            .find(|l| l.rel == "next")
            .expect("a next link");
        assert_eq!(
            next.href,
            format!("/v1/namespaces/default/networks?page.page_size=2&page.page_token={token}")
        );
        let second = list(dir.path(), "", 2, &token);
        assert_eq!(names(&second), ["lab-c"]);
        assert!(second.page.as_ref().unwrap().next_page_token.is_empty());
        assert!(!second.links.iter().any(|l| l.rel == "next"));
        // A page that ends exactly at the last record offers no next page.
        let exact = list(dir.path(), "", 3, "");
        assert!(exact.page.unwrap().next_page_token.is_empty());

        let bad = |req: ListNetworksRequest| list_in(dir.path(), &req).unwrap_err().code();
        let page = |size, token: &str| {
            Some(PageRequest {
                page_size: size,
                page_token: token.into(),
            })
        };
        assert_eq!(
            bad(ListNetworksRequest {
                page: page(-1, ""),
                ..Default::default()
            }),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            bad(ListNetworksRequest {
                page: page(0, "zz"),
                ..Default::default()
            }),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            bad(ListNetworksRequest {
                label_selector: "a in (b)".into(),
                ..Default::default()
            }),
            tonic::Code::InvalidArgument
        );
    }
}
