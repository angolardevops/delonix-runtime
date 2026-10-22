//! The HTTP surface, driven against a real server: the note lifecycle, the one
//! error shape, readiness, and the inbound webhook's verify-and-deduplicate.

mod common;

use common::{spawn, state, test_key};
use __MODULE__::httpapi::router;
use __MODULE__::webhook;
use serde_json::Value;

#[tokio::test]
async fn note_lifecycle() {
    let base = spawn(router(state(false, None))).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/v1/notes"))
        .json(&serde_json::json!({ "title": "first", "body": "hello" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    assert!(resp.headers().get("location").is_some());
    let created: Value = resp.json().await.unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(!id.is_empty());

    let got: Value = client
        .get(format!("{base}/api/v1/notes/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["title"], "first");

    let page: Value = client
        .get(format!("{base}/api/v1/notes?limit=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["total"], 1);
}

#[tokio::test]
async fn errors_share_one_shape() {
    let base = spawn(router(state(false, None))).await;
    let client = reqwest::Client::new();
    let big_title = "x".repeat(2000);
    let cases: Vec<(&str, String, String, u16, &str)> = vec![
        ("POST", "/api/v1/notes".into(), r#"{"title":""}"#.into(), 422, "validation_failed"),
        ("POST", "/api/v1/notes".into(), r#"{"title":"x","extra":1}"#.into(), 400, "malformed_json"),
        ("POST", "/api/v1/notes".into(), format!(r#"{{"title":"{big_title}"}}"#), 413, "body_too_large"),
        ("GET", "/api/v1/notes?limit=500".into(), String::new(), 422, "validation_failed"),
        ("GET", "/api/v1/notes?offset=abc".into(), String::new(), 422, "validation_failed"),
        ("GET", "/api/v1/notes/missing".into(), String::new(), 404, "not_found"),
        ("GET", "/nowhere".into(), String::new(), 404, "not_found"),
    ];
    for (method, path, body, status, code) in cases {
        let req = if method == "POST" {
            client
                .post(format!("{base}{path}"))
                .header("content-type", "application/json")
                .body(body)
        } else {
            client.get(format!("{base}{path}"))
        };
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), status, "{method} {path}");
        let b: Value = resp.json().await.unwrap();
        assert_eq!(b["error"]["code"], code, "{method} {path}: {b}");
        assert!(b["error"]["request_id"].as_str().is_some(), "{method} {path}");
    }
}

#[tokio::test]
async fn readiness_follows_the_lifecycle() {
    let st = state(false, None);
    let ready = st.readiness.clone();
    let base = spawn(router(st)).await;
    let client = reqwest::Client::new();

    assert_eq!(
        client.get(format!("{base}/api/v1/health/ready")).send().await.unwrap().status(),
        200
    );
    ready.set_draining();
    let resp = client.get(format!("{base}/api/v1/health/ready")).send().await.unwrap();
    assert_eq!(resp.status(), 503);
    let b: Value = resp.json().await.unwrap();
    assert_eq!(b["status"], "draining");
    // Liveness must not follow readiness.
    assert_eq!(
        client.get(format!("{base}/api/v1/health/live")).send().await.unwrap().status(),
        200
    );
}

#[tokio::test]
async fn inbound_webhook_verifies_and_deduplicates() {
    let base = spawn(router(state(true, None))).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/api/v1/webhooks/inbound");
    let body = r#"{"type":"note.create","data":{"title":"from a webhook"}}"#;
    let now = webhook::now_unix();
    let key = test_key();

    let signed = |id: &str, body: &str, ts: i64, key: &[u8]| {
        vec![
            (webhook::HEADER_ID.to_string(), id.to_string()),
            (webhook::HEADER_TIMESTAMP.to_string(), ts.to_string()),
            (webhook::HEADER_SIGNATURE.to_string(), webhook::sign(key, id, ts, body.as_bytes())),
        ]
    };
    let post = |headers: Vec<(String, String)>, body: String| {
        let mut req = client.post(&url).header("content-type", "application/json");
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req.body(body)
    };

    // Unsigned.
    assert_eq!(post(vec![], body.into()).send().await.unwrap().status(), 400);
    // Bad key.
    assert_eq!(
        post(signed("m1", body, now, b"wrong-key-wrong-key-wrong-key"), body.into())
            .send().await.unwrap().status(),
        401
    );
    // Stale timestamp (replay).
    assert_eq!(
        post(signed("m1", body, now - 600, &key), body.into()).send().await.unwrap().status(),
        401
    );
    // Tampered body.
    assert_eq!(
        post(signed("m1", body, now, &key), body.replace("from", "FROM"))
            .send().await.unwrap().status(),
        401
    );
    // Two identical deliveries: both 204, processed once.
    for _ in 0..2 {
        assert_eq!(
            post(signed("m1", body, now, &key), body.into()).send().await.unwrap().status(),
            204
        );
    }
    let page: Value = client
        .get(format!("{base}/api/v1/notes"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(page["total"], 1, "a duplicate delivery was processed twice");
}
