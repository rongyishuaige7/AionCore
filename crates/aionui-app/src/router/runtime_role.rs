//! What a resident role may read and write about itself from inside the
//! conversation it is running in — the `aioncore role` helper's backend.
//!
//! A team member has `team_remember` and `team_progress`; a role run on its
//! own (a scheduled task, a conversation with the user) has no team tools, and
//! these routes are how it keeps its memory there. The caller never names a
//! role: the token-bound conversation decides which assistant is speaking, and
//! that assistant has to be a resident role. The role's permission table
//! applies here as it does in a team.
//!
//! A role can also set itself a follow-up here: a one-shot timer that brings a
//! note back to this conversation later, so a matter it cannot finish now is
//! not left to chance.
//!
//! Authenticates on the three `x-aionui-*` headers itself, like the other
//! runtime routes, and is mounted outside the ordinary auth and CSRF middleware.

use std::sync::Arc;

use aionui_ai_agent::{RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_api_types::{
    RoleCliEnvelope, RoleCliErrorCode, RoleCurateRequest, RoleCurateResponse, RoleFollowUpRequest,
    RoleFollowUpResponse, RoleProgressRequest, RoleProgressResponse, RoleRememberRequest, RoleRememberResponse,
    RoleResponse, TeamToolName,
};
use aionui_cron::error::CronError;
use aionui_cron::service::CronService;
use aionui_db::IConversationRepository;
use aionui_role::{RoleError, RoleService};
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::Value;

const HEADER_USER_ID: &str = "x-aionui-user-id";
const HEADER_CONVERSATION_ID: &str = "x-aionui-conversation-id";
const HEADER_RUNTIME_TOKEN: &str = "x-aionui-runtime-token";

#[derive(Clone)]
pub struct RuntimeRoleState {
    pub roles: Arc<RoleService>,
    /// Where a role's follow-ups wait until they are due.
    pub timers: Arc<CronService>,
    pub conversations: Arc<dyn IConversationRepository>,
    pub runtime_token_service: Arc<RuntimeTokenService>,
}

pub fn runtime_role_routes(state: RuntimeRoleState) -> Router {
    Router::new()
        .route("/api/runtime/role", get(show))
        .route("/api/runtime/role/remember", post(remember))
        .route("/api/runtime/role/curate", post(curate))
        .route("/api/runtime/role/progress", post(progress))
        .route("/api/runtime/role/follow-up", post(follow_up))
        .with_state(state)
}

type Reply = (StatusCode, Json<RoleCliEnvelope<Value>>);

/// The role with its whole memory and the full state of every task under way —
/// the detail the standing-role section only summarizes.
async fn show(State(state): State<RuntimeRoleState>, headers: HeaderMap) -> Reply {
    const COMMAND: &str = "role show";
    match caller_role(&state, &headers, COMMAND).await {
        Ok(role) => success(COMMAND, &role),
        Err(reply) => reply,
    }
}

async fn remember(
    State(state): State<RuntimeRoleState>,
    headers: HeaderMap,
    body: Result<Json<RoleRememberRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "role remember";
    let role = match caller_role(&state, &headers, COMMAND).await {
        Ok(role) => role,
        Err(reply) => return reply,
    };
    let request = match parsed(body, COMMAND) {
        Ok(request) => request,
        Err(reply) => return *reply,
    };
    if is_denied(&role, TeamToolName::TeamRemember) {
        return failure(
            StatusCode::FORBIDDEN,
            RoleCliErrorCode::PermissionDenied,
            "the user keeps this role's memory; say in your reply what should be remembered",
            COMMAND,
        );
    }
    // Notes carry the day they were written, in the user's own time zone.
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    match state.roles.remember(&role.id, &request.note, &today).await {
        Ok(noted) => success(
            COMMAND,
            &RoleRememberResponse {
                line: noted.line,
                already_recorded: noted.already_recorded,
            },
        ),
        Err(error) => role_failure(error, COMMAND),
    }
}

/// Replace the whole memory with a tidied version; the one it replaces is
/// archived. Writing the memory is one permission, whether a note at a time or
/// all of it.
async fn curate(
    State(state): State<RuntimeRoleState>,
    headers: HeaderMap,
    body: Result<Json<RoleCurateRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "role curate";
    let role = match caller_role(&state, &headers, COMMAND).await {
        Ok(role) => role,
        Err(reply) => return reply,
    };
    let request = match parsed(body, COMMAND) {
        Ok(request) => request,
        Err(reply) => return *reply,
    };
    if is_denied(&role, TeamToolName::TeamRemember) {
        return failure(
            StatusCode::FORBIDDEN,
            RoleCliErrorCode::PermissionDenied,
            "the user keeps this role's memory; say in your reply what you would change in it",
            COMMAND,
        );
    }
    // The replaced memory is filed under the moment it was replaced, in the
    // user's own time zone.
    let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string();
    match state.roles.curate_memory(&role.id, &request.memory, &stamp).await {
        Ok(curated) => success(
            COMMAND,
            &RoleCurateResponse {
                archived_as: curated.archived_as,
                bytes_before: curated.bytes_before,
                bytes_after: curated.bytes_after,
            },
        ),
        Err(error) => role_failure(error, COMMAND),
    }
}

async fn progress(
    State(state): State<RuntimeRoleState>,
    headers: HeaderMap,
    body: Result<Json<RoleProgressRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "role progress";
    let role = match caller_role(&state, &headers, COMMAND).await {
        Ok(role) => role,
        Err(reply) => return reply,
    };
    let request = match parsed(body, COMMAND) {
        Ok(request) => request,
        Err(reply) => return *reply,
    };
    if is_denied(&role, TeamToolName::TeamProgress) {
        return failure(
            StatusCode::FORBIDDEN,
            RoleCliErrorCode::PermissionDenied,
            "the user keeps this role's task progress; say in your reply where the task stands",
            COMMAND,
        );
    }
    match state.roles.set_progress(&role.id, &request.task, &request.state).await {
        Ok(update) => success(
            COMMAND,
            &RoleProgressResponse {
                task: update.task,
                closed: update.closed,
            },
        ),
        Err(error) => role_failure(error, COMMAND),
    }
}

/// Set a follow-up for the calling conversation. Open to every role: it
/// touches nothing the user keeps, and what it wakes is the role itself.
async fn follow_up(
    State(state): State<RuntimeRoleState>,
    headers: HeaderMap,
    body: Result<Json<RoleFollowUpRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "role follow-up";
    let (caller, _role) = match caller_and_role(&state, &headers, COMMAND).await {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let request = match parsed(body, COMMAND) {
        Ok(request) => request,
        Err(reply) => return *reply,
    };
    match state
        .timers
        .create_follow_up(
            &caller.user_id,
            &caller.conversation_id,
            &request.note,
            request.after_minutes,
        )
        .await
    {
        Ok(set) => success(
            COMMAND,
            &RoleFollowUpResponse {
                job_id: set.job_id,
                due_at: set.due_at,
                pending: set.pending,
            },
        ),
        Err(CronError::FollowUpRefused(message)) => failure(
            StatusCode::BAD_REQUEST,
            RoleCliErrorCode::SchemaValidationFailed,
            message,
            COMMAND,
        ),
        Err(error) => {
            tracing::error!(command = COMMAND, error = %error, "role helper: follow-up could not be set");
            internal(COMMAND)
        }
    }
}

/// The resident role the calling conversation runs as. Refuses a caller whose
/// token does not open the conversation it names, and a conversation whose
/// assistant is not a role.
async fn caller_role(state: &RuntimeRoleState, headers: &HeaderMap, command: &str) -> Result<RoleResponse, Reply> {
    caller_and_role(state, headers, command).await.map(|(_, role)| role)
}

async fn caller_and_role(
    state: &RuntimeRoleState,
    headers: &HeaderMap,
    command: &str,
) -> Result<(RuntimeCaller, RoleResponse), Reply> {
    let caller = runtime_caller(state, headers).ok_or_else(|| {
        // No header values are logged — the token must never reach the logs.
        tracing::warn!(command, error_code = "runtime_auth_failed", "role helper refused");
        failure(
            StatusCode::UNAUTHORIZED,
            RoleCliErrorCode::RuntimeAuthFailed,
            "runtime auth failed",
            command,
        )
    })?;
    let not_a_role = || {
        failure(
            StatusCode::NOT_FOUND,
            RoleCliErrorCode::NotARole,
            "this conversation does not run as a resident role",
            command,
        )
    };
    let snapshot = state
        .conversations
        .get_assistant_snapshot(&caller.user_id, &caller.conversation_id)
        .await
        .map_err(|error| {
            tracing::error!(command, error = %error, "role helper: assistant lookup failed");
            internal(command)
        })?
        .ok_or_else(not_a_role)?;
    match state.roles.find(&snapshot.assistant_id).await {
        Ok(Some(role)) => Ok((caller, role)),
        // An assistant id that cannot name a role directory is not a role.
        Ok(None) | Err(RoleError::InvalidId(_)) => Err(not_a_role()),
        Err(error) => {
            tracing::error!(command, error = %error, "role helper: role lookup failed");
            Err(internal(command))
        }
    }
}

struct RuntimeCaller {
    user_id: String,
    conversation_id: String,
}

fn runtime_caller(state: &RuntimeRoleState, headers: &HeaderMap) -> Option<RuntimeCaller> {
    let user_id = required_header(headers, HEADER_USER_ID)?;
    let conversation_id = required_header(headers, HEADER_CONVERSATION_ID)?;
    let token = required_header(headers, HEADER_RUNTIME_TOKEN)?;
    state
        .runtime_token_service
        .validate(
            Some(&token),
            &user_id,
            &conversation_id,
            RuntimeTokenScope::ConversationHelper,
            // Every conversation's helper token is issued with this constant;
            // despite the prefix it is not team-specific.
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        )
        .ok()?;
    Some(RuntimeCaller {
        user_id,
        conversation_id,
    })
}

fn required_header(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The request body, parsed only after the caller is known to be a role so
/// that anyone else learns nothing about the schema.
fn parsed<T>(body: Result<Json<T>, JsonRejection>, command: &str) -> Result<T, Box<Reply>> {
    body.map(|Json(request)| request).map_err(|rejection| {
        Box::new(failure(
            StatusCode::BAD_REQUEST,
            RoleCliErrorCode::SchemaValidationFailed,
            format!("request body does not match the schema: {}", rejection.body_text()),
            command,
        ))
    })
}

fn is_denied(role: &RoleResponse, tool: TeamToolName) -> bool {
    role.denied_tools.iter().any(|denied| denied == tool.as_str())
}

fn role_failure(error: RoleError, command: &str) -> Reply {
    match error {
        RoleError::InvalidRequest(message) => failure(
            StatusCode::BAD_REQUEST,
            RoleCliErrorCode::SchemaValidationFailed,
            message,
            command,
        ),
        // The role was dismissed between the lookup and the write.
        RoleError::InvalidId(_) | RoleError::NotFound(_) => failure(
            StatusCode::NOT_FOUND,
            RoleCliErrorCode::NotARole,
            "this conversation does not run as a resident role",
            command,
        ),
        RoleError::Corrupt(_) | RoleError::Io(_) => {
            tracing::error!(command, error = %error, "role helper: write failed");
            internal(command)
        }
    }
}

fn internal(command: &str) -> Reply {
    failure(
        StatusCode::INTERNAL_SERVER_ERROR,
        RoleCliErrorCode::Internal,
        "internal error",
        command,
    )
}

fn success<T: Serialize>(command: &str, data: &T) -> Reply {
    match serde_json::to_value(data) {
        Ok(data) => (StatusCode::OK, Json(RoleCliEnvelope::success(data, command))),
        Err(error) => {
            tracing::error!(command, error = %error, "role helper: reply could not be encoded");
            internal(command)
        }
    }
}

fn failure(status: StatusCode, code: RoleCliErrorCode, message: impl Into<String>, command: &str) -> Reply {
    (status, Json(RoleCliEnvelope::failure(code, message, command)))
}
