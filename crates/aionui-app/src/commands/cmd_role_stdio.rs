//! `aioncore mcp-role-stdio` subcommand: the `aioncore role` commands as an
//! MCP stdio server.
//!
//! Codex runs the shell commands an agent issues inside a sandbox that, in its
//! workspace tier, has no network, so `aioncore role report` cannot reach the
//! backend there and a role run's declaration is lost. Codex starts MCP
//! servers itself, outside that sandbox, so the same calls made as MCP tools
//! get through. Each tool posts to the route its `role` command uses, with the
//! same runtime headers, and returns the backend's envelope as it came.
//!
//! A conversation that is not a role is shown no tools: `tools/list` asks the
//! backend first.

use std::process::ExitCode;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ListToolsResult};
use rmcp::{schemars, service::ServiceExt, tool, tool_router, transport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::commands::cmd_role::RuntimeEnv;
use crate::commands::error::{CliBoundaryCode, CliBoundaryError, missing_env};

const SUBCOMMAND: &str = "mcp-role-stdio";

pub async fn run_role_stdio() -> ExitCode {
    let env = match RuntimeEnv::from_env() {
        Ok(env) => env,
        Err(name) => {
            let err = missing_env(SUBCOMMAND, name);
            eprintln!("{}", err.stderr_line());
            return err.exit_code();
        }
    };
    // The backend is on this machine: a proxy in the environment must not
    // carry the call elsewhere.
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let server = RoleStdioServer { env, client };

    match server.serve(transport::io::stdio()).await {
        Ok(peer) => {
            if peer.waiting().await.is_err() {
                let err = CliBoundaryError::new(
                    CliBoundaryCode::McpSessionEndedWithError,
                    SUBCOMMAND,
                    "MCP stdio session ended with an error",
                );
                eprintln!("{}", err.stderr_line());
                err.exit_code()
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(_) => {
            let err = CliBoundaryError::new(
                CliBoundaryCode::McpStdioServeFailed,
                SUBCOMMAND,
                "failed to start MCP stdio server",
            );
            eprintln!("{}", err.stderr_line());
            err.exit_code()
        }
    }
}

#[derive(Clone)]
struct RoleStdioServer {
    env: RuntimeEnv,
    client: reqwest::Client,
}

// No doc comment on the variants: schemars would turn the plain string enum
// into a `oneOf`, a shape agents read less reliably.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Outcome {
    Silent,
    Say,
    Ask,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReportParams {
    /// silent: nothing the user needs to hear. say: something they should know. ask: their answer is needed to go on.
    outcome: Outcome,
    /// The one line the user sees first, or the question they are asked. Required for say and ask; 300 characters at most.
    #[serde(default)]
    summary: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct FollowUpParams {
    /// What to pick up when woken, written for a reader who remembers nothing of this moment.
    note: String,
    /// A whole number of minutes from now, from 1 to 10080 (a week).
    after_minutes: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RememberParams {
    /// One thing worth keeping, kept as a single dated line.
    note: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProgressParams {
    /// A short name for the task; the same name replaces what was recorded before.
    task: String,
    /// What is done and what is left. Empty or absent closes the task.
    #[serde(default)]
    state: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CurateParams {
    /// The whole memory, tidied: what repeats merged, what no longer holds dropped.
    memory: String,
}

// No doc comment on the variants: schemars would turn the plain string enum
// into a `oneOf`, a shape agents read less reliably.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RecallScope {
    Thread,
    Runs,
    All,
}

fn deserialize_bound_opt<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<Value> = Option::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(Value::Number(num)) => Ok(Some(num.to_string())),
        Some(_) => Err(serde::de::Error::custom(
            "expected timestamp string or epoch ms integer",
        )),
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecallParams {
    /// Keywords or phrase to search across your thread with the user, your scheduled runs, and your thread summary leaves.
    query: String,
    /// Maximum hits to return, from 1 to 50 (default 10).
    #[serde(default)]
    limit: Option<u32>,
    /// thread: only your thread with the user and its leaves. runs: only scheduled runs. all: both (default).
    #[serde(default)]
    scope: Option<RecallScope>,
    /// Only hits strictly earlier than this moment (YYYY-MM-DD, YYYY-MM-DD HH:MM, RFC 3339, or epoch ms).
    #[serde(default, deserialize_with = "deserialize_bound_opt")]
    before: Option<String>,
    /// Only hits at or after this moment (YYYY-MM-DD, YYYY-MM-DD HH:MM, RFC 3339, or epoch ms).
    #[serde(default, deserialize_with = "deserialize_bound_opt")]
    after: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExpandParams {
    /// A ref_id returned by role_recall (msg:... or leaf:...).
    ref_id: String,
    /// Preceding messages to include around a message hit, from 0 to 20 (default 3).
    #[serde(default)]
    before: Option<u32>,
    /// Following messages to include around a message hit, from 0 to 20 (default 3).
    #[serde(default)]
    after: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RoleSendParams {
    /// Peer resident role id (lazily creates its thread when needed). Provide either `to_role` or `to_conversation`, not both.
    #[serde(default)]
    to_role: Option<String>,
    /// Target conversation id (e.g. `reply_to` from an incoming `[[AION_SESSION_MESSAGE]]`). Provide either `to_role` or `to_conversation`, not both.
    #[serde(default)]
    to_conversation: Option<String>,
    /// Concise, self-contained message body. Never use relative file paths across workspaces.
    message: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SessionListParams {
    /// Optional conversation name filter.
    #[serde(default)]
    q: Option<String>,
    /// Optional project id filter.
    #[serde(default)]
    project_id: Option<String>,
    /// Maximum targets to return (1..=50, default 20).
    #[serde(default)]
    limit: Option<u32>,
    /// Opaque pagination cursor from an earlier page.
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SessionSendParams {
    /// Target conversation id (`id` from `[[AION_SESSIONS]]`, `thread_conversation_id` from `[[AION_ROLES]]`, or `reply_to`). Never pass a name or `*`.
    to: String,
    /// Message body the target conversation receives.
    message: String,
}

#[tool_router]
impl RoleStdioServer {
    #[tool(
        name = "role_report",
        description = "Declare how a run nobody watched ended, once, before your final reply: silent (nothing the user needs to hear; the run asks for no attention), say (something they should know) or ask (you cannot go on without their answer). A run that declares nothing is shown to the user as if it had something to say.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    async fn report(&self, Parameters(params): Parameters<ReportParams>) -> CallToolResult {
        let mut body = json!({ "outcome": params.outcome });
        if let Some(summary) = params.summary {
            body["summary"] = json!(summary);
        }
        self.post("/api/runtime/role/report", body).await
    }

    #[tool(
        name = "role_follow_up",
        description = "Set yourself a follow-up for a matter that has to wait (a build, a reply, a date): the note comes back to this conversation once, after that many minutes.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    async fn follow_up(&self, Parameters(params): Parameters<FollowUpParams>) -> CallToolResult {
        self.post(
            "/api/runtime/role/follow-up",
            json!({ "note": params.note, "after_minutes": params.after_minutes }),
        )
        .await
    }

    #[tool(
        name = "role_remember",
        description = "Add one note to your resident role's memory: a preference of the user, a mistake not to repeat, a decision and its reason. One fact per call.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    async fn remember(&self, Parameters(params): Parameters<RememberParams>) -> CallToolResult {
        self.post("/api/runtime/role/remember", json!({ "note": params.note }))
            .await
    }

    #[tool(
        name = "role_progress",
        description = "Record where a piece of ongoing work stands, replacing what was recorded for that task before, so that a later run can pick it up. Leave the state out when the task is finished.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    async fn progress(&self, Parameters(params): Parameters<ProgressParams>) -> CallToolResult {
        let mut body = json!({ "task": params.task });
        if let Some(state) = params.state {
            body["state"] = json!(state);
        }
        self.post("/api/runtime/role/progress", body).await
    }

    #[tool(
        name = "role_curate",
        description = "Replace your resident role's whole memory with a tidied version. The version it replaces is archived.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    async fn curate(&self, Parameters(params): Parameters<CurateParams>) -> CallToolResult {
        self.post("/api/runtime/role/curate", json!({ "memory": params.memory }))
            .await
    }

    #[tool(
        name = "role_show",
        description = "Read your resident role whole: charter, memory, every task under way with its full state, and what you last told the user.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn show(&self) -> CallToolResult {
        self.send(self.client.get(self.env.url("/api/runtime/role"))).await
    }

    #[tool(
        name = "role_recall",
        description = "Search your resident role's past conversations: your one thread with the user, its summary leaves, and your scheduled runs. Returns compact snippets with ref_id handles you can pass to role_expand.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn recall(&self, Parameters(params): Parameters<RecallParams>) -> CallToolResult {
        let mut body = json!({ "query": params.query });
        if let Some(limit) = params.limit {
            body["limit"] = json!(limit);
        }
        if let Some(scope) = params.scope {
            body["scope"] = json!(scope);
        }
        if let Some(before) = params.before {
            body["before"] = json!(before);
        }
        if let Some(after) = params.after {
            body["after"] = json!(after);
        }
        self.post("/api/runtime/role/recall", body).await
    }

    #[tool(
        name = "role_expand",
        description = "Expand a ref_id returned by role_recall: reads the surrounding messages in order for a message hit, or the whole body and span of a summary leaf.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn expand(&self, Parameters(params): Parameters<ExpandParams>) -> CallToolResult {
        let mut body = json!({ "ref_id": params.ref_id });
        if let Some(before) = params.before {
            body["before"] = json!(before);
        }
        if let Some(after) = params.after {
            body["after"] = json!(after);
        }
        self.post("/api/runtime/role/expand", body).await
    }

    #[tool(
        name = "role_peers",
        description = "List other resident roles on the roster with their display name, charter summary, thread_conversation_id, and whether their thread is currently busy.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn peers(&self) -> CallToolResult {
        self.send(self.client.get(self.env.url("/api/runtime/role/peers")))
            .await
    }

    #[tool(
        name = "role_send",
        description = "Deliver a message either to a peer resident role (`to_role`, lazily creating its thread when needed) or to an explicit conversation (`to_conversation`, e.g. `reply_to` from `[[AION_SESSION_MESSAGE]]`). Reach out only across charter boundaries or when asked via `@@@`; never send courtesy acknowledgements.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    async fn role_send(&self, Parameters(params): Parameters<RoleSendParams>) -> CallToolResult {
        let mut body = json!({ "message": params.message });
        if let Some(to_role) = params.to_role {
            body["to_role"] = json!(to_role);
        }
        if let Some(to_conversation) = params.to_conversation {
            body["to_conversation"] = json!(to_conversation);
        }
        self.post("/api/runtime/role/send", body).await
    }

    #[tool(
        name = "session_list",
        description = "List conversations this conversation may deliver a message to. Use only when the user named a target conversation in prose instead of selecting it with `@@` or `@@@`.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn session_list(&self, Parameters(params): Parameters<SessionListParams>) -> CallToolResult {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(q) = params.q {
            query.push(("q", q));
        }
        if let Some(project_id) = params.project_id {
            query.push(("project_id", project_id));
        }
        if let Some(limit) = params.limit {
            query.push(("limit", limit.to_string()));
        }
        if let Some(cursor) = params.cursor {
            query.push(("cursor", cursor));
        }
        self.send(
            self.client
                .get(self.env.url("/api/runtime/session-messages/targets"))
                .query(&query),
        )
        .await
    }

    #[tool(
        name = "session_send_message",
        description = "Deliver a message to another conversation (`to` = conversation id from `[[AION_SESSIONS]]`, `thread_conversation_id` from `[[AION_ROLES]]`, or `reply_to` from `[[AION_SESSION_MESSAGE]]`). Reaches the backend even inside sandboxes where shell networking is disabled.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    async fn session_send_message(&self, Parameters(params): Parameters<SessionSendParams>) -> CallToolResult {
        self.post(
            "/api/runtime/session-messages/send",
            json!({ "to": params.to, "message": params.message }),
        )
        .await
    }
}

