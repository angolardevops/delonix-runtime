//! Generates the `delonix.node.v1` stubs (prost/tonic, server AND client — the
//! client is what the conformance test drives) and the proto3 JSON encoding of
//! every message (pbjson, proto field names — the same naming the published
//! OpenAPI uses, `naming=proto` in `scripts/contract_gate.py`).
//!
//! The sources are the repository's `proto/` and the vendored googleapis, so the
//! contract gate and this crate read the same files.
use std::path::PathBuf;

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
            &[proto.join("delonix/node/v1/node.proto")],
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
    for f in ["delonix/node/v1/node.proto", "delonix/node/v1/common.proto"] {
        println!("cargo:rerun-if-changed={}", proto.join(f).display());
    }
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
