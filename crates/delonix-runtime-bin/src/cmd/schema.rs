//! `delonix schema` / `delonix explain` — the manifest schema, **generated from
//! the code** (ADR-0007).
//!
//! Until this existed, the schema lived in two independent places: the Rust
//! structs, which are the truth, and `docs/kinds.html`, written by hand. This
//! repo has already paid for that shape three times (the v0.32.2 sweep found the
//! docs calling `serve docker-api` read-only, `cluster kubeadm` HA-less, and
//! `network` overlay-less — each written correctly once and then quietly become
//! a lie). Generation is the fix; another review pass buys one more correct
//! snapshot, not a property.
//!
//! Two surfaces, one source:
//!
//! - **`delonix schema`** emits JSON Schema. Point an editor at it with one
//!   comment at the top of the manifest and you get completion, type checking
//!   and the field docs inline while you type:
//!   ```yaml
//!   # yaml-language-server: $schema=./delonix.schema.json
//!   ```
//! - **`delonix explain <kind>[.path]`** answers the same question in the
//!   terminal, the way `kubectl explain` does.
//!
//! **The doc-comments on the spec structs are now user-facing.** A note written
//! for a maintainer shows up in somebody's editor. That is a gain, but it is a
//! change in what those comments are for, and it is said out loud here rather
//! than discovered.

use clap::Subcommand;
use delonix_runtime_core::{Error, Result};
use schemars::generate::SchemaSettings;

use super::kinds as k;

/// The Kinds that have a TYPED spec, and therefore a real schema.
///
/// A Kind missing from this list is reported as «no typed schema» rather than
/// omitted — a schema that silently covers half the manifest would validate a
/// typo into looking correct, which is worse than not validating at all.
/// `Storage` is the one Kind deliberately NOT here, and not for lack of
/// attention. It has no spec type of its own: `kind: Storage` is rewritten into
/// `kind: Volume` with an `nfs:`/`cifs:`/`webdav:` block (`lower_storage`),
/// reading the raw `Value`. Typing it would mean authoring a struct that exists
/// only to be described — a second, hand-maintained copy of fields the
/// `NetShareSpec` already owns, which is the exact arrangement ADR-0007 exists
/// to abolish. So it stays untyped and [`no_typed_schema`] names the spelling to
/// use instead.
const TYPED_KINDS: &[&str] = &[
    k::CONTAINER,
    k::POD,
    k::VOLUME,
    k::NETWORK,
    k::VM,
    k::SECRET,
    k::IMAGE,
    k::GATEWAY,
    k::DEPENDENCY,
    k::NETWORK_ROUTE,
    k::HTTP_ROUTE,
    k::INGRESS,
    k::FIREWALL_POLICY,
    k::EGRESS,
    k::WORKLOAD,
    k::CLUSTER,
    k::STACK,
];

/// Fields of NESTED types that serde accepts under a second name, as
/// `(def, canonical, alias)`.
///
/// **schemars does not see `#[serde(alias = ...)]`** — only `rename`. At the TOP
/// level of a Kind that costs nothing, because the accept list injected below
/// comes from the `*_SPEC_FIELDS` constants and those already carry both
/// spellings (`sshKeys` and `ssh_keys` are both in `VM_SPEC_FIELDS`). A nested
/// type has no such list, so nothing put the second spelling anywhere.
///
/// Found by validating `examples/` against the generated schema, which rejected
/// `examples/cluster-ssh.yaml` — a published, working manifest — with «'ip' is a
/// required property», five times over. `address` is the CANONICAL name in the
/// templates and `ip` is the compatibility spelling, so the schema was demanding
/// the one nobody is told to write.
const NESTED_ALIASES: &[(&str, &str, &str)] =
    &[("HostSpec", "ip", "address"), ("SshSpec", "key", "keyPath")];

/// The refusal for a Kind with no typed spec.
///
/// Shared by `schema print --kind` and `explain` so the two cannot drift, and it
/// carries a DIRECTED hint where there is one: told only «no typed schema for
/// Storage», a reader looks for the bug in their manifest, when the answer is
/// that the Kind has a current spelling and this is not it.
fn no_typed_schema(kind: &str) -> Error {
    Error::Invalid(format!(
        "no typed schema for '{kind}'{} — Kinds with one: {}",
        untyped_hint(kind),
        TYPED_KINDS.join(", ")
    ))
}

