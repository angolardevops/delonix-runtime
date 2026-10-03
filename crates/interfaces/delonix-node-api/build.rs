//! Generates the `delonix.node.v1` stubs (prost/tonic, server AND client — the
//! client is what the conformance test drives) and the proto3 JSON encoding of
//! every message (pbjson, proto field names — the same naming the published
//! OpenAPI uses, `naming=proto` in `scripts/contract_gate.py`).
//!
//! The sources are the repository's `proto/` and the vendored googleapis, so the
//! contract gate and this crate read the same files.
use std::path::PathBuf;

/// Every file of the contract. All of them are compiled, served or not: the
/// REST route table (`rest::generate`) is derived from the `google.api.http`
/// annotation of every RPC, and a route the engine does not serve yet has to
/// answer 501, not 404.
const CONTRACT_FILES: [&str; 5] = [
    "delonix/node/v1/common.proto",
    "delonix/node/v1/node.proto",
    "delonix/node/v1/compute.proto",
    "delonix/node/v1/infra.proto",
    "delonix/node/v1/operations.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let proto = root.join("proto");
    let googleapis = root.join("third_party/googleapis");
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let descriptors = out.join("delonix_node_v1.bin");
    tonic_build::configure()
        .build_client(true)
        .file_descriptor_set_path(&descriptors)
        // The well-known types come from `pbjson_types`, which carries their
        // proto3 JSON; tonic's default maps them to `prost_types` (no serde),
        // and it only steps aside when told the WKTs are "compiled" here.
        .compile_well_known_types(true)
        .extern_path(".google.protobuf", "::pbjson_types")
        .compile_protos(
            &CONTRACT_FILES.map(|f| proto.join(f)),
            &[proto.clone(), googleapis],
        )?;
    let bytes = std::fs::read(&descriptors)?;
    pbjson_build::Builder::new()
        .register_descriptors(&bytes)?
        .preserve_proto_field_names()
        // Every field, defaults included: proto3 JSON omits a `false` or an
        // empty string by default, and a client reading `capabilities[].supported`
        // would find the key missing exactly on the rows that say no. What
        // `provider ls -o json` prints is the whole record; so is this.
        .emit_fields()
        .build(&[".delonix.node.v1"])?;
    for f in CONTRACT_FILES {
        println!("cargo:rerun-if-changed={}", proto.join(f).display());
    }
    std::fs::write(out.join("rest_routes.rs"), rest::generate(&bytes)?)?;
    // `NodeInfo.engine_commit`: the short hash the CLI's `--version` prints,
    // with the same rule (`bins/delonix-runtime-bin/build.rs`): `git rev-parse
    // --short=9 HEAD`, "unknown" in a tree without git.
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=DELONIX_GIT_HASH={hash}");
    println!(
        "cargo:rerun-if-changed={}",
        root.join(".git/HEAD").display()
    );
    verify_ui_assets(&root.join("third_party/node-api-docs"))?;
    Ok(())
}

/// ADR-0042 D3: the Swagger UI and ReDoc files embedded in the binary are
/// checked against the committed `third_party/node-api-docs/SHA256SUMS` on
/// every build — a
/// changed byte, a missing file, or a file nobody listed fails the build.
/// The files are byte-identical to the upstream npm packages (`third_party/node-api-docs/README.md`).
fn verify_ui_assets(dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS"))?;
    println!(
        "cargo:rerun-if-changed={}",
        dir.join("SHA256SUMS").display()
    );
    let mut listed = BTreeSet::new();
    for line in sums.lines().filter(|l| !l.trim().is_empty()) {
        let (want, file) = line.split_once("  ").ok_or_else(|| {
            format!("third_party/node-api-docs/SHA256SUMS: malformed line '{line}'")
        })?;
        let path = dir.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = std::fs::read(&path).map_err(|e| {
            format!("third_party/node-api-docs/{file} is listed in SHA256SUMS and unreadable: {e}")
        })?;
        let got: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if got != want {
            return Err(format!(
                "third_party/node-api-docs/{file}: sha256 {got} is not the {want} SHA256SUMS records — the embedded \
                 UI assets must be the upstream files, unchanged (its README.md)"
            )
            .into());
        }
        listed.insert(file.to_string());
    }
    for sub in ["swagger-ui", "redoc"] {
        for entry in std::fs::read_dir(dir.join(sub))? {
            let name = format!("{sub}/{}", entry?.file_name().to_string_lossy());
            if !listed.contains(&name) {
                return Err(format!(
                    "third_party/node-api-docs/{name} is not listed in its SHA256SUMS"
                )
                .into());
            }
        }
    }
    Ok(())
}

