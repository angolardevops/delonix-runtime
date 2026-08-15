//! Round-trip gRPC REAL contra o servidor CRI — o gap que o `CLAUDE.md` declarava.
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
use delonix_cri::cri::{StatusRequest, VersionRequest};

/// Um caminho de socket CURTO — o `sun_path` do `AF_UNIX` são 108 bytes, e o
/// `$TMPDIR` de uma sessão de agente já passa dos 90.
fn sock_curto() -> String {
    format!("/tmp/dlx-grpc-t{}.sock", std::process::id())
}

#[tokio::test]
async fn o_status_chega_pelo_transporte_grpc_a_serio() {
    let sock = sock_curto();
    let _ = std::fs::remove_file(&sock);
    let base = std::env::temp_dir().join(format!("dlx-grpc-base-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();

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
    let s2 = sock.clone();
    let canal = tonic::transport::Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_| {
            let p = s2.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(p).await?,
                ))
            }
        }))
        .await
        .expect("ligar ao socket unix do servidor");

    let mut cli = RuntimeServiceClient::new(canal);

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

    drop(cli);
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_dir_all(&base);
    // O servidor não tem paragem limpa (é um `serve_blocking`); o processo de
    // teste termina e leva-o. Não se faz `join`, que penduraria.
    drop(servidor);
}
