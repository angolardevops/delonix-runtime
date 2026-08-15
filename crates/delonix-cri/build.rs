// Gera os stubs gRPC do CRI v1 (Kubernetes) a partir de `proto/api.proto`
// (já sem anotações gogoproto) via tonic-build/prost. C2.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_client(true) // servidor + cliente (o cliente serve os testes: ver `tests/grpc_status.rs`)
        .compile_protos(&["proto/api.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/api.proto");
    Ok(())
}
