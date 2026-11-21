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
    role_routes(RoleRouterState::new(Arc::new(RoleService::new(
        root.path().join("roles"),
    ))))
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
    let app = role_routes(RoleRouterState::new(service.clone()));
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

async fn send_as_agent(
    app: &Router,
    method: Method,
    uri: &str,
    conv_id: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let mut request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    request.extensions_mut().insert(RuntimeHelperCaller {
        user_id: "system_default_user".into(),
        conversation_id: conv_id.into(),
    });
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn agent_major_changes_become_proposals_while_tightening_applies_and_only_user_approves() {
    let root = tempfile::tempdir().unwrap();
    let app = app(&root);

    // 1. Agent hires a brand-new role: applies immediately.
    let (status, hired) = send_as_agent(
        &app,
        Method::PUT,
        "/api/roles/watcher",
        "conv-butler",
        Some(json!({
            "charter": "Watch CI builds.",
            "denied_tools": ["team_spawn_agent", "team_task_create"],
            "asks_when_scheduled": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hired["data"]["charter"], "Watch CI builds.");
    assert_eq!(hired["data"]["proposals"], json!([]));

    // 2. Agent tightens (`verifies: true`) AND attempts major changes (edit charter + turn off ask-first + allow team_task_create).
    let (status, updated) = send_as_agent(
        &app,
        Method::PUT,
        "/api/roles/watcher",
        "conv-watcher",
        Some(json!({
            "charter": "Deploy straight to prod without asking.",
            "denied_tools": ["team_spawn_agent"],
            "verifies": true,
            "asks_when_scheduled": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Tightening applied immediately; major changes stayed gated!
    assert_eq!(updated["data"]["verifies"], true);
    assert_eq!(updated["data"]["charter"], "Watch CI builds.");
    assert_eq!(updated["data"]["asks_when_scheduled"], true);
    assert_eq!(
        updated["data"]["denied_tools"],
        json!(["team_spawn_agent", "team_task_create"])
    );
    let proposals = updated["data"]["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1);
    let prop_id = proposals[0]["id"].as_str().unwrap().to_owned();

    // 3. Agent CANNOT approve or reject its own proposal (`403 Forbidden`).
    let (status, _) = send_as_agent(
        &app,
        Method::POST,
        &format!("/api/roles/watcher/proposals/{prop_id}/approve"),
        "conv-watcher",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 4. Human user approves the proposal -> changes take effect and commit to timeline!
    let (status, approved) = send(
        &app,
        Method::POST,
        &format!("/api/roles/watcher/proposals/{prop_id}/approve"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(approved["data"]["charter"], "Deploy straight to prod without asking.");
    assert_eq!(approved["data"]["asks_when_scheduled"], false);
    assert_eq!(approved["data"]["denied_tools"], json!(["team_spawn_agent"]));
    assert_eq!(approved["data"]["proposals"], json!([]));

    // 5. Timeline records every step and human user can 1-click revert the approval commit!
    let (status, tl) = send(&app, Method::GET, "/api/roles/watcher/timeline", None).await;
    assert_eq!(status, StatusCode::OK);
    let commits = tl["data"]["commits"].as_array().unwrap();
    assert!(commits.len() >= 3, "{commits:?}");
    let head_sha = commits[0]["sha"].as_str().unwrap().to_owned();
    assert!(commits[0]["can_revert"].as_bool().unwrap());

    // Agent cannot revert.
    let (status, _) = send_as_agent(
        &app,
        Method::POST,
        "/api/roles/watcher/revert",
        "conv-watcher",
        Some(json!({ "sha": head_sha })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // User reverts -> charter & asks_when_scheduled roll back via a forward commit!
    let (status, reverted) = send(
        &app,
        Method::POST,
        "/api/roles/watcher/revert",
        Some(json!({ "sha": head_sha })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reverted["data"]["charter"], "Watch CI builds.");
    assert_eq!(reverted["data"]["asks_when_scheduled"], true);
}

#[tokio::test]
async fn keepers_over_http_where_only_the_user_hands_out_projects() {
    let root = tempfile::tempdir().unwrap();
    let app = app(&root);
    for id in ["chief", "seo"] {
        let (status, _) = send(
            &app,
            Method::PUT,
            &format!("/api/roles/{id}"),
            Some(json!({ "charter": "" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, body) = send(&app, Method::GET, "/api/roles/keepers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["keepers"], json!([]));

    let one = json!({ "projects": [{ "project_id": "project_j", "name": "journal" }] });
    let (status, body) = send(&app, Method::PUT, "/api/roles/chief/projects", Some(one.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["projects"][0]["project_id"], "project_j");

    let (status, body) = send(&app, Method::PUT, "/api/roles/seo/projects", Some(one.clone())).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "role_project_taken");

    let (status, body) = send_as_agent(&app, Method::PUT, "/api/roles/seo/projects", "conv-1", Some(one)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "role_user_review_required");

    let taken = json!({ "projects": [{ "project_id": "project_j", "name": "journal" }], "transfer": true });
    let (status, _) = send(&app, Method::PUT, "/api/roles/seo/projects", Some(taken)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = send(&app, Method::GET, "/api/roles/keepers", None).await;
    assert_eq!(body["data"]["keepers"][0]["role_id"], "seo");

    let (status, body) = send_as_agent(
        &app,
        Method::PUT,
        "/api/roles/seo/projects",
        "conv-1",
        Some(json!({ "projects": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["projects"], json!([]));
}
