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
async fn the_user_sees_and_takes_back_what_the_roles_know_about_them() {
    use aionui_api_types::PlaybookDeltaOp;

    let root = tempfile::tempdir().unwrap();
    let service = Arc::new(RoleService::new(root.path().join("roles")));
    let app = role_routes(RoleRouterState::new(service.clone()));
    let (status, body) = send(&app, Method::GET, "/api/roles/_boss", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["items"], json!([]));

    send(&app, Method::PUT, "/api/roles/scout", Some(json!({}))).await;
    let learned = ["先给结论，再列数据。", "周末不发通知。"].map(|text| PlaybookDeltaOp::Add { content: text.into() });
    service
        .apply_retrospective("scout", &[], &learned, "reflector", "review", 1_000)
        .await
        .unwrap();
    let (_, body) = send(&app, Method::GET, "/api/roles/_boss", None).await;
    assert_eq!(
        body["data"]["items"],
        json!([{ "id": "u-0001", "text": "先给结论，再列数据。" }, { "id": "u-0002", "text": "周末不发通知。" }])
    );

    // Only the user takes an item back; a role asking is refused.
    let (status, _) = send_as_agent(&app, Method::DELETE, "/api/roles/_boss/u-0001", "conv-scout", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = send(&app, Method::DELETE, "/api/roles/_boss/u-0001", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["items"],
        json!([{ "id": "u-0002", "text": "周末不发通知。" }])
    );
    assert!(!service.boss_profile().await.unwrap().contains("先给结论"));
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

/// Records which saves the conversation side was told changed.
#[derive(Default)]
struct FollowedCards(std::sync::Mutex<Vec<String>>);

#[async_trait::async_trait]
impl crate::RoleExternalPort for FollowedCards {
    async fn apply_cron_proposal(
        &self,
        _user_id: &str,
        _role_id: &str,
        _change: &aionui_api_types::RoleProposalChange,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn on_role_dismissed(&self, _user_id: &str, _role_id: &str) {}

    async fn run_dreamer(&self, _user_id: &str, _role_id: &str) -> Result<aionui_api_types::RoleDreamResponse, String> {
        Err("not in this test".into())
    }

    async fn on_debrief_changed(&self, _user_id: &str, debrief: &aionui_api_types::KeeperDebrief) {
        self.0.lock().unwrap().push(debrief.id.clone());
    }
}

/// A keeper of `p_store` with one lesson, and one save that kept a note and
/// warned about that lesson.
async fn keeper_with_a_save(root: &tempfile::TempDir) -> (Arc<RoleService>, aionui_api_types::KeeperDebrief) {
    use aionui_api_types::{
        KeeperDebriefDraft, KeeperDebriefDraftWarning, KeeperDebriefStatus, PlaybookDeltaOp, RoleProjectClaim,
        SetRoleProjectsRequest, UpsertRoleRequest,
    };
    let service = Arc::new(RoleService::new(root.path().join("roles")));
    service
        .upsert(
            "seo",
            UpsertRoleRequest {
                charter: "Keeps the store findable.".into(),
                denied_tools: Vec::new(),
                verifies: false,
                asks_when_scheduled: false,
            },
        )
        .await
        .unwrap();
    service
        .set_projects(
            "seo",
            SetRoleProjectsRequest {
                projects: vec![RoleProjectClaim {
                    project_id: "p_store".into(),
                    name: "store".into(),
                }],
                transfer: false,
            },
            1_000,
        )
        .await
        .unwrap();
    service
        .apply_retrospective(
            "seo",
            &[PlaybookDeltaOp::Add {
                content: "Check robots.txt on staging before shipping".into(),
            }],
            &[],
            "user",
            "seed",
            2_000,
        )
        .await
        .unwrap();
    let draft = KeeperDebriefDraft {
        notes: vec!["Product pages use JSON-LD".into()],
        warnings: vec![KeeperDebriefDraftWarning {
            lesson_id: "p-0001".into(),
            files: vec!["public/robots.txt".into()],
            note: "robots.txt changed without a staging check".into(),
        }],
        ..KeeperDebriefDraft::default()
    };
    let saved = service
        .record_debrief(crate::DebriefRecord {
            role_id: "seo",
            project_id: "p_store",
            conversation_id: "conv-1",
            message_id: "msg-1",
            status: KeeperDebriefStatus::Saved,
            draft: Some(&draft),
            day: "2026-11-21",
            at: 3_000,
        })
        .await
        .unwrap();
    (service, saved)
}

#[tokio::test]
async fn automatic_saves_are_listed_and_only_the_user_takes_them_back() {
    let root = tempfile::tempdir().unwrap();
    let (service, saved) = keeper_with_a_save(&root).await;
    let followed = Arc::new(FollowedCards::default());
    let app = role_routes(RoleRouterState::with_external(service.clone(), followed.clone()));
    let note = saved.items[0].key.clone();
    let forget = format!("/api/roles/seo/debriefs/{}/forget", saved.id);
    let outdated = format!("/api/roles/seo/debriefs/{}/outdated", saved.id);

    let (status, body) = send(&app, Method::GET, "/api/roles/seo/debriefs", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["debriefs"][0]["id"], saved.id.as_str());
    assert_eq!(
        body["data"]["debriefs"][0]["items"][0]["text"],
        "Product pages use JSON-LD"
    );
    assert_eq!(body["data"]["debriefs"][0]["warnings"][0]["lesson_id"], "p-0001");
    let (status, body) = send(&app, Method::GET, "/api/roles/nobody/debriefs", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "role_not_found");

    // An agent can take nothing back.
    let (status, body) = send_as_agent(&app, Method::POST, &forget, "conv-1", Some(json!({ "key": note }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "role_user_only");
    let (status, _) = send_as_agent(
        &app,
        Method::POST,
        &outdated,
        "conv-1",
        Some(json!({ "lesson_id": "p-0001" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(service.get("seo").await.unwrap().memory.contains(&note));
    assert!(followed.0.lock().unwrap().is_empty());

    // The user can, and the card follows.
    let (status, body) = send(&app, Method::POST, &forget, Some(json!({ "key": note }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["debrief"]["items"][0]["forgotten"], true);
    assert!(!service.get("seo").await.unwrap().memory.contains(&note));
    let (status, body) = send(&app, Method::POST, &outdated, Some(json!({ "lesson_id": " p-0001 " }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["debrief"]["warnings"][0]["outdated"], true);
    assert_eq!(*followed.0.lock().unwrap(), vec![saved.id.clone(), saved.id.clone()]);

    let (status, body) = send(&app, Method::POST, &forget, Some(json!({ "key": "nope" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "role_bad_request");
    let (status, _) = send(
        &app,
        Method::POST,
        "/api/roles/seo/debriefs/d-404/outdated",
        Some(json!({ "lesson_id": "p-0001" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