/// ADR-0042 step C: the REST encoding is GENERATED from the contract. This
/// reads the descriptor set the stubs were built from and writes, per RPC that
/// carries a `google.api.http` rule, one row of the route table (method, path
/// template, body rule, the request fields a query string may bind) and one
/// arm of its service's dispatcher. Nothing about a route is written by hand,
/// so a route cannot disagree with `proto/` — or with the OpenAPI document,
/// which the contract gate generates from the same annotations.
mod rest {
    use prost::Message;
    use std::collections::BTreeMap;
    use std::fmt::Write as _;

    // The slice of `descriptor.proto` the generator needs. `google.api.http` is
    // extension 72295728 of `MethodOptions`; prost-types drops extensions, so
    // the option message is declared here with that tag as an ordinary field.
    #[derive(Clone, PartialEq, Message)]
    struct FileSet {
        #[prost(message, repeated, tag = "1")]
        file: Vec<File>,
    }
    #[derive(Clone, PartialEq, Message)]
    struct File {
        #[prost(string, tag = "2")]
        package: String,
        #[prost(message, repeated, tag = "4")]
        message_type: Vec<Msg>,
        #[prost(message, repeated, tag = "6")]
        service: Vec<Service>,
    }
    #[derive(Clone, PartialEq, Message)]
    struct Msg {
        #[prost(string, tag = "1")]
        name: String,
        #[prost(message, repeated, tag = "2")]
        field: Vec<Field>,
        #[prost(message, repeated, tag = "3")]
        nested_type: Vec<Msg>,
    }
    #[derive(Clone, PartialEq, Message)]
    struct Field {
        #[prost(string, tag = "1")]
        name: String,
        #[prost(int32, tag = "4")]
        label: i32,
        #[prost(int32, tag = "5")]
        r#type: i32,
        #[prost(string, tag = "6")]
        type_name: String,
    }
    #[derive(Clone, PartialEq, Message)]
    struct Service {
        #[prost(string, tag = "1")]
        name: String,
        #[prost(message, repeated, tag = "2")]
        method: Vec<Method>,
    }
    #[derive(Clone, PartialEq, Message)]
    struct Method {
        #[prost(string, tag = "1")]
        name: String,
        #[prost(string, tag = "2")]
        input_type: String,
        #[prost(message, optional, tag = "4")]
        options: Option<MethodOptions>,
        #[prost(bool, tag = "5")]
        client_streaming: bool,
        #[prost(bool, tag = "6")]
        server_streaming: bool,
    }
    #[derive(Clone, PartialEq, Message)]
    struct MethodOptions {
        #[prost(message, optional, tag = "72295728")]
        http: Option<HttpRule>,
    }
    #[derive(Clone, PartialEq, Message)]
    struct HttpRule {
        #[prost(string, tag = "2")]
        get: String,
        #[prost(string, tag = "3")]
        put: String,
        #[prost(string, tag = "4")]
        post: String,
        #[prost(string, tag = "5")]
        delete: String,
        #[prost(string, tag = "6")]
        patch: String,
        #[prost(string, tag = "7")]
        body: String,
        #[prost(message, repeated, tag = "11")]
        additional_bindings: Vec<HttpRule>,
    }

    const PACKAGE: &str = "delonix.node.v1";
    const REPEATED: i32 = 3;

    /// `GetApiRoot` -> `get_api_root`, the name tonic gives the trait method.
    fn snake(name: &str) -> String {
        let chars: Vec<char> = name.chars().collect();
        let mut out = String::new();
        for (i, c) in chars.iter().enumerate() {
            if c.is_uppercase() && i > 0 {
                let prev = chars[i - 1];
                let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
                if prev.is_lowercase()
                    || prev.is_ascii_digit()
                    || (prev.is_uppercase() && next_lower)
                {
                    out.push('_');
                }
            }
            out.extend(c.to_lowercase());
        }
        out
    }

