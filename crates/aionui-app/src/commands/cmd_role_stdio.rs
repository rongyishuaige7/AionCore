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
}

#[rmcp::tool_handler(router = Self::tool_router())]
impl rmcp::ServerHandler for RoleStdioServer {
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let tools = if self.runs_as_a_role().await {
            Self::tool_router().list_all()
        } else {
            Vec::new()
        };
        Ok(ListToolsResult::with_all_items(tools))
    }
}

impl RoleStdioServer {
    /// Whether this conversation runs as a resident role. A backend that cannot
    /// say counts as no: tools that could only fail are left out.
    async fn runs_as_a_role(&self) -> bool {
        let Ok(headers) = self.env.header_map() else {
            return false;
        };
        match self
            .client
            .get(self.env.url("/api/runtime/role"))
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
    fn every_role_command_that_writes_or_reads_is_a_tool() {
        let names: Vec<String> = RoleStdioServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        for name in [
            "role_report",
            "role_follow_up",
            "role_remember",
            "role_progress",
            "role_curate",
            "role_show",
            "role_recall",
            "role_expand",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} missing from {names:?}");
        }
        assert_eq!(names.len(), 8);
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