/// Why a Kind this engine KNOWS has no typed schema, or `""` for a name it does
/// not know at all (a typo, where the list above is the whole answer).
///
/// Split out so `todo_kind_conhecido_tem_schema_ou_dica` can require one: a Kind
/// added to `cmd::kinds` and forgotten here would answer «no typed schema for
/// X», which reads as a defect in the manifest when the truth is a property of
/// the Kind. Same shape as `not_converged_reason` and `no_teardown_reason`.
fn untyped_hint(kind: &str) -> &'static str {
    if kind.eq_ignore_ascii_case(k::SHARE_VOLUME) {
        return " — `kind: ShareVolume` is the deprecated spelling: write `kind: Volume` with a \
                `share: {from: ...}` block, which is what it is rewritten to at load";
    }
    if kind.eq_ignore_ascii_case(k::STORAGE) {
        " — `kind: Storage` is the deprecated spelling: write `kind: Volume` with an \
         `nfs:`/`cifs:`/`webdav:` block, which is what it is rewritten to at load"
    } else {
        ""
    }
}

#[derive(Subcommand)]
pub enum SchemaCmd {
    /// Print the JSON Schema of the manifest (all Kinds, or one).
    Print {
        /// Only this Kind's spec (e.g. `Container`).
        #[arg(long)]
        kind: Option<String>,
    },
}

