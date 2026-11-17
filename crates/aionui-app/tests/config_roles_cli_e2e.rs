//! E2E coverage for `aioncore config roles`, the commands an agent uses to hire
//! a resident role for the user, against a fake backend that keeps one role.

use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;

#[derive(Debug, Default)]
struct FakeRoles {
    role: Option<Value>,
    /// Every definition body `PUT /api/roles/{id}` received, in order.
    puts: Vec<Value>,
}

type SharedRoles = Arc<Mutex<FakeRoles>>;

async fn fake_role_list(State(roles): State<SharedRoles>) -> axum::Json<Value> {
    let roles = roles.lock().unwrap();
    let listed: Vec<Value> = roles.role.iter().cloned().collect();
    axum::Json(json!({ "success": true, "data": { "roles": listed } }))
}

async fn fake_role_get(State(roles): State<SharedRoles>, Path(id): Path<String>) -> (StatusCode, axum::Json<Value>) {
    let roles = roles.lock().unwrap();
    match &roles.role {
        Some(role) if role["id"] == id.as_str() => {
            (StatusCode::OK, axum::Json(json!({ "success": true, "data": role })))
        }
        _ => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "success": false, "code": "role_not_found" })),
        ),
    }
}

/// Like the real route: replaces the definition and keeps the memory.
async fn fake_role_put(
    State(roles): State<SharedRoles>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    let mut roles = roles.lock().unwrap();
    let memory = roles
        .role
        .as_ref()
        .and_then(|role| role.get("memory").cloned())
        .unwrap_or_else(|| json!(""));
    let role = json!({
        "id": id,
        "charter": body.get("charter").cloned().unwrap_or_else(|| json!("")),
        "denied_tools": body.get("denied_tools").cloned().unwrap_or_else(|| json!([])),
        "verifies": body.get("verifies").cloned().unwrap_or(json!(false)),
        "asks_when_scheduled": body.get("asks_when_scheduled").cloned().unwrap_or(json!(false)),
        "memory": memory,
        "memory_dir": "/data/roles/memory",
        "progress": [],
        "reported": []
    });
    roles.puts.push(body);
    roles.role = Some(role.clone());
    axum::Json(json!({ "success": true, "data": role }))
}

async fn fake_role_memory_put(
    State(roles): State<SharedRoles>,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    let mut roles = roles.lock().unwrap();
    let role = roles
        .role
        .as_mut()
        .expect("memory is only seeded after the role is saved");
    role["memory"] = body["memory"].clone();
    axum::Json(json!({ "success": true, "data": role.clone() }))
}

async fn fake_assistant_get(Path(id): Path<String>) -> axum::Json<Value> {
    axum::Json(json!({ "success": true, "data": { "id": id, "name": "Build Watcher" } }))
}

async fn spawn_role_server(roles: SharedRoles) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/api/roles", get(fake_role_list))
        .route("/api/roles/{id}", get(fake_role_get).put(fake_role_put))
        .route("/api/roles/{id}/memory", put(fake_role_memory_put))
        .route("/api/assistants/{id}", get(fake_assistant_get))
        .with_state(roles);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

async fn run_config(base_url: &str, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aioncore"))
        .arg("config")
        .args(args)
        .env("AIONUI_BASE_URL", base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-hiring")
        .env("AIONUI_USER_ID", "user-hiring")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(stdin.as_bytes()).await.unwrap();
    drop(child.stdin.take());
    child.wait_with_output().await.unwrap()
}

fn stdout_json(output: &Output, what: &str) -> Value {
    assert!(
        output.status.success(),
        "{what} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn an_agent_hires_a_role_then_changes_one_switch_without_losing_the_rest() {
    let roles: SharedRoles = Arc::new(Mutex::new(FakeRoles::default()));
    let (base_url, handle) = spawn_role_server(roles.clone()).await;

    let created = run_config(
        &base_url,
        &["roles", "create"],
        r#"{ "role_id": "asst-1", "charter": "Watch the nightly build", "denied_tools": ["team_spawn_agent"], "asks_when_scheduled": true, "memory": "- seed" }"#,
    )
    .await;
    let created = stdout_json(&created, "roles create");
    assert_eq!(created["data"]["role_id"], "asst-1");
    assert_eq!(created["data"]["charter"], "Watch the nightly build");
    assert_eq!(created["data"]["memory_chars"], 6);
    assert!(
        created["data"].get("memory").is_none(),
        "the memory text is not printed"
    );
    assert_eq!(created["meta"]["memory_seeded"], true);
    assert_eq!(created["meta"]["before"], Value::Null);

    let updated = run_config(
        &base_url,
        &["roles", "update"],
        r#"{ "role_id": "asst-1", "verifies": true }"#,
    )
    .await;
    let updated = stdout_json(&updated, "roles update");
    assert_eq!(updated["meta"]["before"]["verifies"], false);
    assert_eq!(updated["meta"]["after"]["verifies"], true);

    let again = run_config(
        &base_url,
        &["roles", "create"],
        r#"{ "role_id": "asst-1", "charter": "Other" }"#,
    )
    .await;
    let wiped = run_config(
        &base_url,
        &["roles", "update"],
        r#"{ "role_id": "asst-1", "memory": "" }"#,
    )
    .await;

    handle.abort();
    let roles = roles.lock().unwrap();
    let first = &roles.puts[0];
    assert!(first.get("memory").is_none(), "the definition carries no memory");
    let last = roles.puts.last().unwrap();
    assert_eq!(last["charter"], "Watch the nightly build");
    assert_eq!(last["denied_tools"], json!(["team_spawn_agent"]));
    assert_eq!(last["asks_when_scheduled"], true);
    assert_eq!(last["verifies"], true);
    assert_eq!(roles.puts.len(), 2, "neither refused command wrote");
    assert_eq!(roles.role.as_ref().unwrap()["memory"], "- seed");

    assert_eq!(again.status.code(), Some(2));
    let again_err = String::from_utf8_lossy(&again.stderr);
    assert!(again_err.contains("CONFIG_PAYLOAD_INVALID"), "{again_err}");
    assert!(again_err.contains("field=\"role_id\""), "{again_err}");
    assert_eq!(wiped.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&wiped.stderr).contains("field=\"memory\""));
}

#[tokio::test]
async fn the_roles_commands_are_advertised() {
    let output = Command::new(env!("CARGO_BIN_EXE_aioncore"))
        .args(["config", "capabilities"])
        .output()
        .await
        .unwrap();
    let stdout = stdout_json(&output, "config capabilities");
    let commands: Vec<String> = stdout["data"]["domains"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|domain| domain["commands"].as_array().into_iter().flatten())
        .filter_map(|command| command["command"].as_str().map(str::to_owned))
        .collect();
    for expected in [
        "config roles list",
        "config roles get",
        "config roles create",
        "config roles update",
        "config roles delete",
    ] {
        assert!(commands.iter().any(|command| command == expected), "{expected} missing");
    }
}
