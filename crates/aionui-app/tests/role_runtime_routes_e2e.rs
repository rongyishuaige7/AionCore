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

async fn user_call(
    fixture: &Fixture,
    method: &str,
    path: &str,
    token: &str,
    csrf: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("x-csrf-token", csrf)
        .header("cookie", format!("aionui-csrf-token={csrf}"))
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let response = fixture.app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

#[tokio::test]
async fn proposals_gate_agent_role_and_cron_mutations_and_dismiss_cleans_up_schedules() {
    let mut fixture = fixture().await;
    let (user_token, csrf) = common::setup_and_login(&mut fixture.app, &fixture.services, "admin", "secret123").await;
    let role_id = seeded_assistant_id(&fixture).await;
    define_role_as(&fixture, &role_id, &["team_spawn_agent"]).await;

    let agent_token = conversation_as(&fixture, "conv-steward", Some(&role_id)).await;

    // 1. Agent attempts to relax charter & permissions via PUT /api/roles/{id}: held as proposal!
    let (status, put_res) = call(
        &fixture,
        "PUT",
        &format!("/api/roles/{role_id}"),
        "conv-steward",
        &agent_token,
        Some(json!({
            "charter": "Unilateral charter change",
            "denied_tools": [],
            "verifies": true,
            "asks_when_scheduled": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{put_res}");
    // `verifies: true` tightened immediately, while charter & denied_tools stayed unchanged.
    assert_eq!(put_res["data"]["verifies"], json!(true));
    assert_eq!(
        put_res["data"]["charter"],
        json!("Checks work before it is called done.")
    );
    assert_eq!(put_res["data"]["denied_tools"], json!(["team_spawn_agent"]));
    let role_prop_id = put_res["data"]["proposals"][0]["id"].as_str().unwrap().to_owned();

    // 2. Agent attempts to create a cron job for the resident role: held as 202 proposal!
    let (status, cron_res) = call(
        &fixture,
        "POST",
        "/api/cron/jobs",
        "conv-steward",
        &agent_token,
        Some(json!({
            "name": "Morning Sweep",
            "schedule": { "kind": "cron", "expr": "0 8 * * *" },
            "prompt": "Run morning check",
            "conversation_id": "conv-steward",
            "created_by": "agent",
            "execution_mode": "new_conversation",
            "agent_config": {
                "name": "Sweeper",
                "assistant_id": role_id,
                "model": {
                    "provider_id": "p1",
                    "model": "m1"
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{cron_res}");
    assert_eq!(cron_res["data"]["status"], json!("proposal_pending"));
    let cron_prop_id = cron_res["data"]["proposal"]["id"].as_str().unwrap().to_owned();

    let (status, jobs_before) = user_call(&fixture, "GET", "/api/cron/jobs", &user_token, &csrf, None).await;
    assert_eq!(status, StatusCode::OK, "{jobs_before}");
    assert_eq!(jobs_before["data"].as_array().unwrap().len(), 0);

    // 3. Agent also sets a one-shot follow-up (`role follow-up` stays direct, not gated).
    let (status, fu) = call(
        &fixture,
        "POST",
        "/api/runtime/role/follow-up",
        "conv-steward",
        &agent_token,
        Some(json!({ "note": "check later", "after_minutes": 60 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fu}");

    // 4. Agent cannot approve its own proposal.
    let (status, _) = call(
        &fixture,
        "POST",
        &format!("/api/roles/{role_id}/proposals/{cron_prop_id}/approve"),
        "conv-steward",
        &agent_token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 5. User approves both proposals -> charter updates and cron job is created!
    let (status, approved_role) = user_call(
        &fixture,
        "POST",
        &format!("/api/roles/{role_id}/proposals/{role_prop_id}/approve"),
        &user_token,
        &csrf,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved_role}");
    assert_eq!(approved_role["data"]["charter"], json!("Unilateral charter change"));

    let (status, approved_cron) = user_call(
        &fixture,
        "POST",
        &format!("/api/roles/{role_id}/proposals/{cron_prop_id}/approve"),
        &user_token,
        &csrf,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved_cron}");
    assert_eq!(approved_cron["data"]["proposals"], json!([]));

    let (_, jobs_after) = user_call(&fixture, "GET", "/api/cron/jobs", &user_token, &csrf, None).await;
    let jobs_arr = jobs_after["data"].as_array().unwrap();
    assert_eq!(
        jobs_arr.len(),
        2,
        "1 approved recurring job + 1 follow-up: {jobs_arr:?}"
    );

    // 6. User dismisses the role -> follow-up is deleted and recurring schedule is paused!
    let (status, _) = user_call(
        &fixture,
        "DELETE",
        &format!("/api/roles/{role_id}"),
        &user_token,
        &csrf,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, jobs_dismissed) = user_call(&fixture, "GET", "/api/cron/jobs", &user_token, &csrf, None).await;
    let remaining = jobs_dismissed["data"].as_array().unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "follow-up removed, recurring kept paused: {remaining:?}"
    );
    assert_eq!(remaining[0]["name"], json!("Morning Sweep"));
    assert_eq!(remaining[0]["enabled"], json!(false));

    fixture.services.database.close().await;
}

#[tokio::test]
async fn role_peers_and_role_send_collaborate_lazily_provision_threads_and_preserve_boss_news() {
    let fixture = fixture().await;
    let assistants: Vec<(String, String)> = sqlx::query_as(
        "SELECT assistant_id, name FROM assistant_definitions WHERE source = 'builtin' ORDER BY assistant_id LIMIT 2",
    )
    .fetch_all(fixture.services.database.pool())
    .await
    .expect("two builtin assistants");
    assert_eq!(assistants.len(), 2);
    let (role_a_id, role_a_name) = &assistants[0];
    let (role_b_id, role_b_name) = &assistants[1];

    // Point Role B at the seeded builtin `claude` ACP agent so `ensure_role_thread`
    // can materialize Role B's direct conversation in the in-memory DB without
    // needing external model provider credentials.
    sqlx::query("UPDATE assistant_definitions SET agent_id = 'claude' WHERE assistant_id = ?")
        .bind(role_b_id)
        .execute(fixture.services.database.pool())
        .await
        .unwrap();

    define_role_as(&fixture, role_a_id, &[]).await;
    define_role_as(&fixture, role_b_id, &[]).await;

    let token_a = conversation_as(&fixture, "conv-role-a", Some(role_a_id)).await;

    // 1. `GET /api/runtime/role/peers` lists Role B (excluding self Role A), initially without a thread.
    let (status, peers_before) = call(
        &fixture,
        "GET",
        "/api/runtime/role/peers",
        "conv-role-a",
        &token_a,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{peers_before}");
    let peer_arr = peers_before["data"]["peers"].as_array().unwrap();
    assert_eq!(peer_arr.len(), 1, "{peer_arr:?}");
    assert_eq!(peer_arr[0]["role_id"], json!(role_b_id));
    assert_eq!(peer_arr[0]["name"], json!(role_b_name));
    assert!(peer_arr[0].get("thread_conversation_id").is_none());

    // 2. Validation bad paths: self-send, missing peer, ambiguous target.
    let (status, self_err) = call(
        &fixture,
        "POST",
        "/api/runtime/role/send",
        "conv-role-a",
        &token_a,
        Some(json!({ "to_role": role_a_id, "message": "talk to myself" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{self_err}");
    assert_eq!(self_err["error"]["code"], json!("target_is_self"));

    let (status, missing_err) = call(
        &fixture,
        "POST",
        "/api/runtime/role/send",
        "conv-role-a",
        &token_a,
        Some(json!({ "to_role": "ghost-role", "message": "hello?" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing_err}");
    assert_eq!(missing_err["error"]["code"], json!("target_not_found"));

    let (status, schema_err) = call(
        &fixture,
        "POST",
        "/api/runtime/role/send",
        "conv-role-a",
        &token_a,
        Some(json!({ "to_role": role_b_id, "to_conversation": "conv-x", "message": "both?" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{schema_err}");
    assert_eq!(schema_err["error"]["code"], json!("schema_validation_failed"));

    // 3. `POST /api/runtime/role/send` lazily provisions Role B's thread and delivers `from_role` + `hop: 1/4`.
    let (status, sent) = call(
        &fixture,
        "POST",
        "/api/runtime/role/send",
        "conv-role-a",
        &token_a,
        Some(json!({ "to_role": role_b_id, "message": "请协助审查鉴权改动" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let b_thread_id = sent["data"]["to_conversation_id"].as_str().unwrap().to_owned();
    assert_eq!(sent["data"]["to_role_id"], json!(role_b_id));
    assert_eq!(sent["data"]["hop"], json!(1));
    assert_eq!(sent["data"]["max_hops"], json!(4));
    assert_eq!(
        fixture.services.role_service.thread(role_b_id).await.unwrap(),
        Some(b_thread_id.clone()),
        "Role B must claim the auto-provisioned conversation as its unique thread"
    );

    // Verify delivered message carries `from_role:` and `hop: 1/4`.
    let page = fixture
        .services
        .conversation_repo
        .list_messages_page(
            USER,
            &b_thread_id,
            &aionui_db::MessagePageParams {
                limit: 10,
                direction: aionui_db::MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    let user_msg = page
        .items
        .iter()
        .rfind(|m| m.position.as_deref() == Some("right"))
        .expect("delivered message persisted in Role B thread");
    let text = serde_json::from_str::<Value>(&user_msg.content).unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        text.contains(&format!("from_role: {role_a_id}\t{role_a_name}\n")),
        "{text}"
    );
    assert!(text.contains("hop: 1/4\n"), "{text}");

    // 4. Record an unread scheduled-run report on Role B, send a second colleague message,
    //    and confirm Role B's unread `<role-news>` was NOT consumed by the colleague turn!
    let now_after = aionui_common::now_ms() + 5_000;
    fixture
        .services
        .role_service
        .record_report(
            role_b_id,
            aionui_api_types::RoleReportOutcome::Say,
            "巡检发现证书将在 7 天后到期",
            "conv-cron-run-b",
            now_after,
        )
        .await
        .unwrap();

    // `resolve_role_mentions` (`@@@`) reuses the exact same `b_thread_id`!
    let mentioned = fixture
        .services
        .conversation_service
        .resolve_role_mentions(
            USER,
            "conv-role-a",
            "请跟进证书续期",
            &[aionui_api_types::RoleRef { id: role_b_id.clone() }],
            Some("/w/caller"),
        )
        .await
        .unwrap();
    assert!(mentioned.contains("[[AION_ROLES]]"), "{mentioned}");
    assert!(
        mentioned.contains(&format!("{role_b_name}\t{role_b_id}\t{b_thread_id}\tworkspace: ")),
        "{mentioned}"
    );

    // Unread report is STILL waiting for the human user (`open_thread_turn` returns 1 report)!
    let turn = fixture
        .services
        .role_service
        .open_thread_turn(role_b_id, &b_thread_id, false, now_after + 1_000)
        .await
        .unwrap();
    match turn {
        aionui_role::ThreadTurn::Thread { reports, .. } => {
            assert_eq!(reports.len(), 1, "colleague turn must not consume boss <role-news>");
            assert_eq!(reports[0].summary, "巡检发现证书将在 7 天后到期");
        }
        other => panic!("expected Thread, got {other:?}"),
    }

    // 5. Denying `role_send` on Role A immediately blocks outbound `role send` with 403 `permission_denied`.
    define_role_as(&fixture, role_a_id, &["role_send"]).await;
    let (status, denied_err) = call(
        &fixture,
        "POST",
        "/api/runtime/role/send",
        "conv-role-a",
        &token_a,
        Some(json!({ "to_role": role_b_id, "message": "blocked outreach" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied_err}");
    assert_eq!(denied_err["error"]["code"], json!("permission_denied"));

    fixture.services.database.close().await;
}

#[tokio::test]
async fn role_runtime_switch_syncs_existing_thread_session_and_cron_jobs_and_heals_startup_drift() {
    let mut fixture = fixture().await;
    let (token, csrf) = common::setup_and_login(&mut fixture.app, &fixture.services, "admin", "Passw0rd!234").await;

    // 1. Create a user assistant bound to Antigravity (`a9f3c21e`) with a fixed model.
    let create_resp = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "POST",
            "/api/assistants",
            json!({
                "id": "custom-growth-lead",
                "name": "出海情报与增长巡检员",
                "agent_id": "a9f3c21e",
                "defaults": {
                    "model": { "mode": "fixed", "value": "gemini-4-hydrogen-high" }
                }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::CREATED);

    // Promote it to a resident role.
    let role_resp = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "PUT",
            "/api/roles/custom-growth-lead",
            json!({
                "charter": "负责独立站 SEO 与增长巡检。",
                "denied_tools": [],
                "verifies": false,
                "asks_when_scheduled": false
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(role_resp.status(), StatusCode::OK);

    // Provision its unique thread conversation (`ensure_role_thread`) and record a backend resume anchor
    // so we can verify switching agents drops the incompatible anchor while same-agent model switch keeps it.
    let (_, thread_row) = fixture
        .services
        .conversation_service
        .ensure_role_thread(USER, "custom-growth-lead")
        .await
        .unwrap();
    let thread_id = thread_row.id.clone();
    assert_eq!(thread_row.r#type, "antigravity");
    let initial_thread_extra: Value = serde_json::from_str(&thread_row.extra).unwrap();
    assert_eq!(
        initial_thread_extra["session_mode"],
        json!("yolo"),
        "autonomous resident role (`asks_when_scheduled == false`) must seed full-auto on thread creation"
    );

    let acp_repo = aionui_db::SqliteAcpSessionRepository::new(fixture.services.database.pool().clone());
    use aionui_db::IAcpSessionRepository;
    assert!(
        acp_repo
            .update_session_id_for_user(USER, &thread_id, "old-agy-session-anchor")
            .await
            .unwrap()
    );

    // Create a scheduled task bound to this resident role (`new_conversation` mode).
    let cron_resp = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "POST",
            "/api/cron/jobs",
            json!({
                "name": "每日 SEO 巡检",
                "schedule": { "kind": "cron", "expr": "0 9 * * *", "tz": "UTC" },
                "prompt": "执行每日 SEO 巡检",
                "conversation_id": "",
                "created_by": "user",
                "execution_mode": "new_conversation",
                "agent_config": {
                    "name": "出海情报与增长巡检员",
                    "assistant_id": "custom-growth-lead"
                }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    let cron_status = cron_resp.status();
    let cron_created = body_json(cron_resp).await;
    assert_eq!(cron_status, StatusCode::CREATED, "{cron_created}");
    let job_id = cron_created["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(cron_created["data"]["metadata"]["agent_config"]["mode"], json!("yolo"));

    // Also create a non-role assistant and conversation to ensure ordinary assistant snapshots stay immutable.
    let ordinary_asst_resp = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "POST",
            "/api/assistants",
            json!({
                "id": "custom-ordinary-helper",
                "name": "普通写作助手",
                "agent_id": "a9f3c21e",
                "defaults": {
                    "model": { "mode": "fixed", "value": "gemini-4-hydrogen-high" }
                }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(ordinary_asst_resp.status(), StatusCode::CREATED);
    let ordinary_conv = fixture
        .services
        .conversation_service
        .create(
            USER,
            serde_json::from_value(json!({
                "name": "普通对话",
                "assistant": { "id": "custom-ordinary-helper" },
                "extra": {}
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    // Security & bad-path checks on assistant runtime switch:
    // - State-changing PUT without CSRF token is rejected with 403 Forbidden.
    let missing_csrf_resp = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/assistants/custom-growth-lead")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(r#"{"agent_id":"2d23ff1c"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_csrf_resp.status(), StatusCode::FORBIDDEN);

    // - Invalid `agent_id` returns 400 and leaves the role thread on Antigravity.
    let bad_agent_resp = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "PUT",
            "/api/assistants/custom-growth-lead",
            json!({ "agent_id": "missing-agent-id" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(bad_agent_resp.status(), StatusCode::BAD_REQUEST);

    // 2. Subscribe to WebSocket event bus and switch the resident role from Antigravity (`a9f3c21e`)
    //    to Claude Code (`2d23ff1c`, `acp`) with a new fixed model (`claude-sonnet-4-6`).
    let mut ws_rx = fixture.services.event_bus.subscribe();
    let switch_resp = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "PUT",
            "/api/assistants/custom-growth-lead",
            json!({
                "agent_id": "2d23ff1c",
                "defaults": {
                    "model": { "mode": "fixed", "value": "claude-sonnet-4-6" }
                }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(switch_resp.status(), StatusCode::OK);

    // Verify `conversation.listChanged` WebSocket event was broadcast for `thread_id` scoped to `USER`.
    let mut saw_thread_updated_event = false;
    while let Ok(msg) = ws_rx.try_recv() {
        if msg.name == "conversation.listChanged"
            && msg.data["conversation_id"] == json!(thread_id)
            && msg.data["action"] == json!("updated")
        {
            assert_eq!(msg.data["user_id"], json!(USER));
            saw_thread_updated_event = true;
        }
    }
    assert!(
        saw_thread_updated_event,
        "switching a resident role's agent must emit conversation.listChanged(updated) for its thread"
    );

    // Clicking 「找它聊」 fetches `GET /api/conversations/{thread_id}` — verify it immediately reflects Claude!
    let get_conv_resp = fixture
        .app
        .clone()
        .oneshot(common::get_with_token(
            &format!("/api/conversations/{thread_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(get_conv_resp.status(), StatusCode::OK);
    let conv_json = body_json(get_conv_resp).await;
    assert_eq!(conv_json["data"]["type"], json!("acp"));
    assert_eq!(conv_json["data"]["extra"]["agent_id"], json!("2d23ff1c"));
    assert_eq!(conv_json["data"]["extra"]["backend"], json!("claude"));
    assert_eq!(
        conv_json["data"]["extra"]["current_model_id"],
        json!("claude-sonnet-4-6")
    );
    assert_eq!(
        conv_json["data"]["extra"]["session_mode"],
        json!("bypassPermissions"),
        "switching an autonomous role from Antigravity to Claude must map full-auto `yolo` -> `bypassPermissions`"
    );
    assert_eq!(
        conv_json["data"]["extra"]["current_mode_id"],
        json!("bypassPermissions")
    );
    assert_eq!(conv_json["data"]["assistant"]["backend"], json!("claude"));
    assert!(
        conv_json["data"]["extra"].get("preset_context").is_none(),
        "HTTP response must keep preset_context scrubbed"
    );
    let stored_thread_row = fixture
        .services
        .conversation_repo
        .get(USER, &thread_id)
        .await
        .unwrap()
        .unwrap();
    let stored_extra: Value = serde_json::from_str(&stored_thread_row.extra).unwrap();
    let preset_ctx = stored_extra["preset_context"].as_str().unwrap_or_default();
    assert!(
        preset_ctx.contains("## Your Standing Role"),
        "standing role instructions must stay intact in stored extra after runtime sync: {preset_ctx}"
    );
    let stored_snapshot = fixture
        .services
        .conversation_repo
        .get_assistant_snapshot(USER, &thread_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stored_snapshot.rules_content.contains("## Your Standing Role"),
        "standing role instructions must stay intact in conversation_assistant_snapshots"
    );

    // Verify `acp_session` updated `agent_id`, cleared the incompatible Antigravity `session_id`,
    // and seeded `current_model_id = "claude-sonnet-4-6"`.
    let acp_after_switch = acp_repo.get_for_user(USER, &thread_id).await.unwrap().unwrap();
    assert_eq!(acp_after_switch.agent_id, "2d23ff1c");
    assert_eq!(acp_after_switch.session_id, None);
    let runtime_state = acp_repo
        .load_runtime_state_for_user(USER, &thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runtime_state.current_model_id.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(runtime_state.current_mode_id.as_deref(), Some("bypassPermissions"));
    assert_eq!(
        stored_snapshot.resolved_permission_value.as_deref(),
        Some("bypassPermissions")
    );

    // Verify the scheduled Cron Job bound to this role also updated its full-auto `mode` (`bypassPermissions`)
    // and `model_id` (`claude-sonnet-4-6`).
    let cron_get_resp = fixture
        .app
        .clone()
        .oneshot(common::get_with_token(&format!("/api/cron/jobs/{job_id}"), &token))
        .await
        .unwrap();
    assert_eq!(cron_get_resp.status(), StatusCode::OK);
    let cron_after = body_json(cron_get_resp).await;
    assert_eq!(
        cron_after["data"]["metadata"]["agent_config"]["mode"],
        json!("bypassPermissions")
    );
    assert_eq!(
        cron_after["data"]["metadata"]["agent_config"]["model_id"],
        json!("claude-sonnet-4-6")
    );

    // Meanwhile, switching the ordinary non-role assistant does NOT mutate its historical conversation snapshot!
    let ord_switch = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "PUT",
            "/api/assistants/custom-ordinary-helper",
            json!({ "agent_id": "2d23ff1c" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(ord_switch.status(), StatusCode::OK);
    let ord_row = fixture
        .services
        .conversation_repo
        .get(USER, &ordinary_conv.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ord_row.r#type, "antigravity");

    // 3. Same-agent default-model switch preserves the live `acp_session.session_id` while updating the model.
    acp_repo
        .update_session_id_for_user(USER, &thread_id, "live-claude-session-1")
        .await
        .unwrap();
    let model_only_resp = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "PUT",
            "/api/assistants/custom-growth-lead",
            json!({
                "defaults": {
                    "model": { "mode": "fixed", "value": "claude-opus-4-7" }
                }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(model_only_resp.status(), StatusCode::OK);
    let acp_after_model = acp_repo.get_for_user(USER, &thread_id).await.unwrap().unwrap();
    assert_eq!(
        acp_after_model.session_id.as_deref(),
        Some("live-claude-session-1"),
        "switching default model on the same agent must keep the live session anchor"
    );
    let runtime_after_model = acp_repo
        .load_runtime_state_for_user(USER, &thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runtime_after_model.current_model_id.as_deref(), Some("claude-opus-4-7"));
    assert_eq!(
        runtime_after_model.current_mode_id.as_deref(),
        Some("bypassPermissions"),
        "switching default model on the same agent must preserve the thread's full-auto permission"
    );

    // 4. Simulate pre-existing DB drift (e.g. an offline or pre-fix edit setting `agent_id = '8e1acf31'` Codex)
    //    and rebuild the app router: startup drift reconciliation heals `thread_id` automatically!
    sqlx::query(
        "UPDATE assistant_definitions SET agent_id = '8e1acf31', default_model_mode = 'auto', default_model_value = NULL \
         WHERE assistant_id = 'custom-growth-lead'",
    )
    .execute(fixture.services.database.pool())
    .await
    .unwrap();
    let _restarted_router = create_router(&fixture.services).await.expect("rebuild router");
    let healed_row = fixture
        .services
        .conversation_repo
        .get(USER, &thread_id)
        .await
        .unwrap()
        .unwrap();
    let healed_extra: Value = serde_json::from_str(&healed_row.extra).unwrap();
    assert_eq!(healed_extra["agent_id"], json!("8e1acf31"));
    assert_eq!(healed_extra["backend"], json!("codex"));
    assert!(healed_extra.get("current_model_id").is_none());
    assert_eq!(
        healed_extra["session_mode"],
        json!("agent-full-access"),
        "startup drift heal onto Codex must map full-auto `bypassPermissions` -> `agent-full-access`"
    );

    // 5. Simulate wiped thread permission (matching live `8e34e0ba`) while `assistant_preferences`
    //    carries a stale `"plan"` from an unrelated conversation: startup reconciliation restores
    //    `agent-full-access` without letting `assistant_preferences` downgrade the role thread!
    sqlx::query(
        "UPDATE conversations SET extra = json_remove(extra, '$.session_mode', '$.current_mode_id') WHERE id = ?",
    )
    .bind(&thread_id)
    .execute(fixture.services.database.pool())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE conversation_assistant_snapshots SET resolved_permission_value = NULL WHERE conversation_id = ?",
    )
    .bind(&thread_id)
    .execute(fixture.services.database.pool())
    .await
    .unwrap();
    acp_repo
        .save_runtime_state_for_user(
            USER,
            &thread_id,
            &aionui_db::SaveRuntimeStateParams {
                current_mode_id: Some(None),
                config_selections_json: Some(Some("{}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    sqlx::query("UPDATE assistant_preferences SET last_permission_value = 'plan'")
        .execute(fixture.services.database.pool())
        .await
        .unwrap();
    let _restarted_again = create_router(&fixture.services).await.expect("rebuild router 2");
    let restored_row = fixture
        .services
        .conversation_repo
        .get(USER, &thread_id)
        .await
        .unwrap()
        .unwrap();
    let restored_extra: Value = serde_json::from_str(&restored_row.extra).unwrap();
    assert_eq!(restored_extra["session_mode"], json!("agent-full-access"));
    let restored_acp = acp_repo
        .load_runtime_state_for_user(USER, &thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored_acp.current_mode_id.as_deref(), Some("agent-full-access"));
    assert_eq!(restored_row.name_source.as_deref(), Some("role"));

    // 6. Resident role conversation name lock & single-source-of-truth sync:
    //    - Manual rename on `thread_id` via PATCH /api/conversations/{thread_id} is rejected (400),
    //      whereas renaming `ordinary_conv` succeeds (200).
    let reject_rename = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "PATCH",
            &format!("/api/conversations/{thread_id}"),
            json!({ "name": "随便改的名字" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(reject_rename.status(), StatusCode::BAD_REQUEST);

    let ok_ord_rename = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "PATCH",
            &format!("/api/conversations/{}", ordinary_conv.id),
            json!({ "name": "可自由改名的普通对话" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(ok_ord_rename.status(), StatusCode::OK);

    //    - Legacy frontend `name_source: "auto"` on `thread_id` is silently ignored without mutating `name`.
    let ignore_auto_rename = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "PATCH",
            &format!("/api/conversations/{thread_id}"),
            json!({ "name": "Derived auto title", "name_source": "auto" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(ignore_auto_rename.status(), StatusCode::OK);
    let after_auto = fixture
        .services
        .conversation_repo
        .get(USER, &thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(after_auto.name, "Derived auto title");

    //    - Renaming the role via PUT /api/assistants/{id} immediately updates `thread_id`'s name.
    let role_rename_resp = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "PUT",
            "/api/assistants/custom-growth-lead",
            json!({ "name": "出海情报总负责人" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(role_rename_resp.status(), StatusCode::OK);
    let after_role_rename = fixture
        .services
        .conversation_repo
        .get(USER, &thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_role_rename.name, "出海情报总负责人");
    assert_eq!(after_role_rename.name_source.as_deref(), Some("role"));

    //    - Polluted legacy DB title (`name_source = 'agent'`) is healed back to the role name on startup
    //      without bumping `updated_at`!
    let prev_updated_at = after_role_rename.updated_at;
    sqlx::query(
        "UPDATE conversations SET name = 'yipan.store SEO数据巡检：曝光升至50次', name_source = 'agent' WHERE id = ?",
    )
    .bind(&thread_id)
    .execute(fixture.services.database.pool())
    .await
    .unwrap();
    let _restarted_name_heal = create_router(&fixture.services).await.expect("rebuild router 3");
    let healed_name_row = fixture
        .services
        .conversation_repo
        .get(USER, &thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(healed_name_row.name, "出海情报总负责人");
    assert_eq!(healed_name_row.name_source.as_deref(), Some("role"));
    assert_eq!(healed_name_row.updated_at, prev_updated_at);

    // 7. Direct `POST /api/conversations` for a resident role converges onto its unique thread:
    //    - Repeating `POST /api/conversations` for `custom-growth-lead` (which already owns `thread_id`)
    //      idempotently returns `thread_id` instead of spawning an Aside chat in Scratchpad!
    let repeat_create_resp = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "POST",
            "/api/conversations",
            json!({
                "name": "https://github.com/Panniantong/Agent-Reach 这个 skill 你安装了么",
                "assistant": { "id": "custom-growth-lead" },
                "extra": {}
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(repeat_create_resp.status(), StatusCode::CREATED);
    let repeat_created = body_json(repeat_create_resp).await;
    assert_eq!(repeat_created["data"]["id"], json!(thread_id));
    assert_eq!(repeat_created["data"]["name"], json!("出海情报总负责人"));

    //    - Creating the very first direct conversation of a brand-new resident role via `/guid` (`POST /api/conversations`)
    //      claims it immediately as `thread_conversation_id` and locks `name = role_name`, `name_source = "role"`.
    let create_fresh_asst = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "POST",
            "/api/assistants",
            json!({
                "id": "custom-ecom-director",
                "name": "跨境电商总负责人",
                "agent_id": "a9f3c21e",
                "rules": { "content": "统筹跨境电商业务。" }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(create_fresh_asst.status(), StatusCode::CREATED);
    define_role_as(&fixture, "custom-ecom-director", &[]).await;

    let first_guid_resp = fixture
        .app
        .clone()
        .oneshot(common::json_with_token(
            "POST",
            "/api/conversations",
            json!({
                "name": "Agent-Reach 这个 skill 你安装了么",
                "assistant": { "id": "custom-ecom-director" },
                "extra": {}
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(first_guid_resp.status(), StatusCode::CREATED);
    let first_guid_json = body_json(first_guid_resp).await;
    let ecom_thread_id = first_guid_json["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(first_guid_json["data"]["name"], json!("跨境电商总负责人"));
    let ecom_row = fixture
        .services
        .conversation_repo
        .get(USER, &ecom_thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ecom_row.name, "跨境电商总负责人");
    assert_eq!(ecom_row.name_source.as_deref(), Some("role"));
    let roles_resp = fixture
        .app
        .clone()
        .oneshot(common::get_with_token("/api/roles", &token))
        .await
        .unwrap();
    assert_eq!(roles_resp.status(), StatusCode::OK);
    let roles_json = body_json(roles_resp).await;
    let ecom_role = roles_json["data"]["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("custom-ecom-director"))
        .expect("custom-ecom-director in /api/roles");
    assert_eq!(ecom_role["thread_conversation_id"], json!(ecom_thread_id));

    fixture.services.database.close().await;
}
