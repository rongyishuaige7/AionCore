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
    define_role_as(fixture, ROLE, denied_tools).await;
}

/// An assistant the app really has, for what needs more of a role's assistant
/// than its id: a timer is set up to run as it.
async fn seeded_assistant_id(fixture: &Fixture) -> String {
    let (assistant_id,): (String,) =
        sqlx::query_as("SELECT assistant_id FROM assistant_definitions WHERE source = 'builtin' LIMIT 1")
            .fetch_one(fixture.services.database.pool())
            .await
            .expect("the app seeds its built-in assistants");
    assistant_id
}

async fn define_role_as(fixture: &Fixture, role: &str, denied_tools: &[&str]) {
    aionui_role::RoleService::new(fixture.services.data_dir.join("roles"))
        .upsert(
            role,
            aionui_api_types::UpsertRoleRequest {
                charter: "Checks work before it is called done.".into(),
                denied_tools: denied_tools.iter().map(|tool| (*tool).to_owned()).collect(),
                verifies: false,
                asks_when_scheduled: false,
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
async fn a_role_sets_itself_a_follow_up_that_waits_as_a_one_shot_task_for_its_conversation() {
    let fixture = fixture().await;
    let role = seeded_assistant_id(&fixture).await;
    define_role_as(&fixture, &role, &[]).await;
    let token = conversation_as(&fixture, "conv-role", Some(&role)).await;
    let plain = conversation_as(&fixture, "conv-plain", None).await;

    let (status, set) = call(
        &fixture,
        "POST",
        "/api/runtime/role/follow-up",
        "conv-role",
        &token,
        Some(json!({ "note": "check whether the deploy went out", "after_minutes": 45 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["meta"]["command"], json!("role follow-up"));
    assert_eq!(set["data"]["pending"], json!(1));
    let job_id = set["data"]["job_id"].as_str().expect("job id").to_owned();
    let due_at = set["data"]["due_at"].as_i64().expect("due time");

    let (kind, created_by, conversation_id, message, next_run_at): (String, String, String, String, Option<i64>) =
        sqlx::query_as(
            "SELECT schedule_kind, created_by, conversation_id, payload_message, next_run_at
             FROM cron_jobs WHERE id = ?",
        )
        .bind(&job_id)
        .fetch_one(fixture.services.database.pool())
        .await
        .unwrap();
    assert_eq!(kind, "at");
    assert_eq!(created_by, "agent");
    assert_eq!(conversation_id, "conv-role");
    assert_eq!(message, "check whether the deploy went out");
    assert_eq!(next_run_at, Some(due_at));

    // The delay has to be a real one, and the caller has to be a role.
    let (status, refused) = call(
        &fixture,
        "POST",
        "/api/runtime/role/follow-up",
        "conv-role",
        &token,
        Some(json!({ "note": "x", "after_minutes": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"]["code"], json!("schema_validation_failed"));

    let (status, refused) = call(
        &fixture,
        "POST",
        "/api/runtime/role/follow-up",
        "conv-plain",
        &plain,
        Some(json!({ "note": "x", "after_minutes": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    assert_eq!(refused["error"]["code"], json!("not_a_role"));
    fixture.services.database.close().await;
}

#[tokio::test]
async fn a_run_declares_how_it_ended_and_what_it_told_the_user_is_kept_and_announced() {
    let fixture = fixture().await;
    // Kept from its memory: declaring how a run ended is not a memory write.
    define_role(&fixture, &["team_remember", "team_progress"]).await;
    let token = conversation_as(&fixture, "conv-role", Some(ROLE)).await;
    let mut events = fixture.services.event_bus.subscribe();
    let post = |body: Value| {
        call(
            &fixture,
            "POST",
            "/api/runtime/role/report",
            "conv-role",
            &token,
            Some(body),
        )
    };

    // A quiet run: announced, so that it asks for no attention, and not kept.
    let (status, silent) = post(json!({ "outcome": "silent" })).await;
    assert_eq!(status, StatusCode::OK, "{silent}");
    assert_eq!(silent["data"], json!({ "outcome": "silent", "recorded": false }));
    let event = events.recv().await.unwrap();
    assert_eq!(event.name, "role.reported");
    assert_eq!(event.data["conversation_id"], json!("conv-role"));
    assert_eq!(event.data["user_id"], json!(USER));
    assert_eq!(event.data["outcome"], json!("silent"));

    // A question for the user: kept as one line, with where it was asked.
    let (status, asked) = post(json!({ "outcome": "ask", "summary": "ship it\nto finance too?" })).await;
    assert_eq!(status, StatusCode::OK, "{asked}");
    assert_eq!(asked["data"], json!({ "outcome": "ask", "recorded": true }));
    let event = events.recv().await.unwrap();
    assert_eq!(event.data["outcome"], json!("ask"));
    assert_eq!(event.data["summary"], json!("ship it to finance too?"));
    assert_eq!(event.data["role_id"], json!(ROLE));

    let (_, shown) = call(&fixture, "GET", "/api/runtime/role", "conv-role", &token, None).await;
    let reported = shown["data"]["reported"].as_array().unwrap();
    assert_eq!(reported.len(), 1);
    assert_eq!(reported[0]["outcome"], json!("ask"));
    assert_eq!(reported[0]["summary"], json!("ship it to finance too?"));
    assert_eq!(reported[0]["conversation_id"], json!("conv-role"));

    // Something to say needs the line to say, and an outcome has to be one of the three.
    for body in [
        json!({ "outcome": "say" }),
        json!({ "outcome": "say", "summary": "  " }),
        json!({ "outcome": "shout", "summary": "x" }),
        json!({ "summary": "x" }),
    ] {
        let (status, refused) = post(body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {refused}");
        assert_eq!(refused["error"]["code"], json!("schema_validation_failed"));
    }
    assert!(events.try_recv().is_err(), "a refused report announces nothing");
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

#[tokio::test]
async fn recall_and_expand_search_only_the_caller_roles_thread_runs_and_leaves() {
    let fixture = fixture().await;
    // Even a role kept from writing its memory/progress can recall and expand.
    define_role(&fixture, &["team_remember", "team_progress"]).await;
    define_role_as(&fixture, "assistant-other", &[]).await;

    let pool = fixture.services.database.pool();
    let thread_token = conversation_as(&fixture, "conv-thread", Some(ROLE)).await;
    let _aside_token = conversation_as(&fixture, "conv-aside", Some(ROLE)).await;
    let run_token = conversation_as(&fixture, "conv-run", Some(ROLE)).await;
    let _team_token = conversation_as(&fixture, "conv-team", Some(ROLE)).await;
    let other_token = conversation_as(&fixture, "conv-other-role", Some("assistant-other")).await;

    // Mark conv-run as a scheduled run and conv-team as a team member session.
    sqlx::query("UPDATE conversations SET extra = ? WHERE id = 'conv-run'")
        .bind(r#"{"cron_job_id":"job_daily"}"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET extra = ? WHERE id = 'conv-team'")
        .bind(r#"{"cron_job_id":"job_team","teamId":"team_1"}"#)
        .execute(pool)
        .await
        .unwrap();

    // Adopt conv-thread as ROLE's one thread and add one summary leaf.
    let role_service = aionui_role::RoleService::new(fixture.services.data_dir.join("roles"));
    role_service
        .open_thread_turn(ROLE, "conv-thread", false, 1_000)
        .await
        .unwrap();
    role_service
        .add_thread_leaf(
            ROLE,
            "conv-thread",
            "<!-- 2026-04-12 09:00 → 2026-04-12 09:30 · 6 messages -->\n- 敲定了供应链对账口径与月度阈值",
            aionui_role::ThreadMark {
                created_at: 900,
                id: "m0".into(),
            },
        )
        .await
        .unwrap();

    let insert_msg = |id: &'static str,
                      conv: &'static str,
                      kind: &'static str,
                      pos: &'static str,
                      status: &'static str,
                      at: i64,
                      content: Value| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO messages (id, conversation_id, type, content, position, status, hidden, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, 0, ?)",
            )
            .bind(id)
            .bind(conv)
            .bind(kind)
            .bind(content.to_string())
            .bind(pos)
            .bind(status)
            .bind(at)
            .execute(&pool)
            .await
            .unwrap();
        }
    };

    // Thread messages + report card + an in-flight streaming message at the tail.
    insert_msg(
        "m1",
        "conv-thread",
        "text",
        "right",
        "finish",
        1_000,
        json!({ "content": "请检查周报里的数据源核对逻辑" }),
    )
    .await;
    insert_msg(
        "m2",
        "conv-thread",
        "text",
        "left",
        "finish",
        2_000,
        json!({ "content": "数据源核对脚本已切换到 v2 分支" }),
    )
    .await;
    insert_msg(
        "m3",
        "conv-thread",
        "role_report",
        "center",
        "finish",
        3_000,
        json!({ "outcome": "say", "summary": "夜间巡检完成：数据源核对零异常" }),
    )
    .await;
    insert_msg(
        "m4_stream",
        "conv-thread",
        "text",
        "left",
        "pending",
        4_000,
        json!({ "content": "数据源核对流式半截内容暂未完成" }),
    )
    .await;

    // Scheduled run message (included).
    insert_msg(
        "r1",
        "conv-run",
        "text",
        "left",
        "finish",
        5_000,
        json!({ "content": "早八点定时任务：数据源核对发现 2 条延迟记录" }),
    )
    .await;

    // Aside, team, and other-role conversations (all MUST be excluded for ROLE).
    insert_msg(
        "aside1",
        "conv-aside",
        "text",
        "right",
        "finish",
        6_000,
        json!({ "content": "数据源核对旁路对话不应进入索引" }),
    )
    .await;
    insert_msg(
        "team1",
        "conv-team",
        "text",
        "left",
        "finish",
        7_000,
        json!({ "content": "数据源核对团队内部消息不应进入角色召回" }),
    )
    .await;
    insert_msg(
        "other1",
        "conv-other-role",
        "text",
        "left",
        "finish",
        8_000,
        json!({ "content": "数据源核对来自另一个常驻角色" }),
    )
    .await;

    // 1. Trigram search from the scheduled run finds thread + report + run, excluding pending/aside/team/other.
    let (status, recalled) = call(
        &fixture,
        "POST",
        "/api/runtime/role/recall",
        "conv-run",
        &run_token,
        Some(json!({ "query": "数据源核对" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{recalled}");
    let hits = recalled["data"]["hits"].as_array().unwrap();
    let ref_ids: Vec<&str> = hits.iter().map(|h| h["ref_id"].as_str().unwrap()).collect();
    assert_eq!(ref_ids.len(), 4, "{ref_ids:?}");
    for expected in [
        "msg:conv-thread:m1",
        "msg:conv-thread:m2",
        "msg:conv-thread:m3",
        "msg:conv-run:r1",
    ] {
        assert!(ref_ids.contains(&expected), "missing {expected} in {ref_ids:?}");
    }

    // 2. Once m4_stream finishes, incremental catch-up indexes it without losing watermark continuity!
    sqlx::query("UPDATE messages SET status = 'finish', content = ? WHERE id = 'm4_stream'")
        .bind(json!({ "content": "数据源核对第四条已完成落盘" }).to_string())
        .execute(pool)
        .await
        .unwrap();
    let (status, recalled_after) = call(
        &fixture,
        "POST",
        "/api/runtime/role/recall",
        "conv-thread",
        &thread_token,
        Some(json!({ "query": "已完成落盘" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{recalled_after}");
    assert_eq!(
        recalled_after["data"]["hits"][0]["ref_id"],
        json!("msg:conv-thread:m4_stream")
    );

    // 3. 2-char Chinese short query falls back to LIKE cleanly.
    let (status, short) = call(
        &fixture,
        "POST",
        "/api/runtime/role/recall",
        "conv-thread",
        &thread_token,
        Some(json!({ "query": "周报", "scope": "thread" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{short}");
    assert_eq!(short["data"]["hits"].as_array().unwrap().len(), 1);
    assert_eq!(short["data"]["hits"][0]["ref_id"], json!("msg:conv-thread:m1"));

    // 4. Expand message window returns chronological neighbors around m2.
    let (status, expanded_msg) = call(
        &fixture,
        "POST",
        "/api/runtime/role/expand",
        "conv-thread",
        &thread_token,
        Some(json!({ "ref_id": "msg:conv-thread:m2", "before": 1, "after": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{expanded_msg}");
    let window = expanded_msg["data"]["messages"].as_array().unwrap();
    assert_eq!(window.len(), 3);
    assert_eq!(window[0]["ref_id"], json!("msg:conv-thread:m1"));
    assert_eq!(window[1]["ref_id"], json!("msg:conv-thread:m2"));
    assert_eq!(window[1]["hit"], json!(true));
    assert_eq!(window[2]["ref_id"], json!("msg:conv-thread:m3"));
    assert_eq!(window[2]["speaker"], json!("report"));

    // 5. Recall and expand summary leaf.
    let (status, leaf_recall) = call(
        &fixture,
        "POST",
        "/api/runtime/role/recall",
        "conv-thread",
        &thread_token,
        Some(json!({ "query": "供应链对账口径" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{leaf_recall}");
    assert_eq!(
        leaf_recall["data"]["hits"][0]["ref_id"],
        json!("leaf:conv-thread:00001")
    );
    let (status, expanded_leaf) = call(
        &fixture,
        "POST",
        "/api/runtime/role/expand",
        "conv-thread",
        &thread_token,
        Some(json!({ "ref_id": "leaf:conv-thread:00001" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{expanded_leaf}");
    assert_eq!(expanded_leaf["data"]["kind"], json!("leaf"));
    assert_eq!(expanded_leaf["data"]["leaf"], json!("- 敲定了供应链对账口径与月度阈值"));

    // 6. Another role cannot expand ROLE's ref_id.
    let (status, cross_role) = call(
        &fixture,
        "POST",
        "/api/runtime/role/expand",
        "conv-other-role",
        &other_token,
        Some(json!({ "ref_id": "msg:conv-thread:m2" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{cross_role}");
    assert_eq!(cross_role["error"]["code"], json!("schema_validation_failed"));

    fixture.services.database.close().await;
}