fn select_exposed_tools(
    all_tools: Vec<rmcp::model::Tool>,
    role_denied_tools: Option<&[String]>,
    can_send_cross_session: bool,
) -> Vec<rmcp::model::Tool> {
    match role_denied_tools {
        Some(denied) => {
            let role_send_denied = denied.iter().any(|t| t == aionui_api_types::ROLE_SEND_TOOL_NAME);
            all_tools
                .into_iter()
                .filter(|tool| {
                    let name = tool.name.as_ref();
                    if name.starts_with("session_") {
                        return false;
                    }
                    if name == "role_peers" {
                        return can_send_cross_session;
                    }
                    if name == "role_send" {
                        return can_send_cross_session && !role_send_denied;
                    }
                    true
                })
                .collect()
        }
        None => {
            if !can_send_cross_session {
                Vec::new()
            } else {
                all_tools
                    .into_iter()
                    .filter(|tool| tool.name.starts_with("session_"))
                    .collect()
            }
        }
    }
}

#[rmcp::tool_handler(router = Self::tool_router())]
impl rmcp::ServerHandler for RoleStdioServer {
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let (role_denied, can_send) = tokio::join!(self.role_denied_tools(), self.can_send_cross_session());
        let tools = select_exposed_tools(Self::tool_router().list_all(), role_denied.as_deref(), can_send);
        Ok(ListToolsResult::with_all_items(tools))
    }
}

