//! `ListProviders` through the real transports: gRPC over the unix socket with
//! the generated client, and HTTP/JSON through the same router. The server is
//! the one `delonix serve node-api` runs, not a test double.

use delonix_node_api::proto::v1::node_service_client::NodeServiceClient;
use delonix_node_api::proto::v1::{
    ConditionStatus, GetApiRootRequest, GetCapacityRequest, GetHealthRequest, GetNodeInfoRequest,
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
    let mut nets = delonix_node_api::proto::v1::network_service_client::NetworkServiceClient::new(
        channel.clone(),
    );
    let mut cli = NodeServiceClient::new(channel.clone());

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

    // ADR-0042 step E, first wave: the network reads are served over gRPC too.
    // A namespace other than `default` has no networks whatever this host has,
    // and a mutation of the same service still says it is not served.
    let none = nets
        .list_networks(delonix_node_api::proto::v1::ListNetworksRequest {
            namespace: "no-such-namespace".into(),
            ..Default::default()
        })
        .await
        .expect("ListNetworks")
        .into_inner();
    assert!(none.networks.is_empty());
    assert!(none.links.iter().any(|l| l.rel == "self"));
    let missing = nets
        .get_network(delonix_node_api::proto::v1::GetNetworkRequest {
            name: "x".into(),
            namespace: "no-such-namespace".into(),
        })
        .await
        .expect_err("no such network");
    assert_eq!(missing.code(), tonic::Code::NotFound);
    // The mutations are served (their work is covered where the state root is
    // the test's own); over the wire, here, only what they refuse before
    // touching anything: this test runs against the process's state root.
    let create = nets
        .create_network(delonix_node_api::proto::v1::CreateNetworkRequest {
            namespace: "no-such-namespace".into(),
            name: "x".into(),
            ..Default::default()
        })
        .await
        .expect_err("a network is created in default");
    assert_eq!(create.code(), tonic::Code::InvalidArgument);
    let delete = nets
        .delete_network(delonix_node_api::proto::v1::DeleteNetworkRequest {
            namespace: "no-such-namespace".into(),
            name: "x".into(),
            ..Default::default()
        })
        .await
        .expect_err("a network is removed in default");
    assert_eq!(delete.code(), tonic::Code::InvalidArgument);
    let connect = nets
        .connect_container(delonix_node_api::proto::v1::ConnectContainerRequest::default())
        .await
        .expect_err("not served yet");
    assert_eq!(connect.code(), tonic::Code::Unimplemented);

    // The operation record, over gRPC: an id nothing was ever given is NOT_FOUND.
    let mut ops =
        delonix_node_api::proto::v1::operation_service_client::OperationServiceClient::new(
            channel.clone(),
        );
    let missing = ops
        .get_operation(delonix_node_api::proto::v1::GetOperationRequest {
            id: "op-no-such-operation".into(),
        })
        .await
        .expect_err("no such operation");
    assert_eq!(missing.code(), tonic::Code::NotFound);
    let cancel = ops
        .cancel_operation(delonix_node_api::proto::v1::CancelOperationRequest {
            id: "op-no-such-operation".into(),
        })
        .await
        .expect_err("not served yet");
    assert_eq!(cancel.code(), tonic::Code::Unimplemented);

    // ADR-0042 D2: the entry point, on the gRPC encoding too.
    let root = cli
        .get_api_root(GetApiRootRequest::default())
        .await
        .expect("GetApiRoot")
        .into_inner();
    assert_eq!(root.api_version, "delonix.node.v1");
    assert!(root
        .links
        .iter()
        .any(|l| l.rel == "self" && l.href == "/v1"));

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
    assert_eq!(
        bad.headers()["content-type"],
        "application/problem+json",
        "an error is an RFC 9457 problem document"
    );
    let body = bad.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["dx"], "DX-1000", "{v}");
    assert_eq!(v["code"], "DX_INVALID_ARGUMENT", "{v}");
    assert_eq!(
        v["grpc_status"], 3,
        "INVALID_ARGUMENT on the gRPC encoding: {v}"
    );
    assert_eq!(v["instance"], "/v1/providers", "{v}");
}