/// Builds the schema document for the whole manifest.
///
/// Shape: the four envelope fields every document has, plus an `allOf` of
/// `if kind == X then spec: <X's schema>` — which is what lets ONE `$schema`
/// cover a multi-document manifest, since each document selects its own spec by
/// its `kind`.
fn manifest_schema(only: Option<&str>) -> Result<serde_json::Value> {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    let mut branches = Vec::new();
    let mut strict: Vec<(String, &str, &[&str])> = Vec::new();
    for kind in TYPED_KINDS {
        if let Some(k) = only {
            if !k.eq_ignore_ascii_case(kind) {
                continue;
            }
        }
        // The middle element is the `$defs` key schemars will generate, which is
        // the Rust TYPE name and not the Kind. It used to be guessed as
        // `format!("{kind}Spec")` further down, and the guess held only while
        // every typed Kind happened to be named after its struct. It stops
        // holding here — `FirewallPolicy` is `FwDocSpec`, `HTTPRoute` is
        // `HttpRouteSpec` — and the failure mode was silent: the strictness loop
        // did `continue` on a miss, so the Kind entered the schema WITHOUT
        // `additionalProperties: false` and nothing said so. That is the exact
        // shape this schema exists to abolish, so the name is now written down
        // next to the type it belongs to, and a miss is a hard error.
        let (spec, def_name, accepted) = match *kind {
            k::CONTAINER => (
                generator.subschema_for::<super::container::ContainerSpec>(),
                "ContainerSpec",
                super::container::CONTAINER_SPEC_FIELDS,
            ),
            k::POD => (
                generator.subschema_for::<super::container::PodSpec>(),
                "PodSpec",
                super::container::POD_SPEC_FIELDS,
            ),
            k::VOLUME => (
                generator.subschema_for::<super::volume::VolumeSpec>(),
                "VolumeSpec",
                super::volume::VOLUME_SPEC_FIELDS,
            ),
            k::NETWORK => (
                generator.subschema_for::<super::network::NetworkSpec>(),
                "NetworkSpec",
                super::network::NETWORK_SPEC_FIELDS,
            ),
            // `Vm` joins the typed Kinds: it is the largest spec in the manifest
            // (34 fields) and was the one an editor could say nothing about.
            // The accept list is `VM_SPEC_FIELDS`, which also carries the
            // grouped-form keys (`resources:`/`boot:`/`cloudInit:`/`libvirt:`) —
            // hoisted to flat fields before `VmSpec` is deserialized, so they
            // exist in a valid manifest and not in the struct.
            k::VM => (
                generator.subschema_for::<super::vm::VmSpec>(),
                "VmSpec",
                super::vm::VM_SPEC_FIELDS,
            ),
            k::SECRET => (
                generator.subschema_for::<super::secret::SecretSpec>(),
                "SecretSpec",
                super::secret::SECRET_SPEC_FIELDS,
            ),
            k::IMAGE => (
                generator.subschema_for::<super::image::ImageSpec>(),
                "ImageSpec",
                super::image::IMAGE_SPEC_FIELDS,
            ),
            k::GATEWAY => (
                generator.subschema_for::<super::tunnel::TunnelSpec>(),
                "TunnelSpec",
                super::tunnel::TUNNEL_SPEC_FIELDS,
            ),
            k::DEPENDENCY => (
                generator.subschema_for::<super::dependency::DependencySpec>(),
                "DependencySpec",
                super::dependency::DEPENDENCY_SPEC_FIELDS,
            ),
            k::NETWORK_ROUTE => (
                generator.subschema_for::<super::netroute::NetworkRouteSpec>(),
                "NetworkRouteSpec",
                super::netroute::NETWORK_ROUTE_SPEC_FIELDS,
            ),
            k::HTTP_ROUTE => (
                generator.subschema_for::<super::httproute::HttpRouteSpec>(),
                "HttpRouteSpec",
                super::httproute::HTTP_ROUTE_SPEC_FIELDS,
            ),
            k::INGRESS => (
                generator.subschema_for::<super::httproute::IngressSpec>(),
                "IngressSpec",
                super::httproute::INGRESS_SPEC_FIELDS,
            ),
            // One struct, two Kinds: `FirewallPolicy` and the legacy `Egress`
            // share `FwDocSpec` whole — which is why they were merged. Both are
            // listed so a document written either way gets checked; the schema
            // describes what is ACCEPTED, and `Egress` still is.
            k::FIREWALL_POLICY | k::EGRESS => (
                generator.subschema_for::<super::firewall::FwDocSpec>(),
                "FwDocSpec",
                super::firewall::FW_SPEC_FIELDS,
            ),
            k::WORKLOAD => (
                generator.subschema_for::<super::workload::WorkloadSpec>(),
                "WorkloadSpec",
                super::workload::WORKLOAD_SPEC_FIELDS,
            ),
            k::CLUSTER => (
                generator.subschema_for::<super::cluster::ClusterSpec>(),
                "ClusterSpec",
                super::cluster::CLUSTER_SPEC_FIELDS,
            ),
            k::STACK => (
                generator.subschema_for::<super::manifest::StackSpec>(),
                "StackSpec",
                super::manifest::STACK_SPEC_FIELDS,
            ),
            _ => continue,
        };
        strict.push((kind.to_string(), def_name, accepted));
        // `kind: Container` has TWO valid spec shapes: the flat one, and the
        // k8s Pod shape (`spec.containers[]`, still limited to one container).
        // `container::apply` picks between them by the presence of
        // `spec.containers`, so the schema has to offer both — describing only
        // the flat one would reject `examples/pod.yaml`, which is a documented,
        // working manifest.
        let spec = if *kind == k::CONTAINER {
            let pod = generator.subschema_for::<super::container::PodSpec>();
            serde_json::json!({ "anyOf": [spec, pod] })
        } else {
            serde_json::to_value(spec).unwrap_or(serde_json::json!({}))
        };
        branches.push(serde_json::json!({
            "if": { "properties": { "kind": { "const": kind } }, "required": ["kind"] },
            "then": { "properties": { "spec": spec } },
        }));
    }
    if branches.is_empty() {
        return Err(no_typed_schema(only.unwrap_or("?")));
    }
    let mut defs = generator.take_definitions(false);
    // Teach the schema the second spelling of every aliased nested field (see
    // `NESTED_ALIASES`). The alias gets the SAME subschema as the canonical
    // field — they deserialize into one place, so describing them differently
    // would be describing something the engine cannot do — and a REQUIRED field
    // becomes «one of the two», rather than dropping the requirement, which
    // would let a host entry with neither spelling validate clean and then fail
    // at apply.
    for (def_name, canonical, alias) in NESTED_ALIASES {
        let Some(obj) = defs.get_mut(*def_name).and_then(|d| d.as_object_mut()) else {
            continue;
        };
        let Some(sub) = obj
            .get("properties")
            .and_then(|p| p.get(*canonical))
            .cloned()
        else {
            continue;
        };
        if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.insert((*alias).to_string(), sub);
        }
        let required = obj
            .get("required")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        if required.iter().any(|x| x.as_str() == Some(*canonical)) {
            let rest: Vec<_> = required
                .into_iter()
                .filter(|x| x.as_str() != Some(*canonical))
                .collect();
            obj.insert("required".into(), serde_json::Value::Array(rest));
            obj.insert(
                "anyOf".into(),
                serde_json::json!([
                    { "required": [canonical] },
                    { "required": [alias] },
                ]),
            );
        }
    }
    // **A typo in a field name is the number-one mistake when writing a
    // manifest**, and without this the schema does not catch it: JSON Schema
    // allows unknown properties unless told otherwise, so `restartPolicyy`
    // validates clean and then does nothing at apply time.
    //
    // The engine already refuses to be silent about it (`warn_unknown_fields`).
    // The accept list is taken from the SAME `*_SPEC_FIELDS` constants that
    // warning uses, rather than from the generated properties, for one concrete
    // reason: the grouped Container form (`resources:`/`security:`/`storage:`/
    // `limits:`) is hoisted to flat fields BEFORE `ContainerSpec` is ever
    // deserialized, so those keys exist in a valid manifest and do not exist in
    // the struct. Deriving strictness from the struct alone would flag correct
    // manifests — a false positive, which is worse than the gap it closes.
    for (kind, def_name, accepted) in &strict {
        let Some(obj) = defs.get_mut(*def_name).and_then(|d| d.as_object_mut()) else {
            // Loud on purpose. The previous `continue` here meant a Kind whose
            // struct name did not match the guess got NO strictness at all and
            // said nothing — a typo in one of its fields would then validate
            // clean, which is the single thing this block exists to prevent.
            return Err(Error::Invalid(format!(
                "internal: kind '{kind}' names `{def_name}` in $defs and it is not there — \
                 the schema would accept unknown fields for it"
            )));
        };
        if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
            for name in *accepted {
                props
                    .entry(name.to_string())
                    .or_insert_with(|| serde_json::json!({}));
            }
            // `network` and `env` accept TWO shapes each (see
            // `normalize_container_spec`): the flat one the struct describes, and
            // a mapping that is hoisted before the struct ever sees it. The
            // struct only knows the first, so widening them here is what keeps a
            // valid manifest from being flagged.
            //
            // Found by validating `examples/` against the schema, which is the
            // only reason this is a note and not a shipped false positive.
            // `Vm` has the same duality in `network`: a plain string in the flat
            // form, a mapping in the grouped one (`normalize_vm_spec`). Found the
            // same way as the Container case — by validating `examples/` against
            // the generated schema, where `examples/vm.yaml` was rejected for a
            // shape the engine accepts.
            // `Dependency.to` is the same duality by a different mechanism: a
            // `deserialize_with = "string_or_vec"` that takes `to: db` as well
            // as `to: [db, cache]`. schemars describes the DECLARED type
            // (`Vec<String>`) and cannot see the custom deserializer, so the
            // scalar form — the one `examples/dependency.yaml` uses — would be
            // rejected for a shape the engine accepts.
            //
            // The SECOND shape is named per field rather than assumed to be an
            // object: `to:` widens to a plain string, and widening it to an
            // object instead would have kept rejecting the very form the example
            // uses while looking like it had been handled.
            let dual_shaped: &[(&str, &str)] = match kind.as_str() {
                k::CONTAINER => &[("network", "object"), ("env", "object")],
                k::VM => &[("network", "object")],
                k::DEPENDENCY => &[("to", "string")],
                _ => &[],
            };
            for (dual, other) in dual_shaped.iter().copied() {
                if let Some(existing) = props.get(dual).cloned() {
                    props.insert(
                        dual.to_string(),
                        serde_json::json!({ "anyOf": [existing, { "type": other }] }),
                    );
                }
            }
        }
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    Ok(serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Delonix manifest (delonix.io/v1)",
        "type": "object",
        "required": ["apiVersion", "kind", "metadata"],
        "properties": {
            "apiVersion": { "const": "delonix.io/v1" },
            "kind": { "type": "string" },
            "metadata": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" },
                    "namespace": { "type": "string" },
                    "labels": { "type": "object", "additionalProperties": { "type": "string" } },
                    "annotations": { "type": "object", "additionalProperties": { "type": "string" } },
                },
            },
            "spec": { "type": "object" },
        },
        "allOf": branches,
        "$defs": defs,
    }))
}

