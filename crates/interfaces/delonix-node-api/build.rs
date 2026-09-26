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
    Ok(())
}
