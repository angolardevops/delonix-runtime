//! Round-trip gRPC REAL contra o servidor CRI — o gap que o `AGENTS.md` declarava.
//!
//! O que estava escrito lá: «Não validado com um kubelet/`crictl` real (nenhum
//! dos dois existe neste host, e `build_client(false)` no `build.rs` não gera
//! stubs de cliente gRPC): o caminho gRPC está coberto pelo teste do
//! `create_container` real + `cap_flags`/`cap_args`, e a camada tonic são três
//! linhas de `blocking(...)`».
//!
//! «São três linhas» é uma razão para achar que funciona, não uma medição. Este
//! teste mede: sobe o servidor num socket unix, fala com ele pelo cliente
//! gerado, e confirma que o `Status` chega com as condições preenchidas.
//!
//! O custo de gerar o cliente foi medido antes de o ligar: **3,5 s** de build no
//! crate. É o preço de deixar de deduzir a camada de transporte.

use delonix_cri::cri::runtime_service_client::RuntimeServiceClient;
use delonix_cri::cri::{
    ListMetricDescriptorsRequest, ListPodSandboxMetricsRequest, ListPodSandboxRequest,
    PodSandboxConfig, PodSandboxMetadata, PortMapping, Protocol, Protocol as Proto,
    RunPodSandboxRequest, StatusRequest, VersionRequest,
};

/// A SHORT socket path: `sun_path` is 108 bytes and an agent session's
/// `$TMPDIR` is already past 90, so the directory lives in `/tmp` itself
/// (`/tmp/.tmpXXXXXX/x.sock`, 22 bytes). The `TempDir` removes it on every
/// exit, a failed assert included, which a `remove_file` on the last line
/// did not.
fn short_sock() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir_in("/tmp").expect("a directory under /tmp");
    let sock = dir.path().join("x.sock").to_str().unwrap().to_owned();
    (dir, sock)
}

#[tokio::test]
async fn o_status_chega_pelo_transporte_grpc_a_serio() {
    let (_sock_dir, sock) = short_sock();
    let base_dir = tempfile::tempdir().unwrap();
    let base = base_dir.path().to_path_buf();

    // O servidor corre numa thread própria (o `serve_blocking` tem o seu runtime).
    let s = sock.clone();
    let b = base.clone();
    let servidor = std::thread::spawn(move || {
        let _ = delonix_cri::serve_blocking(
            b,
            &format!("unix://{s}"),
            delonix_cri::CapCeiling::unlimited(),
        );
    });

    // Esperar por CONDIÇÃO, nunca por tempo.
    for _ in 0..100 {
        if std::path::Path::new(&sock).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        std::path::Path::new(&sock).exists(),
        "o servidor não chegou a criar o socket"
    );

    // Um socket que ACEITA não é um servidor que RESPONDE — daí a chamada real.
    let mut cli = connect(&sock).await;

    let v = cli
        .version(VersionRequest::default())
        .await
        .expect("Version pelo transporte gRPC")
        .into_inner();
    assert!(
        !v.runtime_name.is_empty(),
        "o Version tem de nomear o runtime"
    );

    let st = cli
        .status(StatusRequest { verbose: false })
        .await
        .expect("Status pelo transporte gRPC")
        .into_inner()
        .status
        .expect("StatusResponse.status preenchido");

    // As duas condições que o kubelet lê para decidir se o nó serve.
    let tipos: Vec<&str> = st.conditions.iter().map(|c| c.r#type.as_str()).collect();
    assert!(
        tipos.contains(&"RuntimeReady"),
        "faltou RuntimeReady: {tipos:?}"
    );
    assert!(
        tipos.contains(&"NetworkReady"),
        "faltou NetworkReady: {tipos:?}"
    );
    let rr = st
        .conditions
        .iter()
        .find(|c| c.r#type == "RuntimeReady")
        .unwrap();
    assert!(
        rr.status,
        "chegámos aqui pelo gRPC, logo o runtime respondeu — RuntimeReady tem de ser true"
    );

    // `ListMetricDescriptors`/`ListPodSandboxMetrics` — real gRPC round-trip,
    // not just the internal function called directly. Empty base directory
    // here, so no sandboxes exist; what matters is that the descriptor names
    // arrive over the wire and match this runtime's own metric names (the
    // spec-mandated pairing `every_emitted_metric_name_has_a_descriptor`
    // checks internally, now also proven through the transport).
    let descriptors = cli
        .list_metric_descriptors(ListMetricDescriptorsRequest {})
        .await
        .expect("ListMetricDescriptors over the real gRPC transport")
        .into_inner()
        .descriptors;
    assert!(
        !descriptors.is_empty(),
        "o runtime tem de declarar pelo menos um descritor de métrica"
    );
    let names: Vec<&str> = descriptors.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"container_cpu_usage_core_nanoseconds"),
        "faltou o descritor de CPU por container: {names:?}"
    );

    let pod_metrics = cli
        .list_pod_sandbox_metrics(ListPodSandboxMetricsRequest {})
        .await
        .expect("ListPodSandboxMetrics over the real gRPC transport")
        .into_inner()
        .pod_metrics;
    assert!(
        pod_metrics.is_empty(),
        "no sandboxes exist, the list has to come back empty — never fabricated"
    );

    drop(cli);
    // O servidor não tem paragem limpa (é um `serve_blocking`); o processo de
    // teste termina e leva-o. Não se faz `join`, que penduraria.
    drop(servidor);
}