impl RoleStdioServer {
    /// When this conversation runs as a resident role, returns `Some(denied_tools)`.
    /// Otherwise `None`.
    async fn role_denied_tools(&self) -> Option<Vec<String>> {
        let headers = self.env.header_map().ok()?;
        let response = self
            .client
            .get(self.env.url("/api/runtime/role"))
            .headers(headers)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: Value = response.json().await.ok()?;
        let denied = body
            .get("data")
            .and_then(|d| d.get("denied_tools"))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Some(denied)
    }

    /// True when cross-session messaging is enabled and this conversation is
    /// not team-owned (`GET /api/runtime/session-messages/targets?limit=1`).
    async fn can_send_cross_session(&self) -> bool {
        let Ok(headers) = self.env.header_map() else {
            return false;
        };
        match self
            .client
            .get(self.env.url("/api/runtime/session-messages/targets?limit=1"))
            .headers(headers)
            .send()
            .await
        {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }

    async fn post(&self, path: &str, body: Value) -> CallToolResult {
        self.send(self.client.post(self.env.url(path)).json(&body)).await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> CallToolResult {
        let headers = match self.env.header_map() {
            Ok(headers) => headers,
            // The NAME only — a token must never reach the agent.
            Err(name) => return failure(format!("environment variable for {name} is not a valid header value")),
        };
        let response = match request.headers(headers).send().await {
            Ok(response) => response,
            Err(error) => return failure(format!("the backend could not be reached: {error}")),
        };
        let succeeded = response.status().is_success();
        match response.text().await {
            // The body is the backend's envelope either way; a failure carries its stable error code.
            Ok(text) if succeeded => CallToolResult::success(vec![Content::text(text)]),
            Ok(text) => CallToolResult::error(vec![Content::text(text)]),
            Err(error) => failure(format!("the backend's answer could not be read: {error}")),
        }
    }
}

fn failure(message: String) -> CallToolResult {
    CallToolResult::error(vec![Content::text(
        json!({ "success": false, "error": { "code": "transport_unavailable", "message": message } }).to_string(),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_and_session_bridge_command_is_registered_and_filtered_by_context() {
        let all = RoleStdioServer::tool_router().list_all();
        let names: Vec<String> = all.iter().map(|tool| tool.name.to_string()).collect();
        for name in [
            "role_report",
            "role_follow_up",
            "role_remember",
            "role_progress",
            "role_curate",
            "role_show",
            "role_recall",
            "role_expand",
            "role_peers",
            "role_send",
            "session_list",
            "session_send_message",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} missing from {names:?}");
        }
        assert_eq!(names.len(), 12);

        // Resident role outside team: gets all 10 role_* tools, no session_* duplicates.
        let role_open: Vec<String> = select_exposed_tools(all.clone(), Some(&[]), true)
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(role_open.len(), 10);
        assert!(role_open.iter().any(|n| n == "role_peers"));
        assert!(role_open.iter().any(|n| n == "role_send"));
        assert!(role_open.iter().all(|n| !n.starts_with("session_")));

        // Resident role with role_send denied: keeps role_peers, drops role_send.
        let role_denied: Vec<String> = select_exposed_tools(all.clone(), Some(&["role_send".into()]), true)
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(role_denied.iter().any(|n| n == "role_peers"));
        assert!(!role_denied.iter().any(|n| n == "role_send"));

        // Resident role inside a team (`can_send_cross_session = false`): drops both role_peers and role_send.
        let role_in_team: Vec<String> = select_exposed_tools(all.clone(), Some(&[]), false)
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(role_in_team.len(), 8);
        assert!(!role_in_team.iter().any(|n| n == "role_peers" || n == "role_send"));

        // Ordinary non-team conversation: exposes session_list + session_send_message for offline sandboxes.
        let ordinary_open: Vec<String> = select_exposed_tools(all.clone(), None, true)
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(ordinary_open, vec!["session_list", "session_send_message"]);

        // Ordinary conversation inside team or feature disabled: empty.
        assert!(select_exposed_tools(all, None, false).is_empty());
    }

    #[test]
    fn recall_params_accept_string_or_integer_bounds() {
        let p: RecallParams = serde_json::from_value(json!({
            "query": "数据源核对",
            "scope": "runs",
            "after": "2026-04-01",
            "before": 1_780_000_000_000_i64
        }))
        .unwrap();
        assert_eq!(p.after.as_deref(), Some("2026-04-01"));
        assert_eq!(p.before.as_deref(), Some("1780000000000"));
    }

    #[test]
    fn a_report_takes_one_of_the_three_outcomes_and_nothing_else() {
        let silent: ReportParams = serde_json::from_value(json!({ "outcome": "silent" })).unwrap();
        assert_eq!(json!(silent.outcome), json!("silent"));
        assert!(serde_json::from_value::<ReportParams>(json!({ "outcome": "loud" })).is_err());
        assert!(serde_json::from_value::<ReportParams>(json!({ "outcome": "say", "note": "x" })).is_err());
    }
}
