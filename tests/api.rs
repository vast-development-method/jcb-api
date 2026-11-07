use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use jcb_api::{
    config::Config,
    engine::Engine,
    service::{router, AppState},
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

async fn fixture() -> (TempDir, Engine, Router) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        cache: dir.path().join("cache.json"),
        tokens: dir.path().join("tokens.json"),
        api_socket: dir.path().join("api.sock"),
        admin_socket: dir.path().join("admin.sock"),
        max_state_bytes: 1024 * 1024,
        max_body_bytes: 4096,
        cpu_count: 2,
        memory_mb: 512,
        max_connections: 64,
        log_requests: false,
        log_queue_capacity: 64,
        poll_seconds: 2,
        service_name: "jcb-api.service".into(),
        public_base_url: None,
    };
    let engine = Engine::open(
        config.cache.clone(),
        config.tokens.clone(),
        config.max_state_bytes,
    )
    .unwrap();
    engine
        .token_add("community", "legacy-secret")
        .await
        .unwrap();
    let router = router(AppState::new(engine.clone(), config).unwrap());
    (dir, engine, router)
}
async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", token);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn heartbeat(status: i64) -> Value {
    json!({"heartbeat":{"monitorID":1,"status":status,"msg":"OK"},"monitor":{"name":"Mirror","url":"https://example.org/code"},"msg":"Monitor update"})
}

#[tokio::test]
async fn legacy_webhook_grows_all_route_levels_and_filters() {
    let (_dir, _engine, app) = fixture().await;
    let (status, response) = request(
        &app,
        "POST",
        "/v1/webhook/community/jcb/powers",
        Some("legacy-secret"),
        heartbeat(1),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response, json!({"message":"Status updated to up"}));
    let target = json!({"target":"powers","network":[{"id":1,"name":"Mirror","url":"https://example.org/code","status":1}]});
    let project = json!({"project":"jcb","targets":[target.clone()]});
    let system = json!({"system":"community","projects":[project.clone()]});
    for (path, expected) in [
        ("/v1/network/", json!({"systems":[system.clone()]})),
        ("/v1/network/community/", system),
        ("/v1/network/community/jcb/", project),
        ("/v1/network/community/jcb/powers", target.clone()),
        ("/v1/network/community/jcb/powers/1", target.clone()),
        ("/v1/network/community/jcb/powers/%2B01", target),
    ] {
        let (status, body) = request(&app, "GET", path, None, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(body, expected, "{path}");
    }
    for status in ["0", "99", "9999999999999999999999999999999999"] {
        let (code, body) = request(
            &app,
            "GET",
            &format!("/v1/network/community/jcb/powers/{status}"),
            None,
            Value::Null,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, json!({"target":"powers","network":[]}));
    }
}
#[tokio::test]
async fn auth_is_scoped_and_exact_before_parsing() {
    let (_dir, _, app) = fixture().await;
    for (path, token) in [
        ("/v1/webhook/community/jcb/powers", None),
        ("/v1/webhook/community/jcb/powers", Some("secret")),
        ("/v1/webhook/other/jcb/powers", Some("legacy-secret")),
    ] {
        let (status, body) = request(&app, "POST", path, token, json!(["invalid"])).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"error":"Unauthorized"}));
    }
}
#[tokio::test]
async fn malformed_authorized_requests_are_rejected_without_mutation() {
    let (_dir, engine, app) = fixture().await;
    for payload in [
        json!([1]),
        json!({}),
        json!({"heartbeat":{}}),
        json!({"heartbeat":{"monitorID":[],"status":1}}),
        json!({"heartbeat":{"monitorID":"","status":1}}),
        json!({"heartbeat":{"monitorID":"x".repeat(257),"status":1}}),
        json!({"heartbeat":{"monitorID":1,"status":"1"}}),
        json!({"heartbeat":{"monitorID":1,"status":1},"monitor":{"name":false}}),
    ] {
        let (code, _) = request(
            &app,
            "POST",
            "/v1/webhook/community/jcb/powers",
            Some("legacy-secret"),
            payload,
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }
    assert_eq!(engine.snapshot(), json!({"systems":[]}));
    let (code, _) = request(
        &app,
        "POST",
        "/v1/webhook/community/jcb/powers",
        Some("legacy-secret"),
        json!({"padding":"x".repeat(8192)}),
    )
    .await;
    assert_eq!(code, StatusCode::PAYLOAD_TOO_LARGE);
}
#[tokio::test]
async fn errors_and_slashes_keep_existing_contract() {
    let (_dir, _, app) = fixture().await;
    request(
        &app,
        "POST",
        "/v1/webhook/community/jcb/powers",
        Some("legacy-secret"),
        heartbeat(1),
    )
    .await;
    for (path, error) in [
        ("/v1/network/absent/", "System not found"),
        ("/v1/network/community/absent/", "Project not found"),
        ("/v1/network/community/jcb/absent", "Target not found"),
    ] {
        assert_eq!(
            request(&app, "GET", path, None, Value::Null).await,
            (StatusCode::FORBIDDEN, json!({"error":error}))
        );
    }
    assert_eq!(
        request(
            &app,
            "GET",
            "/v1/network/community/jcb/powers/no",
            None,
            Value::Null
        )
        .await,
        (
            StatusCode::BAD_REQUEST,
            json!({"error":"Invalid status provided"})
        )
    );
    for path in [
        "/v1/network",
        "/v1/network/community",
        "/v1/network/community/jcb",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(response.headers()["location"], format!("{path}/"));
    }
    assert_eq!(
        request(
            &app,
            "GET",
            "/v1/network/community/jcb/powers/",
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
}
#[tokio::test]
async fn public_docs_head_and_cors_are_consistent() {
    let (_dir, _, app) = fixture().await;
    for path in ["/openapi.json", "/OpenAPI.json"] {
        let (status, spec) = request(&app, "GET", path, None, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(spec["paths"].as_object().unwrap().len(), 5);
        assert!(!spec.to_string().contains("webhook"));
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/v1/network/")
                .header("origin", "https://client.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "https://client.example"
    );
    assert!(response.headers().contains_key("content-length"));
    assert!(to_bytes(response.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/v1/network/")
                .header("origin", "https://client.example")
                .header("access-control-request-headers", "X-Custom")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers()["access-control-allow-headers"],
        "X-Custom"
    );
}
#[tokio::test]
async fn concurrent_reads_observe_complete_updates() {
    let (_dir, _, app) = fixture().await;
    request(
        &app,
        "POST",
        "/v1/webhook/community/jcb/powers",
        Some("legacy-secret"),
        heartbeat(1),
    )
    .await;
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..100 {
        let app = app.clone();
        tasks.spawn(async move {
            if n % 10 == 0 {
                assert_eq!(
                    request(
                        &app,
                        "POST",
                        "/v1/webhook/community/jcb/powers",
                        Some("legacy-secret"),
                        heartbeat((n / 10) % 2)
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            } else {
                let (status, body) = request(
                    &app,
                    "GET",
                    "/v1/network/community/jcb/powers",
                    None,
                    Value::Null,
                )
                .await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(body["network"].as_array().unwrap().len(), 1);
                assert!(body["network"][0]["status"] == 0 || body["network"][0]["status"] == 1);
            }
        });
    }
    while let Some(task) = tasks.join_next().await {
        task.unwrap();
    }
}