pub fn run(action: SchemaCmd) -> Result<()> {
    match action {
        SchemaCmd::Print { kind } => {
            let s = manifest_schema(kind.as_deref())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&s)
                    .map_err(|e| Error::Invalid(format!("schema: {e}")))?
            );
            Ok(())
        }
    }
}

/// `delonix explain <kind>[.field[.field…]]` — the same schema, for a human.
///
/// Resolves `$ref`s as it walks, so a nested type reads like the YAML the user
/// is going to write rather than like the JSON Schema it comes from.
pub fn explain(path: &str) -> Result<()> {
    let (kind, rest) = match path.split_once('.') {
        Some((k, r)) => (k, Some(r)),
        None => (path, None),
    };
    // The registry resolves the plural and the shortnames too, so `explain po`
    // and `explain pods` reach `Pod` — before this, only the exact canonical
    // spelling did, which is not the one a caller has in their fingers after
    // typing `get pods`. One resolver for every verb is the point: a second
    // table of abbreviations here is how `po` starts meaning two things.
    //
    // A token the registry does not know falls through to the ORIGINAL refusal
    // on purpose: `no_typed_schema` carries a targeted hint (a Kind can be real
    // and still have no typed schema — `Storage` is rewritten into `Volume`),
    // and «no such resource kind» would read as a defect in the manifest when
    // it is a property of the Kind.
    let asked = super::resource::resolve_kind(kind)
        .map(|f| f.kind)
        .unwrap_or(kind);
    let canon = TYPED_KINDS
        .iter()
        .find(|k| k.eq_ignore_ascii_case(asked))
        .ok_or_else(|| no_typed_schema(kind))?;
    let doc = manifest_schema(Some(canon))?;
    let defs = doc.get("$defs").cloned().unwrap_or(serde_json::json!({}));
    // The spec sits inside the `if/then` branch built above.
    let mut node = doc["allOf"][0]["then"]["properties"]["spec"].clone();
    let mut trail = vec![(*canon).to_string(), "spec".to_string()];
    // `spec` itself is usually a `$ref` into `$defs`.
    node = deref(node, &defs);
    // `spec.containers.image` — a list segment steps THROUGH the item type, the
    // way the user thinks about it, rather than making them type `items`.
    let segs: Vec<&str> = rest
        .map(|r| r.split('.').filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    for (i, seg) in segs.iter().enumerate() {
        let next = node
            .get("properties")
            .and_then(|p| p.get(seg))
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("no field '{seg}' in {}", trail.join("."))))?;
        trail.push((*seg).to_string());
        node = deref(next, &defs);
        // Step THROUGH a list only when there is more path to walk — the user
        // thinks `containers.image`, not `containers.items.image`. Doing it on
        // the LAST segment reported a list's ITEM type as the field's own type:
        // `explain Container.ports` answered `string` for an `array<string>`.
        if i + 1 < segs.len() {
            if let Some(items) = node.get("items").cloned() {
                node = deref(items, &defs);
            }
        }
    }
    print_node(&trail.join("."), &node, &defs);
    Ok(())
}

