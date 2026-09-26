//! `ListProviders` through the real transports: gRPC over the unix socket with
//! the generated client, and HTTP/JSON through the same router. The server is
//! the one `delonix serve node-api` runs, not a test double.

use delonix_node_api::proto::v1::node_service_client::NodeServiceClient;
use delonix_node_api::proto::v1::{GetNodeInfoRequest, ListProvidersRequest};

fn short_sock() -> String {
    format!("/tmp/dlx-node-t{}.sock", std::process::id())
}

async fn wait_for(path: &str) {
    for _ in 0..200 {
        if std::path::Path::new(path).exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the server never created {path}");
}

#[tokio::test]
async fn list_providers_answers_over_grpc_on_the_unix_socket() {
    let sock = short_sock();
    let _ = std::fs::remove_file(&sock);
    let s = sock.clone();
    std::thread::spawn(move || {
        let _ = delonix_node_api::serve_blocking(&format!("unix://{s}"));
    });
    wait_for(&sock).await;

    let s2 = sock.clone();
    let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")
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
        .expect("connect to the server's unix socket");
    let mut cli = NodeServiceClient::new(channel);

    let all = cli
        .list_providers(ListProvidersRequest::default())
        .await
        .expect("ListProviders over gRPC")
        .into_inner();
    let ids: Vec<(String, String)> = all
        .providers
        .iter()
        .map(|p| (p.kind.clone(), p.id.clone()))
        .collect();
    assert!(
        ids.contains(&("compute".into(), "libvirt".into())),
        "{ids:?}"
    );
    assert!(ids.contains(&("network".into(), "linux".into())), "{ids:?}");
    assert!(ids.contains(&("storage".into(), "linux".into())), "{ids:?}");
    let p = &all.providers[0];
    assert!(!p.catalog_version.is_empty());
    assert!(p.capabilities.iter().all(|c| !c.state.is_empty()));
    assert!(p.health.is_some());

    let net = cli
        .list_providers(ListProvidersRequest {
            kind: "network".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!net.providers.is_empty());
    assert!(net.providers.iter().all(|p| p.kind == "network"));

    let bad = cli
        .list_providers(ListProvidersRequest {
            kind: "ceph".into(),
        })
        .await
        .expect_err("an unknown kind is refused, not an empty list");
    assert_eq!(bad.code(), tonic::Code::InvalidArgument);

    // What is not served says so — and says with which step it arrives.
    let info = cli
        .get_node_info(GetNodeInfoRequest::default())
        .await
        .expect_err("GetNodeInfo is not served yet");
    assert_eq!(info.code(), tonic::Code::Unimplemented);
    assert!(info.message().contains("ADR-0042"), "{}", info.message());
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn list_providers_answers_as_json_on_the_rest_route() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let app = delonix_node_api::router();
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::get("/v1/providers?kind=storage")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let providers = v["providers"].as_array().expect("providers array");
    assert_eq!(providers.len(), 1, "{v}");
    assert_eq!(providers[0]["kind"], "storage");
    assert_eq!(providers[0]["id"], "linux");
    // Proto field names, as the OpenAPI declares them; the state travels.
    assert!(providers[0].get("catalog_version").is_some(), "{v}");
    assert!(
        providers[0]["capabilities"][0].get("state").is_some(),
        "{v}"
    );

    let bad = app
        .oneshot(
            axum::http::Request::get("/v1/providers?kind=ceph")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let body = bad.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["code"], 3,
        "google.rpc.Status code for INVALID_ARGUMENT: {v}"
    );
}
