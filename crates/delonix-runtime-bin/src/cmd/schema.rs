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

/// The Kinds that have a TYPED spec, and therefore a real schema.
///
/// A Kind missing from this list is reported as «no typed schema» rather than
/// omitted — a schema that silently covers half the manifest would validate a
/// typo into looking correct, which is worse than not validating at all.
const TYPED_KINDS: &[&str] = &["Container", "Pod", "Volume", "Network"];

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
    let mut strict: Vec<(String, &[&str])> = Vec::new();
    for kind in TYPED_KINDS {
        if let Some(k) = only {
            if !k.eq_ignore_ascii_case(kind) {
                continue;
            }
        }
        let (spec, accepted) = match *kind {
            "Container" => (
                generator.subschema_for::<super::container::ContainerSpec>(),
                super::container::CONTAINER_SPEC_FIELDS,
            ),
            "Pod" => (
                generator.subschema_for::<super::container::PodSpec>(),
                super::container::POD_SPEC_FIELDS,
            ),
            "Volume" => (
                generator.subschema_for::<super::volume::VolumeSpec>(),
                super::volume::VOLUME_SPEC_FIELDS,
            ),
            "Network" => (
                generator.subschema_for::<super::network::NetworkSpec>(),
                super::network::NETWORK_SPEC_FIELDS,
            ),
            _ => continue,
        };
        strict.push((kind.to_string(), accepted));
        // `kind: Container` has TWO valid spec shapes: the flat one, and the
        // k8s Pod shape (`spec.containers[]`, still limited to one container).
        // `container::apply` picks between them by the presence of
        // `spec.containers`, so the schema has to offer both — describing only
        // the flat one would reject `examples/pod.yaml`, which is a documented,
        // working manifest.
        let spec = if *kind == "Container" {
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
        return Err(Error::Invalid(format!(
            "no typed schema for '{}' — Kinds with one: {}",
            only.unwrap_or("?"),
            TYPED_KINDS.join(", ")
        )));
    }
    let mut defs = generator.take_definitions(false);
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
    for (kind, accepted) in &strict {
        let def_name = format!("{}Spec", if kind == "Pod" { "Pod" } else { kind });
        let Some(obj) = defs.get_mut(&def_name).and_then(|d| d.as_object_mut()) else {
            continue;
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
            if kind == "Container" {
                for dual in ["network", "env"] {
                    if let Some(existing) = props.get(dual).cloned() {
                        props.insert(
                            dual.to_string(),
                            serde_json::json!({ "anyOf": [existing, { "type": "object" }] }),
                        );
                    }
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
    let canon = TYPED_KINDS
        .iter()
        .find(|k| k.eq_ignore_ascii_case(kind))
        .ok_or_else(|| {
            Error::Invalid(format!(
                "no typed schema for '{kind}' — Kinds with one: {}",
                TYPED_KINDS.join(", ")
            ))
        })?;
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
        let e = manifest_schema(Some("Cluster")).unwrap_err().to_string();
        assert!(e.contains("Container"), "{e}");
        assert!(explain("Cluster").is_err());
        assert!(explain("Container.naoexiste").is_err());
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

    /// A typo in a field name is the number-one mistake when writing a
    /// manifest, and JSON Schema allows unknown properties unless told
    /// otherwise — without this the schema would validate `restartPolicyy`
    /// clean and the field would then do nothing at apply time. The engine
    /// already refuses to be silent about it (`warn_unknown_fields`); the
    /// schema has to be exactly as strict.
    #[test]
    fn o_schema_recusa_um_campo_desconhecido() {
        let s = manifest_schema(None).unwrap();
        for kind in ["ContainerSpec", "VolumeSpec", "NetworkSpec"] {
            assert_eq!(
                s["$defs"][kind]["additionalProperties"],
                serde_json::Value::Bool(false),
                "{kind} accepts unknown fields — a typo would validate clean"
            );
        }
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
