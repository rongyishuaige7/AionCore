//! A resident role reads and writes its own memory from inside the conversation
//! it runs in, through the REAL app router: the token-bound conversation decides
//! which role is speaking, and the role's permission table still applies.

mod common;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_app::{AppConfig, AppServices, create_router};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::body_json;
use serde_json::{Value, json};
use tower::ServiceExt;

const USER: &str = "system_default_user";
const ROLE: &str = "assistant-reviewer";

struct Fixture {
    app: axum::Router,
    services: AppServices,
    _data_dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let data_dir = tempfile::tempdir().unwrap();
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        data_dir: data_dir.path().to_path_buf(),
        work_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let services = AppServices::from_config(db, &config).await.unwrap();
    let app = create_router(&services).await.expect("build router");
    Fixture {
        app,
        services,
        _data_dir: data_dir,
    }
}

/// A conversation run as `assistant_id`, and the helper token its agent holds.
async fn conversation_as(fixture: &Fixture, conversation_id: &str, assistant_id: Option<&str>) -> String {
    let pool = fixture.services.database.pool();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES (?, ?, 'run', 'acp', '{}', 'finished', 1, 1)",
    )
    .bind(conversation_id)
    .bind(USER)
    .execute(pool)
    .await
    .unwrap();
    if let Some(assistant_id) = assistant_id {
        // The snapshot has to point at a real definition; which one does not
        // matter here, the role is found by the snapshot's assistant id.
        let (definition_id,): (String,) = sqlx::query_as("SELECT id FROM assistant_definitions LIMIT 1")
            .fetch_one(pool)
            .await
            .expect("the app seeds its built-in assistants");
        sqlx::query(
            "INSERT INTO conversation_assistant_snapshots (
                conversation_id, assistant_definition_id, assistant_id, assistant_source,
                agent_id, rules_content, default_model_mode, default_permission_mode,
                default_thought_level_mode, default_skills_mode, resolved_skill_ids,
                resolved_disabled_builtin_skill_ids, default_mcps_mode, resolved_mcp_ids,
                created_at, updated_at
             ) VALUES (?, ?, ?, 'user', 'agent-1', '', 'auto', 'auto', 'auto', 'auto', '[]', '[]',
                'auto', '[]', 1, 1)",
        )
        .bind(conversation_id)
        .bind(definition_id)
        .bind(assistant_id)
        .execute(pool)
        .await
        .unwrap();
    }
    fixture
        .services
        .runtime_token_service
        .issue(
            USER,
            conversation_id,
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token
}

/// Define the role the way the user does, through the ordinary role service.
async fn define_role(fixture: &Fixture, denied_tools: &[&str]) {
    aionui_role::RoleService::new(fixture.services.data_dir.join("roles"))
        .upsert(
            ROLE,
            aionui_api_types::UpsertRoleRequest {
                charter: "Checks work before it is called done.".into(),
                denied_tools: denied_tools.iter().map(|tool| (*tool).to_owned()).collect(),
                verifies: false,
            },
        )
        .await
        .unwrap();
}

