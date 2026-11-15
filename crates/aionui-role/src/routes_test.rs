//! Route tests: wire shape of the happy path, and the error codes a client
//! (and a hostile path segment) gets back.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::service::RoleService;

fn app(root: &tempfile::TempDir) -> Router {
    role_routes(RoleRouterState {
        service: Arc::new(RoleService::new(root.path().join("roles"))),
    })
}

async fn send(app: &Router, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn role_lifecycle_over_http() {
    let root = tempfile::tempdir().unwrap();
    let app = app(&root);

    let (status, body) = send(&app, Method::GET, "/api/roles", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["roles"], json!([]));

    let (status, body) = send(
        &app,
        Method::PUT,
        "/api/roles/reviewer",
        Some(json!({ "charter": "Checks work before it is called done.", "denied_tools": ["team_task_create"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["id"], "reviewer");
    assert_eq!(body["data"]["denied_tools"], json!(["team_task_create"]));

    let (status, body) = send(
        &app,
        Method::PUT,
        "/api/roles/reviewer/memory",
        Some(json!({ "memory": "- user wants tests run, not described\n" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["memory"], "- user wants tests run, not described\n");

    let (status, body) = send(&app, Method::GET, "/api/roles", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["roles"].as_array().unwrap().len(), 1);

    let (status, _) = send(&app, Method::DELETE, "/api/roles/reviewer", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(&app, Method::GET, "/api/roles/reviewer", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "role_not_found");
}

#[tokio::test]
async fn a_percent_encoded_colon_in_the_id_reaches_the_role() {
    let root = tempfile::tempdir().unwrap();
    let app = app(&root);

    // The frontend sends `encodeURIComponent("bare:8e1acf31")`.
    let (status, body) = send(
        &app,
        Method::PUT,
        "/api/roles/bare%3A8e1acf31",
        Some(json!({ "charter": "Writes the code." })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["id"], "bare:8e1acf31");

    let (status, body) = send(&app, Method::GET, "/api/roles/bare%3A8e1acf31", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["charter"], "Writes the code.");
}

#[tokio::test]
async fn bad_input_maps_to_stable_codes() {
    let root = tempfile::tempdir().unwrap();
    let app = app(&root);

    let (status, body) = send(
        &app,
        Method::PUT,
        "/api/roles/reviewer",
        Some(json!({ "denied_tools": ["not_a_tool"] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "role_bad_request");

    let (status, body) = send(
        &app,
        Method::PUT,
        "/api/roles/missing/memory",
        Some(json!({ "memory": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "role_not_found");
}

#[tokio::test]
async fn path_traversal_in_the_id_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let app = app(&root);

    // `%2E%2E%2Fescape` decodes to `../escape` inside a single path segment.
    let (status, body) = send(
        &app,
        Method::PUT,
        "/api/roles/%2E%2E%2Fescape",
        Some(json!({ "charter": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "role_invalid_id");
    assert!(!root.path().join("escape").exists());
}

#[tokio::test]
async fn a_task_in_progress_shows_on_the_role_and_can_be_closed_over_http() {
    let root = tempfile::tempdir().unwrap();
    let service = Arc::new(RoleService::new(root.path().join("roles")));
    let app = role_routes(RoleRouterState {
        service: service.clone(),
    });
    send(&app, Method::PUT, "/api/roles/scout", Some(json!({}))).await;
    service
        .set_progress("scout", "舆情 周报", "Monday's sweep done.")
        .await
        .unwrap();

    let (status, body) = send(&app, Method::GET, "/api/roles/scout", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["progress"][0]["task"], "舆情-周报");
    assert_eq!(body["data"]["progress"][0]["state"], "Monday's sweep done.\n");
    assert!(body["data"]["progress"][0]["updated_at"].as_i64().unwrap() > 0);

    // The task name travels percent-encoded, as a browser sends it.
    let (status, body) = send(
        &app,
        Method::DELETE,
        "/api/roles/scout/progress/%E8%88%86%E6%83%85-%E5%91%A8%E6%8A%A5",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["progress"], json!([]));

    let (status, body) = send(&app, Method::DELETE, "/api/roles/nobody/progress/task", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "role_not_found");
}
