//! `spec.provider` of a `kind: Network` — a segment that a cluster provider
//! (Proxmox SDN today) realizes, written inline in the Network (ADR-0070).
//!
//! ```yaml
//! kind: Network
//! metadata: { name: labnet1 }        # the vnet's name
//! spec:
//!   subnet: 10.80.0.0/24             # provider-neutral
//!   gateway: 10.80.0.1
//!   provider:
//!     name: proxmox
//!     proxmox:
//!       zone: lab                    # the SDN zone this vnet lives in
//!       alias: "lab network 1"
//!       dhcpRange: [{ start: 10.80.0.100, end: 10.80.0.150 }]
//!       reservations: [{ ip: 10.80.0.20, mac: "BC:24:11:00:00:20" }]
//!       dns: { server: pdns, zone: lab.example }   # per zone: all its networks must agree
//! ```
//!
//! A zone is shared by its vnets (created with the first, removed with the
//! last), so the networks that name the same zone are folded at load time into
//! the ONE `NetworkZone` document the executor already reconciles — the same
//! lowering shape as `kind: Dependency` into `NetworkPolicy`. There is no second
//! executor: this module only re-spells the manifest. `kind: NetworkZone` itself
//! still loads, announced as superseded.

use std::collections::BTreeMap;

use delonix_model::{Error, Result};
use serde_yaml::{Mapping, Value};

use super::kinds as k;
use super::manifest::{ManifestDoc, Metadata};

/// Annotation on a document this module synthesized.
pub(crate) const LOWERED_FROM: &str = "delonix.io/lowered-from";

/// Keys of `provider.proxmox`.
const PROXMOX_KEYS: &[&str] = &["zone", "alias", "dhcpRange", "reservations", "dns"];
/// Provider-neutral `Network` fields that make no sense for a provider segment.
const NATIVE_ONLY: &[&str] = &["driver", "parent", "vni", "peers", "wgIp", "wg_ip"];

/// A zone being assembled: its vnets, and the zone-level `dns` with the network that declared it.
type ZoneParts = (Vec<Value>, Option<(Value, String)>);

fn bad(net: &str, msg: &str) -> Error {
    Error::Invalid(format!("Network '{net}': {msg}"))
}