/// Follows a `$ref` into `$defs`, and picks the CANONICAL branch of an `anyOf`.
///
/// A Kind with two accepted spellings is an `anyOf` in the schema, which a
/// validator handles fine and a human reading `explain` cannot: the union has no
/// `properties` of its own, so walking it produced an empty answer and
/// `explain Container.ports` failed outright — a regression this very schema
/// work introduced when `kind: Container` gained the deprecated Pod spelling.
///
/// The FIRST branch is the canonical one by construction (`manifest_schema`
/// builds it that way), so `explain` describes the shape people should be
/// writing, not the union of everything still accepted.
fn deref(node: serde_json::Value, defs: &serde_json::Value) -> serde_json::Value {
    let node = match node.get("$ref").and_then(|r| r.as_str()) {
        Some(r) => {
            let name = r.rsplit('/').next().unwrap_or_default();
            defs.get(name).cloned().unwrap_or(node)
        }
        None => node,
    };
    // Only when the node has no `properties` of its own. An `anyOf` NEXT TO
    // properties is a constraint on one object (`NESTED_ALIASES` writes one to
    // say «either `ip` or `address`»), not a choice between two shapes —
    // following it there would throw the whole object away and answer from a
    // bare `{"required": [...]}`, which is how `explain Cluster.controlPlane`
    // would have started reporting a type with no fields.
    if node.get("properties").is_some() {
        return node;
    }
    match node
        .get("anyOf")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
    {
        Some(first) => deref(first.clone(), defs),
        None => node,
    }
}

fn print_node(path: &str, node: &serde_json::Value, defs: &serde_json::Value) {
    let mut d = super::output::Describe::new();
    d.field(super::po::t("Path"), path);
    if let Some(t) = type_of(node) {
        d.field(super::po::t("Type"), t);
    }
    if let Some(desc) = node.get("description").and_then(|x| x.as_str()) {
        d.field(super::po::t("Description"), desc);
    }
    d.print();

    let Some(props) = node.get("properties").and_then(|p| p.as_object()) else {
        return;
    };
    let required: Vec<&str> = node
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    println!();
    let mut t = super::output::Table::new(&["FIELD", "TYPE", "REQUIRED", "DESCRIPTION"]);
    for (name, sub) in props {
        let sub = deref(sub.clone(), defs);
        // First sentence only: `explain` is a map, not the manual. The full text
        // is in the JSON Schema, which is what the editor shows.
        let desc = sub
            .get("description")
            .and_then(|x| x.as_str())
            .map(|s| s.split(". ").next().unwrap_or(s).replace('\n', " "))
            .unwrap_or_default();
        t.row(vec![
            name.clone(),
            type_of(&sub).unwrap_or_else(|| "-".into()),
            if required.contains(&name.as_str()) {
                "yes".into()
            } else {
                "no".into()
            },
            desc,
        ]);
    }
    t.print();
}

