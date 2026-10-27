//! The Ceph `StorageService` served from the node API's own router — the same
//! router the unix socket serves — against the live MicroCeph (ADR-0081).
//!
//! Each is `#[ignore]`: it runs only with `--ignored`, never in the default
//! suite, because it needs a cluster. The process must be able to read
//! `/etc/ceph/ceph.client.admin.keyring` (the session makes it readable).
//!
//!   cargo test -p delonix-node-api --test storage_live -- --ignored --nocapture

use http_body_util::BodyExt;
use tower::ServiceExt;

async fn get(path: &str) -> (u16, serde_json::Value) {
    let app = delonix_node_api::router();
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json =
        serde_json::from_slice::<serde_json::Value>(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
#[ignore = "needs a live Ceph cluster and a readable admin keyring: run with --ignored"]
async fn rados_status_is_served_over_rest() {
    let (status, v) = get("/v1/storage/ceph/status").await;
    println!("[REST] GET /v1/storage/ceph/status -> {status} {v}");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["reachable"], true, "the cluster is reachable: {v}");
    assert!(
        v["health"]
            .as_str()
            .unwrap_or_default()
            .starts_with("HEALTH_"),
        "health is a Ceph status word: {v}"
    );
    assert!(v["osds_up"].as_u64().unwrap_or(0) >= 1, "{v}");
}

#[tokio::test]
#[ignore = "needs a live Ceph cluster with RGW and the 'delonix-bucket' bucket: run with --ignored"]
async fn rgw_buckets_are_served_over_rest() {
    let (status, v) = get("/v1/storage/objectstore/buckets").await;
    println!("[REST] GET /v1/storage/objectstore/buckets -> {status} {v}");
    assert_eq!(status, 200, "{v}");
    let buckets = v["buckets"].as_array().cloned().unwrap_or_default();
    assert!(
        buckets.iter().any(|b| b.as_str() == Some("delonix-bucket")),
        "the S3 bucket is listed: {v}"
    );

    let (status, v) = get("/v1/storage/objectstore/buckets/delonix-bucket").await;
    println!("[REST] GET .../buckets/delonix-bucket -> {status} {v}");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["name"], "delonix-bucket", "{v}");
}