/// ADR-0042 D2/D4: every REST error is a problem document that validates
/// against the `Problem` schema the server publishes at `/openapi.json` —
/// required fields present, no field the schema does not declare, each of the
/// declared type.
#[tokio::test]
async fn rest_errors_validate_against_the_published_problem_schema() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let app = delonix_node_api::router();
    let call = |method: &'static str, path: &'static str| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status().as_u16();
            let ctype = res.headers()["content-type"].to_str().unwrap().to_string();
            let body = res.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                ctype,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        }
    };
    let schema = &delonix_node_api::openapi_document()["components"]["schemas"]["Problem"];
    let props = schema["properties"]
        .as_object()
        .expect("Problem.properties");
    for (method, path, want_status, want_dx, want_grpc) in [
        ("GET", "/v1/providers?kind=ceph", 400, "DX-1000", 3),
        ("GET", "/v1/nada", 404, "DX-4001", 5),
        ("DELETE", "/v1/node", 405, "DX-4001", 5),
    ] {
        let (status, ctype, v) = call(method, path).await;
        assert_eq!(status, want_status, "{method} {path}: {v}");
        assert_eq!(ctype, "application/problem+json", "{method} {path}");
        assert_eq!(v["dx"], want_dx, "{v}");
        assert_eq!(v["status"], want_status, "status field = HTTP status: {v}");
        assert_eq!(v["grpc_status"], want_grpc, "{v}");
        for req in schema["required"].as_array().unwrap() {
            assert!(v.get(req.as_str().unwrap()).is_some(), "{req} missing: {v}");
        }
        for (k, val) in v.as_object().unwrap() {
            let decl = props
                .get(k)
                .unwrap_or_else(|| panic!("{k} is not declared by Problem: {v}"));
            match decl["type"].as_str() {
                Some("string") => assert!(val.is_string(), "{k} should be a string: {v}"),
                Some("integer") => assert!(val.is_i64(), "{k} should be an integer: {v}"),
                other => panic!("{k}: unexpected schema type {other:?}"),
            }
        }
        assert!(
            v["type"].as_str().unwrap().contains("codigos.html#DX-"),
            "type points at the dictionary entry: {v}"
        );
    }
}

/// Every REST route of the socket answers GET only, so the 405 fallback's
/// `Allow: GET` is true. A route that gains another method has to change both.
#[tokio::test]
async fn the_rest_routes_are_get_only() {
    use tower::ServiceExt;
    let app = delonix_node_api::router();
    for path in [
        "/v1",
        "/v1/providers",
        "/v1/node",
        "/v1/node/health",
        "/v1/node/capacity",
        "/openapi.json",
        "/docs",
        "/redoc",
    ] {
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::post(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 405, "POST {path}");
        assert_eq!(res.headers()["allow"], "GET", "POST {path}");
    }
}