/// The connector the two tests share: a socket that ACCEPTS is not a server
/// that ANSWERS, so every assertion here goes over a real channel.
async fn connect(sock: &str) -> RuntimeServiceClient<tonic::transport::Channel> {
    let s = sock.to_owned();
    let canal = tonic::transport::Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_| {
            let p = s.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(p).await?,
                ))
            }
        }))
        .await
        .expect("connecting to the server's unix socket");
    RuntimeServiceClient::new(canal)
}

/// ADR-0074 D1: a `hostPort` this node cannot publish is refused from
/// `RunPodSandbox`, over the kubelet's own transport — and **nothing is left
/// behind**, which is the half a unit test cannot prove.
///
/// Before this, the spec `8080:80/sctp` was stored, the sandbox was created, and
/// the pod only died at `StartContainer` with `invalid protocol in
/// '8080:80/sctp'`: late, after the sandbox existed, naming the spec instead of
/// the cause, and retried by the kubelet forever. The empty-state assertion at
/// the end is what says the refusal happens BEFORE any creation.
#[tokio::test]
async fn an_sctp_host_port_is_refused_over_grpc_and_leaves_nothing_behind() {
    let (_sock_dir, sock) = short_sock();
    let base_dir = tempfile::tempdir().unwrap();
    let base = base_dir.path().to_path_buf();

    let s = sock.clone();
    let b = base.clone();
    let server = std::thread::spawn(move || {
        let _ = delonix_cri::serve_blocking(
            b,
            &format!("unix://{s}"),
            delonix_cri::CapCeiling::unlimited(),
        );
    });
    for _ in 0..100 {
        if std::path::Path::new(&sock).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        std::path::Path::new(&sock).exists(),
        "the server never created the socket"
    );

    let mut cli = connect(&sock).await;

    let cfg = |proto: Protocol, host_port: i32| PodSandboxConfig {
        metadata: Some(PodSandboxMetadata {
            name: "sip".into(),
            uid: "u1".into(),
            namespace: "default".into(),
            attempt: 0,
        }),
        port_mappings: vec![PortMapping {
            protocol: proto as i32,
            container_port: 5070,
            host_port,
            host_ip: String::new(),
        }],
        ..Default::default()
    };

    let err = cli
        .run_pod_sandbox(RunPodSandboxRequest {
            config: Some(cfg(Proto::Sctp, 5070)),
            runtime_handler: String::new(),
        })
        .await
        .expect_err("an SCTP hostPort cannot be published by this node");
    assert_eq!(
        err.code(),
        tonic::Code::FailedPrecondition,
        "the class is what the kubelet turns into a pod event: {err:?}"
    );
    assert!(err.message().contains("5070"), "{}", err.message());
    assert!(err.message().contains("sctp"), "{}", err.message());

    // The half a unit test cannot reach: the refusal came BEFORE any creation,
    // so the node has no sandbox to clean up.
    let sandboxes = cli
        .list_pod_sandbox(ListPodSandboxRequest { filter: None })
        .await
        .expect("ListPodSandbox over the real transport")
        .into_inner()
        .items;
    assert!(
        sandboxes.is_empty(),
        "a refused sandbox must not exist: {sandboxes:?}"
    );

    drop(cli);
    drop(server);
}