    /// The fields of `message` a query string may name, flattened to dotted
    /// paths (`page.page_size`), each with the JSON kind the binder converts
    /// the text to.
    fn bindable(
        messages: &BTreeMap<String, Msg>,
        message: &str,
        prefix: &str,
        depth: u8,
        out: &mut Vec<(String, &'static str, bool)>,
    ) {
        let Some(msg) = messages.get(message) else {
            return;
        };
        for f in &msg.field {
            let path = format!("{prefix}{}", f.name);
            let repeated = f.label == REPEATED;
            let kind = match f.r#type {
                8 => "Bool",
                9 | 12 | 14 => "Text",
                11 => {
                    // A singular message of this package is addressed through
                    // its own fields; anything else (a repeated message, a
                    // map, a well-known type) is not a query parameter.
                    let inner = f.type_name.trim_start_matches('.');
                    if !repeated && depth < 3 && messages.contains_key(inner) {
                        bindable(messages, inner, &format!("{path}."), depth + 1, out);
                    }
                    continue;
                }
                _ => "Number",
            };
            out.push((path, kind, repeated));
        }
    }

    pub fn generate(descriptors: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
        let set = FileSet::decode(descriptors)?;
        let mut messages = BTreeMap::new();
        for file in set.file.iter().filter(|f| f.package == PACKAGE) {
            for m in &file.message_type {
                messages.insert(format!("{PACKAGE}.{}", m.name), m.clone());
            }
        }
        let mut table = String::new();
        let mut dispatchers = String::new();
        let mut count = 0usize;
        for file in set.file.iter().filter(|f| f.package == PACKAGE) {
            for svc in &file.service {
                let module = snake(&svc.name);
                let mut arms = String::new();
                for m in &svc.method {
                    let Some(rule) = m.options.as_ref().and_then(|o| o.http.as_ref()) else {
                        continue;
                    };
                    if !rule.additional_bindings.is_empty() {
                        return Err(format!(
                            "{}.{}: additional_bindings are not transcoded",
                            svc.name, m.name
                        )
                        .into());
                    }
                    let (verb, template) = [
                        ("GET", &rule.get),
                        ("PUT", &rule.put),
                        ("POST", &rule.post),
                        ("DELETE", &rule.delete),
                        ("PATCH", &rule.patch),
                    ]
                    .into_iter()
                    .find(|(_, t)| !t.is_empty())
                    .ok_or_else(|| format!("{}.{}: http rule without a path", svc.name, m.name))?;
                    let input = m.input_type.trim_start_matches('.');
                    let ty = input
                        .strip_prefix(&format!("{PACKAGE}."))
                        .filter(|t| !t.contains('.'))
                        .ok_or_else(|| {
                            format!("{}.{}: request type {input} is not a top-level message of {PACKAGE}", svc.name, m.name)
                        })?;
                    let mut fields = Vec::new();
                    bindable(&messages, input, "", 0, &mut fields);
                    let fields: String = fields
                        .iter()
                        .map(|(p, k, r)| format!("({p:?}, Kind::{k}, {r}), "))
                        .collect();
                    writeln!(
                        table,
                        "    Route {{ service: {:?}, rpc: {:?}, method: {verb:?}, template: {template:?}, body: {:?}, server_streaming: {}, fields: &[{fields}] }},",
                        svc.name, m.name, rule.body, m.server_streaming
                    )?;
                    count += 1;
                    if m.server_streaming || m.client_streaming {
                        writeln!(
                            arms,
                            "        {:?} => Err(crate::transcode::streams_over_grpc_only({:?}, {:?})),",
                            m.name, svc.name, m.name
                        )?;
                    } else {
                        writeln!(
                            arms,
                            "        {:?} => crate::transcode::unary::<crate::proto::v1::{ty}, _, _, _>(input, |r| svc.{}(r)).await,",
                            m.name,
                            snake(&m.name)
                        )?;
                    }
                }
                writeln!(
                    dispatchers,
                    "/// One REST call of `{svc}`: the request built by the binder, the\n/// service method the gRPC encoding calls, the answer as its proto3 JSON.\n#[allow(dead_code)]\npub async fn dispatch_{module}<T: crate::proto::v1::{module}_server::{svc}>(\n    svc: &T,\n    rpc: &str,\n    input: serde_json::Value,\n) -> Result<serde_json::Value, tonic::Status> {{\n    match rpc {{\n{arms}        other => Err(tonic::Status::internal(format!(\"{svc} has no REST mapping for {{other}}\"))),\n    }}\n}}\n",
                    svc = svc.name
                )?;
            }
        }
        if count == 0 {
            return Err("no google.api.http rule found in the contract".into());
        }
        Ok(format!(
            "// Generated by build.rs from proto/delonix/node/v1 — do not edit.\n\n/// Every RPC of the contract that has a `google.api.http` rule.\npub static ROUTES: &[Route] = &[\n{table}];\n\n{dispatchers}"
        ))
    }
}