async fn call(
    fixture: &Fixture,
    method: &str,
    path: &str,
    conversation_id: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-aionui-user-id", USER)
        .header("x-aionui-conversation-id", conversation_id)
        .header("x-aionui-runtime-token", token)
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = fixture.app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

#[tokio::test]
async fn a_role_run_on_its_own_writes_its_memory_and_progress_and_reads_them_back() {
    let fixture = fixture().await;
    define_role(&fixture, &[]).await;
    let token = conversation_as(&fixture, "conv-role", Some(ROLE)).await;

    let (status, noted) = call(
        &fixture,
        "POST",
        "/api/runtime/role/remember",
        "conv-role",
        &token,
        Some(json!({ "note": "the user reads reports on a phone" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{noted}");
    assert_eq!(noted["data"]["already_recorded"], json!(false));

    let (status, moved) = call(
        &fixture,
        "POST",
        "/api/runtime/role/progress",
        "conv-role",
        &token,
        Some(json!({ "task": "Weekly report", "state": "Drafted.\nLeft: the numbers for March." })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["data"], json!({ "task": "weekly-report", "closed": false }));

    let (status, shown) = call(&fixture, "GET", "/api/runtime/role", "conv-role", &token, None).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["data"]["id"], json!(ROLE));
    assert!(
        shown["data"]["memory"]
            .as_str()
            .unwrap()
            .ends_with("the user reads reports on a phone\n"),
        "{shown}"
    );
    assert_eq!(shown["data"]["progress"][0]["task"], json!("weekly-report"));
    assert_eq!(
        shown["data"]["progress"][0]["state"],
        json!("Drafted.\nLeft: the numbers for March.\n")
    );

    // Leaving the state out closes the task.
    let (status, closed) = call(
        &fixture,
        "POST",
        "/api/runtime/role/progress",
        "conv-role",
        &token,
        Some(json!({ "task": "weekly-report" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["data"]["closed"], json!(true));
    fixture.services.database.close().await;
}

#[tokio::test]
async fn a_role_tidies_its_whole_memory_and_the_version_it_replaced_is_kept() {
    let fixture = fixture().await;
    define_role(&fixture, &[]).await;
    let token = conversation_as(&fixture, "conv-role", Some(ROLE)).await;
    for note in ["run the tests", "run the tests before pushing"] {
        call(
            &fixture,
            "POST",
            "/api/runtime/role/remember",
            "conv-role",
            &token,
            Some(json!({ "note": note })),
        )
        .await;
    }

    let (status, curated) = call(
        &fixture,
        "POST",
        "/api/runtime/role/curate",
        "conv-role",
        &token,
        Some(json!({ "memory": "- run the tests before pushing" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{curated}");
    let archived_as = curated["data"]["archived_as"]
        .as_str()
        .expect("the old memory is archived");

    let (_, shown) = call(&fixture, "GET", "/api/runtime/role", "conv-role", &token, None).await;
    assert_eq!(shown["data"]["memory"], json!("- run the tests before pushing\n"));
    let archived = std::path::Path::new(shown["data"]["memory_dir"].as_str().unwrap())
        .join("archive")
        .join(archived_as);
    let replaced = std::fs::read_to_string(archived).unwrap();
    assert!(replaced.contains("run the tests\n"), "{replaced}");

    let (status, refused) = call(
        &fixture,
        "POST",
        "/api/runtime/role/curate",
        "conv-role",
        &token,
        Some(json!({ "memory": "   " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    fixture.services.database.close().await;
}

#[tokio::test]
async fn a_role_kept_from_its_memory_is_refused_here_as_it_is_in_a_team() {
    let fixture = fixture().await;
    define_role(&fixture, &["team_remember", "team_progress"]).await;
    let token = conversation_as(&fixture, "conv-role", Some(ROLE)).await;

    for (path, body) in [
        ("/api/runtime/role/remember", json!({ "note": "x" })),
        ("/api/runtime/role/curate", json!({ "memory": "- x" })),
        ("/api/runtime/role/progress", json!({ "task": "t", "state": "s" })),
    ] {
        let (status, refused) = call(&fixture, "POST", path, "conv-role", &token, Some(body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {refused}");
        assert_eq!(refused["error"]["code"], json!("permission_denied"));
    }
    // Reading stays open: the role is still told what it knows.
    let (status, _) = call(&fixture, "GET", "/api/runtime/role", "conv-role", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    fixture.services.database.close().await;
}

#[tokio::test]
async fn a_conversation_that_is_not_a_role_has_no_role_to_write_to() {
    let fixture = fixture().await;
    define_role(&fixture, &[]).await;
    let plain = conversation_as(&fixture, "conv-plain", None).await;
    let other = conversation_as(&fixture, "conv-other", Some("assistant-writer")).await;

    for (conversation_id, token) in [("conv-plain", &plain), ("conv-other", &other)] {
        let (status, refused) = call(
            &fixture,
            "POST",
            "/api/runtime/role/remember",
            conversation_id,
            token,
            Some(json!({ "note": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{conversation_id}: {refused}");
        assert_eq!(refused["error"]["code"], json!("not_a_role"));
    }
    fixture.services.database.close().await;
}

#[tokio::test]
async fn a_token_opens_only_its_own_conversation_and_a_body_cannot_name_a_role() {
    let fixture = fixture().await;
    define_role(&fixture, &[]).await;
    let role_token = conversation_as(&fixture, "conv-role", Some(ROLE)).await;
    let plain_token = conversation_as(&fixture, "conv-plain", None).await;

    // Another conversation's token does not speak for the role's conversation.
    let (status, refused) = call(
        &fixture,
        "POST",
        "/api/runtime/role/remember",
        "conv-role",
        &plain_token,
        Some(json!({ "note": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");
    assert_eq!(refused["error"]["code"], json!("runtime_auth_failed"));

    let (status, refused) = call(
        &fixture,
        "POST",
        "/api/runtime/role/remember",
        "conv-role",
        &role_token,
        Some(json!({ "note": "x", "role": "assistant-writer" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"]["code"], json!("schema_validation_failed"));
    fixture.services.database.close().await;
}
