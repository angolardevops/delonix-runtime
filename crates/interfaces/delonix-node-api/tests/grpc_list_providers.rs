//! `ListProviders` through the real transports: gRPC over the unix socket with
//! the generated client, and HTTP/JSON through the same router. The server is
//! the one `delonix serve node-api` runs, not a test double.

use delonix_node_api::proto::v1::node_service_client::NodeServiceClient;
use delonix_node_api::proto::v1::{
    ConditionStatus, GetCapacityRequest, GetHealthRequest, GetNodeInfoRequest,
    ListProvidersRequest, WatchEventsRequest,
};

/// A SHORT socket path (`sun_path` is 108 bytes): a `TempDir` in `/tmp`
/// itself, removed on every exit, a failed assert included.
fn short_sock() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir_in("/tmp").expect("a directory under /tmp");
    let sock = dir.path().join("x.sock").to_str().unwrap().to_owned();
    (dir, sock)
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
    let (_sock_dir, sock) = short_sock();
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

    // ADR-0042 step C: what this node is, its health and its room.
    let info = cli
        .get_node_info(GetNodeInfoRequest::default())
        .await
        .expect("GetNodeInfo")
        .into_inner();
    assert_eq!(info.api_version, "delonix.node.v1");
    assert_eq!(info.engine_version, env!("CARGO_PKG_VERSION"));
    assert!(!info.engine_commit.is_empty() && !info.arch.is_empty());
    assert_eq!(info.cgroup_driver, "cgroupfs");

    let health = cli
        .get_health(GetHealthRequest::default())
        .await
        .expect("GetHealth")
        .into_inner();
    let types: Vec<&str> = health
        .conditions
        .iter()
        .map(|c| c.r#type.as_str())
        .collect();
    assert_eq!(
        types,
        [
            "NetworkReady",
            "StoreWritable",
            "CgroupDelegated",
            "ProvidersAvailable"
        ]
    );
    assert_ne!(health.overall, ConditionStatus::Unspecified as i32);
    assert!(health.conditions.iter().all(|c| !c.reason.is_empty()));

    let cap = cli
        .get_capacity(GetCapacityRequest::default())
        .await
        .expect("GetCapacity")
        .into_inner();
    // Measured or named as unmeasured — never a silent zero.
    for (field, value) in [
        ("cpu_millis_total", cap.cpu_millis_total),
        ("cpu_millis_allocatable", cap.cpu_millis_allocatable),
        ("memory_bytes_total", cap.memory_bytes_total),
        ("memory_bytes_allocatable", cap.memory_bytes_allocatable),
        ("pids_allocatable", cap.pids_allocatable),
    ] {
        assert!(
            value > 0 || cap.unmeasured.iter().any(|u| u == field),
            "{field} is 0 and not in unmeasured: {cap:?}"
        );
    }
    assert!(cap.cpu_millis_allocatable <= cap.cpu_millis_total);
    assert!(cap.memory_bytes_allocatable <= cap.memory_bytes_total);

    // What is not served says so — and says with which step it arrives.
    let watch = cli
        .watch_events(WatchEventsRequest::default())
        .await
        .expect_err("WatchEvents is not served yet");
    assert_eq!(watch.code(), tonic::Code::Unimplemented);
    assert!(
        watch.message().contains("ADR-0040 P5"),
        "{}",
        watch.message()
    );
}

/// The JSON routes of `GetNodeInfo`/`GetHealth`/`GetCapacity` answer with the
/// same messages, proto field names included, and `/openapi.json` serves the
/// generated document with a handler behind each `NodeService` GET it lists.
#[tokio::test]
async fn node_service_answers_as_json_and_publishes_its_openapi() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let app = delonix_node_api::router();
    let get = |path: &'static str| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(
                    axum::http::Request::get(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status();
            let body = res.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        }
    };
    let (s, v) = get("/v1/node").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["api_version"], "delonix.node.v1");
    assert!(v.get("supported_workload_types").is_some(), "{v}");
    let (s, v) = get("/v1/node/health").await;
    assert_eq!(s, 200, "{v}");
    assert!(v["overall"].is_string(), "enum as its name: {v}");
    assert_eq!(v["conditions"].as_array().map(Vec::len), Some(4), "{v}");
    let (s, v) = get("/v1/node/capacity").await;
    assert_eq!(s, 200, "{v}");
    assert!(v.get("unmeasured").is_some(), "{v}");

    let (s, doc) = get("/openapi.json").await;
    assert_eq!(s, 200);
    assert!(
        doc["openapi"].as_str().unwrap_or_default().starts_with('3'),
        "{doc}"
    );
    let paths = doc["paths"].as_object().expect("paths");
    for path in [
        "/v1/node",
        "/v1/node/health",
        "/v1/node/capacity",
        "/v1/providers",
    ] {
        assert!(paths.contains_key(path), "{path} missing from the OpenAPI");
        let (s, _) = get(path).await;
        assert_eq!(s, 200, "{path} is in the OpenAPI and has no handler");
    }
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