/// ADR-0042 D2, Richardson level 3: a client navigates from `GET /v1` without
/// building a URI. Every link the root offers answers 200 (nothing it cannot
/// serve is offered), every resource links to itself under its own path, and
/// the RFC 8288 `Link` header says what the body's `links` say.
#[tokio::test]
async fn a_client_navigates_from_the_root_by_its_links() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let app = delonix_node_api::router();
    let get = |path: String| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(
                    axum::http::Request::get(path.as_str())
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status().as_u16();
            let link = res
                .headers()
                .get("link")
                .map(|v| v.to_str().unwrap().to_string());
            let ctype = res.headers()["content-type"].to_str().unwrap().to_string();
            let body = res.into_body().collect().await.unwrap().to_bytes();
            let json = if ctype.starts_with("application/json") {
                serde_json::from_slice::<serde_json::Value>(&body).ok()
            } else {
                None
            };
            (status, link, json)
        }
    };
    let (status, header, root) = get("/v1".into()).await;
    assert_eq!(status, 200);
    let root = root.expect("JSON");
    assert_eq!(root["api_version"], "delonix.node.v1");
    let links = root["links"].as_array().expect("links");
    assert_eq!(
        header.as_deref(),
        delonix_node_api::link_header(&root).as_deref(),
        "the Link header mirrors the body"
    );
    for l in links {
        let href = l["href"].as_str().unwrap();
        assert_eq!(l["method"], "GET", "{l}");
        let (status, header, body) = get(href.to_string()).await;
        assert_eq!(
            status, 200,
            "the root offers {href} and it does not answer 200"
        );
        // A JSON resource links to itself under its own path, and mirrors its
        // links in the header.
        if let Some(body) = body {
            if href == "/openapi.json" {
                continue;
            }
            let own = body["links"]
                .as_array()
                .unwrap_or_else(|| panic!("{href} has no links: {body}"));
            assert!(
                own.iter().any(|x| x["rel"] == "self" && x["href"] == href),
                "{href} does not link to itself: {own:?}"
            );
            assert_eq!(
                header.as_deref(),
                delonix_node_api::link_header(&body).as_deref()
            );
        }
    }
    // A filtered list's self link keeps the filter.
    let (_, _, body) = get("/v1/providers?kind=network".into()).await;
    let body = body.unwrap();
    assert!(body["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["rel"] == "self" && x["href"] == "/v1/providers?kind=network"));
}

/// ADR-0042 step C: the REST routes are generated from the `google.api.http`
/// annotations, and the published OpenAPI is generated from the same ones — so
/// the two list the same operations, method for method and path for path.
#[test]
fn the_generated_routes_are_the_published_openapi_operations() {
    use delonix_node_api::transcode::ROUTES;
    let paths = delonix_node_api::openapi_document()["paths"]
        .as_object()
        .expect("paths");
    let mut published = std::collections::BTreeSet::new();
    for (path, item) in paths {
        for (method, op) in item.as_object().unwrap() {
            published.insert((
                method.to_uppercase(),
                path.clone(),
                op["operationId"].as_str().unwrap_or_default().to_string(),
            ));
        }
    }
    let generated: std::collections::BTreeSet<_> = ROUTES
        .iter()
        .map(|r| {
            (
                r.method.to_string(),
                r.template.to_string(),
                format!("{}_{}", r.service, r.rpc),
            )
        })
        .collect();
    assert_eq!(generated.len(), ROUTES.len(), "no route is listed twice");
    assert_eq!(generated, published);
}

/// A route of the contract this engine does not serve yet is 501 with DX-6001
/// — "in the contract, not here yet" — and never the 404 of a path the
/// contract does not have. Custom verbs (`{name}:start`) resolve, a contract
/// path on another method is 405 with the methods the contract maps, and a
/// query parameter the request does not have is refused.
#[tokio::test]
async fn contract_routes_resolve_and_say_what_is_not_served() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let app = delonix_node_api::router();
    let call = |method: &'static str, path: &'static str, body: &'static str| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(path)
                        .body(axum::body::Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status().as_u16();
            let allow = res
                .headers()
                .get("allow")
                .map(|v| v.to_str().unwrap().to_string());
            let body = res.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                allow,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        }
    };
    for (method, path) in [
        ("GET", "/v1/namespaces/default/containers"),
        ("POST", "/v1/namespaces/default/containers/web:start"),
        ("POST", "/v1/namespaces/default/networks/lab:connect"),
        ("POST", "/v1/operations/op-1:cancel"),
        ("DELETE", "/v1/namespaces/default/volumes/data"),
        ("POST", "/v1/images:pull"),
        ("GET", "/v1/events:watch"),
    ] {
        let (status, _, v) = call(method, path, "").await;
        assert_eq!(status, 501, "{method} {path}: {v}");
        assert_eq!(v["dx"], "DX-6001", "{v}");
        assert_eq!(v["grpc_status"], 12, "{v}");
        assert_eq!(v["instance"], path, "{v}");
    }
    // The network mutations are served. This router answers from the
    // process's state root, so only what they refuse before touching anything
    // is driven here: a network lives in `default`.
    for (method, path, body) in [
        ("POST", "/v1/namespaces/outro/networks", r#"{"name":"lab"}"#),
        ("DELETE", "/v1/namespaces/outro/networks/lab", ""),
    ] {
        let (status, _, v) = call(method, path, body).await;
        assert_eq!(status, 400, "{method} {path}: {v}");
        assert_eq!(v["grpc_status"], 3, "{v}");
    }
    // The volume reads are served: a namespace nothing was ever stored in has
    // no volumes and no volume of any name, whatever this host has.
    let (status, _, v) = call("GET", "/v1/namespaces/no-such-namespace/volumes", "").await;
    assert_eq!(status, 200, "{v}");
    assert!(v["volumes"].as_array().is_none_or(|a| a.is_empty()), "{v}");
    assert!(v["links"][0]["href"]
        .as_str()
        .unwrap()
        .starts_with("/v1/namespaces/no-such-namespace/volumes"));
    let (status, _, v) = call("GET", "/v1/namespaces/no-such-namespace/volumes/x", "").await;
    assert_eq!((status, v["dx"].as_str()), (404, Some("DX-4000")), "{v}");
    // An operation nobody was given: the RESOURCE is missing (DX-4000), which
    // is not the missing ROUTE (DX-4001).
    let (status, _, v) = call("GET", "/v1/operations/op-no-such-operation", "").await;
    assert_eq!((status, v["dx"].as_str()), (404, Some("DX-4000")), "{v}");
    let (status, allow, v) = call("GET", "/v1/namespaces/default/containers/web:start", "").await;
    assert_eq!((status, allow.as_deref()), (405, Some("POST")), "{v}");
    let (status, allow, _) = call("PUT", "/v1/namespaces/default/containers/web", "").await;
    assert_eq!(status, 405);
    assert_eq!(allow.as_deref(), Some("DELETE, GET, PATCH"));
    let (status, _, v) = call("GET", "/v1/providers?kindd=network", "").await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["detail"]
            .as_str()
            .unwrap()
            .contains("unknown query parameter 'kindd'"),
        "{v}"
    );
    let (status, _, v) = call("GET", "/v1/node", "{}").await;
    assert_eq!(status, 400, "a GET route takes no body: {v}");
    let (status, _, v) = call("GET", "/v1/namespaces/default/nothing", "").await;
    assert_eq!((status, v["dx"].as_str()), (404, Some("DX-4001")), "{v}");
}