pub(crate) fn lower_network_providers(docs: Vec<ManifestDoc>) -> Result<Vec<ManifestDoc>> {
    let mut out = Vec::with_capacity(docs.len());
    // zone -> (vnets, dns, first network that declared dns)
    let mut zones: BTreeMap<String, ZoneParts> = BTreeMap::new();
    for doc in docs {
        if doc.kind != k::NETWORK || doc.spec.get("provider").is_none() {
            out.push(doc);
            continue;
        }
        let net = doc.metadata.name.clone();
        let Value::Mapping(spec) = &doc.spec else {
            out.push(doc);
            continue;
        };
        for key in NATIVE_ONLY {
            if spec.contains_key(*key) {
                return Err(bad(
                    &net,
                    &format!(
                        "'{key}' is a native-SDN field and cannot be combined with spec.provider"
                    ),
                ));
            }
        }
        let Some(Value::Mapping(p)) = spec.get("provider") else {
            return Err(bad(&net, "spec.provider must be a mapping"));
        };
        let name = p.get("name").and_then(Value::as_str);
        for key in p.keys().filter_map(Value::as_str) {
            if key != "name" && key != "proxmox" {
                return Err(bad(
                    &net,
                    &format!("spec.provider.{key}: unknown provider (known: proxmox)"),
                ));
            }
        }
        if name.is_some_and(|n| n != "proxmox") {
            return Err(bad(
                &net,
                &format!(
                    "spec.provider.name is '{}', and only 'proxmox' realizes a Network segment",
                    name.unwrap_or_default()
                ),
            ));
        }
        let Some(Value::Mapping(px)) = p.get("proxmox") else {
            return Err(bad(
                &net,
                "spec.provider needs a `proxmox:` block (with `zone`)",
            ));
        };
        for key in px.keys().filter_map(Value::as_str) {
            if !PROXMOX_KEYS.contains(&key) {
                return Err(bad(
                    &net,
                    &format!("spec.provider.proxmox.{key}: unknown field"),
                ));
            }
        }
        for key in spec.keys().filter_map(Value::as_str) {
            if !matches!(key, "subnet" | "gateway" | "provider") {
                return Err(bad(&net, &format!("spec.{key}: unknown field")));
            }
        }
        let zone = px
            .get("zone")
            .and_then(Value::as_str)
            .ok_or_else(|| bad(&net, "spec.provider.proxmox.zone is required"))?
            .to_string();

        let mut vnet = Mapping::new();
        vnet.insert("name".into(), Value::from(net.clone()));
        if let Some(a) = px.get("alias") {
            vnet.insert("alias".into(), a.clone());
        }
        let dhcp = px.get("dhcpRange").cloned();
        let resv = px.get("reservations").cloned();
        match spec.get("subnet") {
            Some(cidr) => {
                let mut sn = Mapping::new();
                sn.insert("cidr".into(), cidr.clone());
                if let Some(g) = spec
                    .get("gateway")
                    .filter(|g| g.as_str().is_some_and(|s| !s.is_empty()))
                {
                    sn.insert("gateway".into(), g.clone());
                }
                if let Some(d) = dhcp {
                    sn.insert("dhcpRange".into(), d);
                }
                if let Some(r) = resv {
                    sn.insert("reservations".into(), r);
                }
                vnet.insert("subnets".into(), Value::Sequence(vec![Value::Mapping(sn)]));
            }
            None if dhcp.is_some() || resv.is_some() || spec.contains_key("gateway") => {
                return Err(bad(
                    &net,
                    "gateway, dhcpRange and reservations need a `subnet` to attach to",
                ));
            }
            None => {}
        }
        let entry = zones.entry(zone.clone()).or_default();
        if entry.0.iter().any(|v| v.get("name") == vnet.get("name")) {
            return Err(bad(&net, &format!("declared twice in zone '{zone}'")));
        }
        entry.0.push(Value::Mapping(vnet));
        if let Some(dns) = px.get("dns") {
            match &entry.1 {
                Some((prev, first)) if prev != dns => {
                    return Err(bad(
                        &net,
                        &format!(
                            "provider.proxmox.dns differs from the one on Network '{first}' — DNS settings belong to the zone '{zone}', so every network in it must declare the same"
                        ),
                    ));
                }
                Some(_) => {}
                None => entry.1 = Some((dns.clone(), net.clone())),
            }
        }
    }
    for (zone, (vnets, dns)) in zones {
        let mut spec = Mapping::new();
        spec.insert("vnets".into(), Value::Sequence(vnets));
        if let Some((d, _)) = dns {
            spec.insert("dns".into(), d);
        }
        out.push(ManifestDoc {
            api_version: "networking.delonix.io/v1alpha1".into(),
            kind: k::NETWORK_ZONE.into(),
            metadata: Metadata {
                name: zone,
                namespace: None,
                labels: BTreeMap::new(),
                // Marks the document as synthesized, so the load does not
                // announce a Kind the author never wrote.
                annotations: BTreeMap::from([(LOWERED_FROM.to_string(), k::NETWORK.to_string())]),
            },
            spec: Value::Mapping(spec),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(yaml: &str) -> ManifestDoc {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn n(name: &str, zone: &str, extra: &str) -> ManifestDoc {
        net(&format!(
            "apiVersion: networking.delonix.io/v1alpha1\nkind: Network\nmetadata: {{ name: {name} }}\nspec:\n  subnet: 10.80.0.0/24\n  gateway: 10.80.0.1\n  provider:\n    name: proxmox\n    proxmox: {{ zone: {zone}{extra} }}\n"
        ))
    }

    #[test]
    fn networks_naming_a_zone_fold_into_one_zone_with_a_vnet_each() {
        let out = lower_network_providers(vec![
            n("labnet1", "lab", ", alias: first"),
            n("labnet2", "lab", ""),
            n("other", "zb", ""),
        ])
        .unwrap();
        assert_eq!(out.len(), 2);
        let lab = out.iter().find(|d| d.metadata.name == "lab").unwrap();
        assert_eq!(lab.kind, "NetworkZone");
        let vnets = lab.spec["vnets"].as_sequence().unwrap();
        assert_eq!(vnets.len(), 2);
        assert_eq!(vnets[0]["subnets"][0]["cidr"], Value::from("10.80.0.0/24"));
        assert_eq!(vnets[0]["alias"], Value::from("first"));
        // ...and the folded document is one the zone executor accepts as written.
        let spec: super::super::network_zone::NetworkZoneSpecDoc =
            serde_yaml::from_value(lab.spec.clone()).unwrap();
        assert_eq!(spec.vnets.len(), 2);
        assert_eq!(spec.vnets[0].subnets[0].cidr, "10.80.0.0/24");
    }

    #[test]
    fn a_native_network_is_left_alone() {
        let native = net("apiVersion: networking.delonix.io/v1alpha1\nkind: Network\nmetadata: { name: lan }\nspec: { subnet: 10.201.0.0/16 }\n");
        let out = lower_network_providers(vec![native]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "Network");
    }

    #[test]
    fn mistakes_are_refused_with_the_network_named() {
        for (bad_doc, needle) in [
            (n("a", "lab", ", bogus: 1"), "proxmox.bogus"),
            (
                net("apiVersion: v\nkind: Network\nmetadata: { name: a }\nspec: { driver: overlay, provider: { name: proxmox, proxmox: { zone: z } } }\n"),
                "native-SDN",
            ),
            (
                net("apiVersion: v\nkind: Network\nmetadata: { name: a }\nspec: { provider: { name: libvirt, proxmox: { zone: z } } }\n"),
                "only 'proxmox'",
            ),
            (
                net("apiVersion: v\nkind: Network\nmetadata: { name: a }\nspec: { provider: { name: proxmox, proxmox: {} } }\n"),
                "zone is required",
            ),
            (
                net("apiVersion: v\nkind: Network\nmetadata: { name: a }\nspec: { provider: { name: proxmox, proxmox: { zone: z, dhcpRange: [{start: a, end: b}] } } }\n"),
                "need a `subnet`",
            ),
        ] {
            let e = lower_network_providers(vec![bad_doc]).unwrap_err().to_string();
            assert!(e.contains("Network 'a'") && e.contains(needle), "{e}");
        }
    }

    #[test]
    fn dns_is_a_zone_setting_and_must_agree_across_its_networks() {
        let a = n("a", "lab", ", dns: { server: pdns, zone: lab.example }");
        let b = n("b", "lab", ", dns: { server: other, zone: lab.example }");
        let e = lower_network_providers(vec![a.clone(), b])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("every network in it must declare the same"),
            "{e}"
        );
        let c = n("c", "lab", ", dns: { server: pdns, zone: lab.example }");
        let out = lower_network_providers(vec![a, c]).unwrap();
        assert_eq!(out[0].spec["dns"]["server"], Value::from("pdns"));
    }
}