/// A readable type name. JSON Schema spells an optional field as
/// `["string","null"]`, which is noise for someone writing YAML — the `null` is
/// dropped and the field's optionality is carried by the REQUIRED column
/// instead, where a reader looks for it.
fn type_of(node: &serde_json::Value) -> Option<String> {
    if let Some(items) = node.get("items") {
        return Some(format!(
            "array<{}>",
            type_of(items).unwrap_or_else(|| "object".into())
        ));
    }
    match node.get("type")? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => {
            let names: Vec<&str> = a
                .iter()
                .filter_map(|x| x.as_str())
                .filter(|x| *x != "null")
                .collect();
            (!names.is_empty()).then(|| names.join("|"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of ADR-0007: the schema comes from the STRUCTS. If a
    /// field is renamed in Rust and nobody touches the docs, this is what
    /// notices — the previous arrangement (a hand-written `kinds.html`) could
    /// not, and did not, three separate times.
    #[test]
    fn o_schema_vem_dos_structs_e_nao_de_uma_lista_a_mao() {
        let s = manifest_schema(None).unwrap();
        let defs = &s["$defs"];
        let c = &defs["ContainerSpec"]["properties"];
        assert!(c.get("image").is_some(), "{c:#?}");
        // The camelCase serde rename must survive into the schema — the schema
        // has to describe the YAML, not the Rust field names.
        assert!(
            c.get("restartPolicy").is_some(),
            "the schema must use the manifest spelling, not the Rust one"
        );
        assert!(c.get("restart_policy").is_none());
        // `image` is the one field a container cannot do without.
        let req = s["$defs"]["ContainerSpec"]["required"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(req.iter().any(|x| x == "image"), "{req:?}");
    }

    /// One `$schema` has to cover a multi-document manifest, so the spec is
    /// selected by the document's own `kind`.
    #[test]
    fn cada_kind_selecciona_o_seu_spec_pelo_proprio_kind() {
        let s = manifest_schema(None).unwrap();
        let branches = s["allOf"].as_array().unwrap();
        assert_eq!(branches.len(), TYPED_KINDS.len());
        let kinds: Vec<&str> = branches
            .iter()
            .filter_map(|b| b["if"]["properties"]["kind"]["const"].as_str())
            .collect();
        assert_eq!(kinds, TYPED_KINDS);
    }

    /// A Kind with no typed spec must SAY so. A schema that silently covered
    /// half the manifest would validate a typo into looking correct, which is
    /// worse than not validating.
    #[test]
    fn um_kind_sem_schema_tipado_e_recusado_com_a_lista_dos_que_tem() {
        // `Storage` é agora o único caso, e de propósito: não tem spec própria
        // (é reescrito para `kind: Volume`), por isso a recusa tem de dizer o
        // que escrever em vez dele — senão lê-se como «ninguém chegou lá».
        let e = manifest_schema(Some("Storage")).unwrap_err().to_string();
        assert!(e.contains("Container"), "{e}");
        assert!(
            e.contains("kind: Volume"),
            "sem a alternativa dirigida: {e}"
        );
        assert!(explain("Storage").is_err());
        assert!(explain("Container.naoexiste").is_err());
    }

    /// O inverso do teste acima, e é o que faltava: aquele prova que o `Storage`
    /// tem dica, e nada exigia o mesmo do PRÓXIMO Kind sem schema.
    ///
    /// Um Kind acrescentado ao `cmd::kinds` e esquecido aqui responde «no typed
    /// schema for X» — que se lê como defeito do manifesto quando é uma
    /// propriedade do Kind. Ou tem schema tipado, ou escreve-se o obstáculo:
    /// a mesma exigência que o `not_converged_reason` e o `no_teardown_reason`
    /// já fazem do lado do stack.
    #[test]
    fn todo_kind_conhecido_tem_schema_ou_dica() {
        for f in crate::cmd::kinds::all() {
            if TYPED_KINDS.contains(&f.kind) {
                continue;
            }
            assert!(
                !untyped_hint(f.kind).is_empty(),
                "{} não tem schema tipado nem dica — a recusa lê-se como «ninguém \
                 chegou lá». Ou entra no TYPED_KINDS, ou o `untyped_hint` diz o que \
                 escrever em vez dele.",
                f.kind
            );
        }
    }

    /// A estritez tem de valer para TODOS os Kinds tipados, e a verificação é
    /// derivada do `allOf` em vez de uma lista à mão — foi uma lista à mão
    /// (`["ContainerSpec", "VolumeSpec", "NetworkSpec"]`) que deixou doze Kinds
    /// entrarem no schema sem ninguém dar por isso.
    ///
    /// Antes disto o nome da definição era ADIVINHADO (`format!("{kind}Spec")`)
    /// e um erro de palpite fazia `continue` em silêncio: o Kind ficava sem
    /// `additionalProperties: false` e um typo num dos seus campos validava
    /// limpo. `FirewallPolicy`/`FwDocSpec` e `HTTPRoute`/`HttpRouteSpec` são
    /// exactamente os nomes em que o palpite deixa de bater.
    #[test]
    fn todo_o_kind_tipado_recusa_um_campo_desconhecido() {
        let s = manifest_schema(None).unwrap();
        let defs = &s["$defs"];
        let branches = s["allOf"].as_array().unwrap();
        assert_eq!(branches.len(), TYPED_KINDS.len());
        for b in branches {
            let kind = b["if"]["properties"]["kind"]["const"].as_str().unwrap();
            let spec = deref(b["then"]["properties"]["spec"].clone(), defs);
            assert_eq!(
                spec["additionalProperties"],
                serde_json::Value::Bool(false),
                "{kind} aceita campos desconhecidos — um typo validaria limpo"
            );
        }
    }

    /// **schemars não vê `#[serde(alias = ...)]`**, só `rename`. No topo de um
    /// Kind isso não custa nada (a lista de aceites já carrega as duas grafias),
    /// mas um tipo ANINHADO não tem lista nenhuma — e o `HostSpec` tem o
    /// agravante de o campo ser obrigatório.
    ///
    /// Encontrado a validar os `examples/` contra o schema gerado, que recusava
    /// o `examples/cluster-ssh.yaml` — publicado e a funcionar — cinco vezes com
    /// «'ip' is a required property». `address` é o nome CANÓNICO nos templates,
    /// por isso o schema exigia justamente o que ninguém é mandado escrever.
    #[test]
    fn um_alias_de_campo_aninhado_e_aceite_sem_deixar_de_exigir_um_dos_dois() {
        let s = manifest_schema(Some("KubernetesCluster")).unwrap();
        let host = &s["$defs"]["HostSpec"];
        for grafia in ["ip", "address"] {
            assert!(
                host["properties"].get(grafia).is_some(),
                "`{grafia}` não está no schema do HostSpec"
            );
        }
        // Não basta tirar o `required`: um host sem NENHUMA das duas grafias
        // validaria limpo e só falhava no apply.
        assert!(
            !host["required"]
                .as_array()
                .is_some_and(|r| r.iter().any(|x| x == "ip")),
            "`ip` continua obrigatório — o `address` do exemplo seria recusado"
        );
        let alts = host["anyOf"]
            .as_array()
            .expect("faltam as duas alternativas");
        assert_eq!(alts.len(), 2, "{host:#?}");
    }

    /// O `deref` seguia SEMPRE um `anyOf`. Com a correcção acima o `HostSpec`
    /// passou a ter um `anyOf` ao lado das `properties` — que é uma restrição
    /// sobre um objecto, não uma escolha entre duas formas — e segui-lo deitava
    /// o objecto inteiro fora, respondendo a partir de um `{"required": [...]}`
    /// nu: um tipo sem campos nenhuns.
    #[test]
    fn um_anyof_ao_lado_de_properties_e_restricao_e_nao_uma_uniao() {
        let s = manifest_schema(Some("KubernetesCluster")).unwrap();
        let defs = &s["$defs"];
        let host = deref(defs["HostSpec"].clone(), defs);
        assert!(
            host.get("properties").is_some(),
            "o objecto foi deitado fora ao seguir o anyOf: {host:#?}"
        );
        // E a forma que o `anyOf` REALMENTE descreve — duas grafias do spec de
        // um `kind: Container` — continua a ser atravessada.
        assert!(explain("Container.ports").is_ok());
    }

    /// Walking into a list steps THROUGH the item type: the user thinks
    /// `containers.image`, not `containers.items.image`.
    #[test]
    fn explain_atravessa_uma_lista_sem_obrigar_a_escrever_items() {
        assert!(explain("Pod.containers").is_ok());
        assert!(explain("Pod.containers.image").is_ok());
    }

    #[test]
    fn type_of_esconde_o_null_de_um_campo_opcional() {
        let opt = serde_json::json!({ "type": ["string", "null"] });
        assert_eq!(type_of(&opt).unwrap(), "string");
        let arr = serde_json::json!({ "type": "array", "items": { "type": "string" } });
        assert_eq!(type_of(&arr).unwrap(), "array<string>");
    }

    /// ...but not at the cost of a FALSE POSITIVE, which would be worse than
    /// the gap. Two shapes reach the same fields and only one is in the struct:
    /// the grouped Container form is hoisted BEFORE `ContainerSpec` is ever
    /// deserialized, so `resources:`/`security:`/`storage:`/`limits:` exist in a
    /// valid manifest and do not exist in the type. Deriving strictness from the
    /// struct alone would flag correct manifests — which is why the accept list
    /// comes from the same `*_SPEC_FIELDS` the engine's own warning uses.
    #[test]
    fn a_forma_agrupada_e_as_duas_formas_de_env_e_network_continuam_validas() {
        let s = manifest_schema(None).unwrap();
        let props = &s["$defs"]["ContainerSpec"]["properties"];
        for group in ["resources", "security", "storage", "limits"] {
            assert!(
                props.get(group).is_some(),
                "the grouped form's `{group}:` would be rejected as unknown"
            );
        }
        // `network` and `env` accept a scalar/array OR a mapping.
        for dual in ["network", "env"] {
            assert!(
                props[dual].get("anyOf").is_some(),
                "`{dual}` only describes one of its two accepted shapes"
            );
        }
    }

    /// `kind: Container` has two valid spec shapes — the flat one and the k8s
    /// Pod shape (`spec.containers[]`). `container::apply` picks between them by
    /// the presence of `spec.containers`, so describing only the flat one would
    /// reject `examples/pod.yaml`, a documented working manifest.
    #[test]
    fn kind_container_aceita_a_forma_plana_e_a_forma_pod() {
        let s = manifest_schema(Some("Container")).unwrap();
        let spec = &s["allOf"][0]["then"]["properties"]["spec"];
        let alts = spec["anyOf"].as_array().expect("two shapes expected");
        assert_eq!(alts.len(), 2, "{spec:#?}");
    }

    /// The schema published in `docs/schema/v1/delonix.json` is what an editor
    /// actually fetches, so it has to BE the generated one.
    ///
    /// Without this test the published file becomes exactly what this whole ADR
    /// exists to abolish: a second, hand-updated copy of the truth, silently
    /// drifting. Regenerate with:
    ///
    /// ```text
    /// delonix schema print > docs/schema/v1/delonix.json
    /// ```
    #[test]
    fn o_schema_publicado_esta_em_dia_com_o_codigo() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/schema/v1/delonix.json"
        );
        let published: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("docs/schema/v1/delonix.json is missing"),
        )
        .expect("the published schema is not valid JSON");
        let generated = manifest_schema(None).unwrap();
        assert_eq!(
            published, generated,
            "the published schema is stale — regenerate it with \
             `delonix schema print > docs/schema/v1/delonix.json`"
        );
    }

    /// Um Kind com duas grafias aceites é um `anyOf` no schema — um validador
    /// lida com isso, um humano a ler o `explain` não: a união não tem
    /// `properties` próprias, por isso a resposta vinha VAZIA e o
    /// `explain Container.ports` falhava. Regressão introduzida por este
    /// mesmo trabalho, quando o `kind: Container` ganhou a grafia-Pod.
    #[test]
    fn explain_atravessa_um_anyof_pela_forma_canonica() {
        assert!(explain("Container").is_ok());
        assert!(explain("Container.ports").is_ok());
        assert!(explain("Container.image").is_ok());
    }

    /// Atravessar uma lista só faz sentido quando há mais caminho a percorrer.
    /// Fazê-lo no ÚLTIMO segmento reportava o tipo do ITEM como o tipo do
    /// campo: `Container.ports` respondia `string` para um `array<string>`.
    #[test]
    fn uma_lista_como_folha_reporta_o_tipo_da_lista() {
        let s = manifest_schema(None).unwrap();
        let defs = &s["$defs"];
        let ports = deref(defs["ContainerSpec"]["properties"]["ports"].clone(), defs);
        assert_eq!(type_of(&ports).unwrap(), "array<string>");
    }
}
