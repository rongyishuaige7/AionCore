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
//! And it declares here how a run nobody watched ended: whether the user is to
//! be told something, asked something, or left alone. What it tells is kept
//! with the role, and the declaration is announced before the turn completes
//! so that a client can decide whether the finished run asks for attention.
//!
//! Authenticates on the three `x-aionui-*` headers itself, like the other
//! runtime routes, and is mounted outside the ordinary auth and CSRF middleware.

use std::sync::Arc;

use aionui_ai_agent::{RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_api_types::{
    ROLE_SEND_TOOL_NAME, RoleCliEnvelope, RoleCliErrorCode, RoleCurateRequest, RoleCurateResponse, RoleExpandRequest,
    RoleFollowUpRequest, RoleFollowUpResponse, RolePeerSummary, RolePeersResponse, RoleProgressRequest,
    RoleProgressResponse, RoleRecallRequest, RoleRecallSource, RoleRememberRequest, RoleRememberResponse, RoleReport,
    RoleReportRequest, RoleReportResponse, RoleReportedEvent, RoleResponse, RoleSendRequest, RoleSendResponse,
    SessionSendMessageRequest, TeamToolName, WebSocketMessage,
};
use aionui_common::now_ms;
use aionui_conversation::runtime_state::ConversationRuntimeStateService;
use aionui_conversation::{ConversationError, ConversationService};
use aionui_cron::error::CronError;
use aionui_cron::service::CronService;
use aionui_db::models::MessageRow;
use aionui_db::{
    IAssistantDefinitionRepository, IConversationRepository, MessagePageCursor, MessagePageDirection, MessagePageParams,
};
use aionui_realtime::EventBroadcaster;
use aionui_role::{RecallConversationSync, RecallMessage, RecallSync, RecallWatermark, RoleError, RoleService};
use aionui_session_message::error::SessionMessageError;
use aionui_session_message::service::SessionMessageService;
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

/// Message type of the card a run's report leaves in the role's thread.
const ROLE_REPORT_MESSAGE: &str = "role_report";
/// Message type of the card a run that told the user nothing leaves in the
/// role's thread: how it ended.
pub(crate) const ROLE_RUN_MESSAGE: &str = "role_run";
/// Announces a card left in a role's thread, carrying the message as stored.
const ROLE_THREAD_CARD_EVENT: &str = "role.thread_card";

#[derive(Clone)]
pub struct RuntimeRoleState {
    pub roles: Arc<RoleService>,
    /// Where a role's follow-ups wait until they are due.
    pub timers: Arc<CronService>,
    pub conversations: Arc<dyn IConversationRepository>,
    pub assistant_definitions: Arc<dyn IAssistantDefinitionRepository>,
    pub conversation_service: ConversationService,
    pub session_messages: Arc<SessionMessageService>,
    pub runtime_state: Arc<ConversationRuntimeStateService>,
    pub runtime_token_service: Arc<RuntimeTokenService>,
    /// Where a run's declaration of how it ended is announced.
    pub broadcaster: Arc<dyn EventBroadcaster>,
}

pub fn runtime_role_routes(state: RuntimeRoleState) -> Router {
    Router::new()
        .route("/api/runtime/role", get(show))
        .route("/api/runtime/role/peers", get(peers))
        .route("/api/runtime/role/send", post(send))
        .route("/api/runtime/role/remember", post(remember))
        .route("/api/runtime/role/curate", post(curate))
        .route("/api/runtime/role/progress", post(progress))
        .route("/api/runtime/role/follow-up", post(follow_up))
        .route("/api/runtime/role/report", post(report))
        .route("/api/runtime/role/recall", post(recall))
        .route("/api/runtime/role/expand", post(expand))
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

/// Declare how the run ended. Open to every role: what it keeps is a record of
/// what the user was told, not the memory the user may reserve.
async fn report(
    State(state): State<RuntimeRoleState>,
    headers: HeaderMap,
    body: Result<Json<RoleReportRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "role report";
    let (caller, role) = match caller_and_role(&state, &headers, COMMAND).await {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let request = match parsed(body, COMMAND) {
        Ok(request) => request,
        Err(reply) => return *reply,
    };
    let at = now_ms();
    let kept = match state
        .roles
        .record_report(&role.id, request.outcome, &request.summary, &caller.conversation_id, at)
        .await
    {
        Ok(kept) => kept,
        Err(error) => return role_failure(error, COMMAND),
    };
    if let Some(report) = kept.as_ref() {
        card_in_thread(&state, &caller.user_id, &role.id, report).await;
    }
    let event = RoleReportedEvent {
        conversation_id: caller.conversation_id,
        role_id: role.id,
        outcome: request.outcome,
        // As it was kept: one line. A silent run's reason is not passed on.
        summary: kept.as_ref().map(|report| report.summary.clone()).unwrap_or_default(),
        at,
    };
    match serde_json::to_value(&event) {
        Ok(mut payload) => {
            payload["user_id"] = Value::String(caller.user_id);
            state
                .broadcaster
                .broadcast(WebSocketMessage::new("role.reported", payload));
        }
        Err(error) => tracing::error!(command = COMMAND, error = %error, "role helper: report not announced"),
    }
    success(
        COMMAND,
        &RoleReportResponse {
            outcome: request.outcome,
            recorded: kept.is_some(),
        },
    )
}

/// Search this role's own thread, scheduled runs, and summary leaves. Open to
/// every role: it reads only this role's own past conversations and writes
/// nothing the user reserves.
async fn recall(
    State(state): State<RuntimeRoleState>,
    headers: HeaderMap,
    body: Result<Json<RoleRecallRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "role recall";
    let (caller, role) = match caller_and_role(&state, &headers, COMMAND).await {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let request = match parsed(body, COMMAND) {
        Ok(request) => request,
        Err(reply) => return *reply,
    };
    catch_up_role_recall(&caller.user_id, &role.id, &state.roles, state.conversations.as_ref()).await;
    match state.roles.recall(&role.id, &request, now_ms()).await {
        Ok(found) => success(COMMAND, &found),
        Err(error) => role_failure(error, COMMAND),
    }
}

/// Expand a `ref_id` returned by `role recall` into its surrounding message
/// window or full summary leaf.
async fn expand(
    State(state): State<RuntimeRoleState>,
    headers: HeaderMap,
    body: Result<Json<RoleExpandRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "role expand";
    let (caller, role) = match caller_and_role(&state, &headers, COMMAND).await {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let request = match parsed(body, COMMAND) {
        Ok(request) => request,
        Err(reply) => return *reply,
    };
    catch_up_role_recall(&caller.user_id, &role.id, &state.roles, state.conversations.as_ref()).await;
    match state.roles.expand(&role.id, &request).await {
        Ok(expanded) => success(COMMAND, &expanded),
        Err(error) => role_failure(error, COMMAND),
    }
}

/// Live roster of other resident roles (`role peers` / `role_peers`), with
/// each peer's display name, charter gist, thread id, and busy flag.
async fn peers(State(state): State<RuntimeRoleState>, headers: HeaderMap) -> Reply {
    const COMMAND: &str = "role peers";
    let (caller, role) = match caller_and_role(&state, &headers, COMMAND).await {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let all_roles = match state.roles.list().await {
        Ok(roles) => roles,
        Err(error) => return role_failure(error, COMMAND),
    };
    let definitions = state
        .assistant_definitions
        .list_for_user(&caller.user_id)
        .await
        .unwrap_or_default();

    let mut peer_list = Vec::new();
    for item in all_roles {
        if item.id == role.id {
            continue;
        }
        let name = definitions
            .iter()
            .find(|def| def.assistant_id == item.id)
            .map(|def| def.name.trim())
            .filter(|name| !name.is_empty())
            .unwrap_or(&item.id)
            .to_owned();
        let busy = item
            .thread_conversation_id
            .as_deref()
            .is_some_and(|tid| state.runtime_state.active_turn_id_for(tid).is_some());
        peer_list.push(RolePeerSummary {
            role_id: item.id,
            name,
            charter_summary: super::state::summarize_role_charter(&item.charter),
            thread_conversation_id: item.thread_conversation_id,
            busy,
        });
    }
    success(COMMAND, &RolePeersResponse { peers: peer_list })
}

/// Deliver a message either to a peer resident role (`to_role`, lazily
/// creating its thread when needed) or to an explicit conversation id
/// (`to_conversation`, e.g. `reply_to` from an incoming `[[AION_SESSION_MESSAGE]]`).
async fn send(
    State(state): State<RuntimeRoleState>,
    headers: HeaderMap,
    body: Result<Json<RoleSendRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "role send";
    let (caller, role) = match caller_and_role(&state, &headers, COMMAND).await {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let request = match parsed(body, COMMAND) {
        Ok(request) => request,
        Err(reply) => return *reply,
    };
    if role.denied_tools.iter().any(|tool| tool == ROLE_SEND_TOOL_NAME) {
        return failure(
            StatusCode::FORBIDDEN,
            RoleCliErrorCode::PermissionDenied,
            "this role is not permitted to message colleagues on its own; tell the user who should be contacted",
            COMMAND,
        );
    }

    let to_role = request.to_role.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let to_conv = request
        .to_conversation
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let (target_conversation_id, resolved_role_id) = match (to_role, to_conv) {
        (Some(_), Some(_)) | (None, None) => {
            return failure(
                StatusCode::BAD_REQUEST,
                RoleCliErrorCode::SchemaValidationFailed,
                "provide exactly one of `to_role` (peer role id) or `to_conversation` (conversation id)",
                COMMAND,
            );
        }
        (Some(peer_role_id), None) => {
            if peer_role_id == role.id {
                return failure(
                    StatusCode::BAD_REQUEST,
                    RoleCliErrorCode::TargetIsSelf,
                    "cannot message your own role",
                    COMMAND,
                );
            }
            match state
                .conversation_service
                .ensure_role_thread(&caller.user_id, peer_role_id)
                .await
            {
                Ok((_, row)) => (row.id, Some(peer_role_id.to_owned())),
                Err(ConversationError::NotFound { id }) => {
                    return failure(
                        StatusCode::NOT_FOUND,
                        RoleCliErrorCode::TargetNotFound,
                        format!("peer resident role not found: {id}"),
                        COMMAND,
                    );
                }
                Err(ConversationError::BadRequest { reason }) => {
                    return failure(
                        StatusCode::BAD_REQUEST,
                        RoleCliErrorCode::SchemaValidationFailed,
                        reason,
                        COMMAND,
                    );
                }
                Err(error) => {
                    return failure(
                        StatusCode::CONFLICT,
                        RoleCliErrorCode::TransportUnavailable,
                        error.to_string(),
                        COMMAND,
                    );
                }
            }
        }
        (None, Some(conv_id)) => (conv_id.to_owned(), None),
    };

    let send_req = SessionSendMessageRequest {
        to: target_conversation_id,
        message: request.message,
    };
    match state
        .session_messages
        .send_detailed(&caller.user_id, &caller.conversation_id, &send_req)
        .await
    {
        Ok(outcome) => success(
            COMMAND,
            &RoleSendResponse {
                status: outcome.response.status,
                to_conversation_id: outcome.response.to,
                to_role_id: resolved_role_id,
                hop: outcome.hop,
                max_hops: outcome.max_hops,
            },
        ),
        Err(error) => session_send_failure(error, COMMAND),
    }
}

fn session_send_failure(error: SessionMessageError, command: &str) -> Reply {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::CONFLICT);
    let message = error.to_string();
    let code = match error {
        SessionMessageError::TargetNotFound { .. } => RoleCliErrorCode::TargetNotFound,
        SessionMessageError::TargetIsSelf { .. } => RoleCliErrorCode::TargetIsSelf,
        SessionMessageError::TargetIsTeam { .. } | SessionMessageError::SenderIsTeam { .. } => {
            RoleCliErrorCode::PermissionDenied
        }
        SessionMessageError::PermissionDenied => RoleCliErrorCode::PermissionDenied,
        SessionMessageError::HopLimitReached { .. } => RoleCliErrorCode::HopLimitReached,
        SessionMessageError::QueueFull => RoleCliErrorCode::QueueFull,
        SessionMessageError::RateLimited => RoleCliErrorCode::RateLimited,
        SessionMessageError::FeatureDisabled => RoleCliErrorCode::FeatureDisabled,
        SessionMessageError::SchemaValidation { .. } => RoleCliErrorCode::SchemaValidationFailed,
        SessionMessageError::TransportUnavailable { .. } => RoleCliErrorCode::TransportUnavailable,
    };
    failure(status, code, message, command)
}

const RECALL_PAGE_ROWS: u32 = 500;
const RECALL_MAX_PAGES: usize = 200;
const STALE_STREAMING_MS: i64 = 60 * 60 * 1000;

/// Incrementally catch up `role_id`'s full-text recall index from its live
/// thread, scheduled-run conversations, and on-disk summary leaves.
pub(crate) async fn catch_up_role_recall(
    user_id: &str,
    role_id: &str,
    roles: &RoleService,
    conversations: &dyn IConversationRepository,
) {
    let recorded_thread = match roles.thread(role_id).await {
        Ok(recorded) => recorded,
        Err(error) => {
            tracing::warn!(role_id, error = %error, "role recall: thread unreadable");
            return;
        }
    };
    let active_thread = match recorded_thread {
        Some(tid) => match conversations.get(user_id, &tid).await {
            Ok(Some(_)) => Some(tid),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(role_id, conversation_id = %tid, error = %error, "role recall: thread lookup failed");
                return;
            }
        },
        None => None,
    };
    let mut active_runs = match conversations.list_scheduled_for_assistant(user_id, role_id).await {
        Ok(runs) => runs,
        Err(error) => {
            tracing::warn!(role_id, error = %error, "role recall: scheduled runs lookup failed");
            return;
        }
    };
    if let Some(ref tid) = active_thread {
        active_runs.retain(|id| id != tid);
    }

    let watermarks = match roles.recall_watermarks(role_id).await {
        Ok(watermarks) => watermarks,
        Err(error) => {
            tracing::warn!(role_id, error = %error, "role recall: watermarks unreadable");
            return;
        }
    };

    let mut targets = Vec::with_capacity(active_runs.len() + usize::from(active_thread.is_some()));
    if let Some(ref tid) = active_thread {
        targets.push((tid.clone(), RoleRecallSource::Thread));
    }
    for run_id in &active_runs {
        targets.push((run_id.clone(), RoleRecallSource::Run));
    }

    let mut batches = Vec::new();
    for (conv_id, source) in targets {
        match scan_conversation_after(user_id, &conv_id, source, watermarks.get(&conv_id), conversations).await {
            Ok(Some(batch)) => batches.push(batch),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    role_id,
                    conversation_id = %conv_id,
                    error = %error,
                    "role recall: conversation messages unreadable"
                );
            }
        }
    }

    if let Err(error) = roles
        .sync_recall(
            role_id,
            RecallSync {
                active_thread,
                active_runs,
                batches,
            },
        )
        .await
    {
        tracing::warn!(role_id, error = %error, "role recall: index sync failed");
    }
}

async fn scan_conversation_after(
    user_id: &str,
    conversation_id: &str,
    source: RoleRecallSource,
    watermark: Option<&RecallWatermark>,
    conversations: &dyn IConversationRepository,
) -> Result<Option<RecallConversationSync>, aionui_db::DbError> {
    let mut cursor = watermark.map_or(
        MessagePageCursor {
            created_at: 0,
            id: String::new(),
        },
        |wm| MessagePageCursor {
            created_at: wm.created_at,
            id: wm.id.clone(),
        },
    );
    let mut rows = Vec::new();
    for _ in 0..RECALL_MAX_PAGES {
        let page = conversations
            .list_messages_page(
                user_id,
                conversation_id,
                &MessagePageParams {
                    limit: RECALL_PAGE_ROWS,
                    direction: MessagePageDirection::After { cursor: cursor.clone() },
                },
            )
            .await?;
        let Some(last) = page.items.last() else {
            break;
        };
        cursor = MessagePageCursor::from(last);
        let more = page.has_more_after;
        rows.extend(page.items);
        if !more {
            break;
        }
    }
    if rows.is_empty() {
        return Ok(None);
    }

    let latest_finished_at = rows
        .iter()
        .rev()
        .find(|row| !row.hidden && row.r#type == "text" && row.status.as_deref() == Some("finish"))
        .map_or(0, |row| row.created_at);
    let cutoff = rows
        .iter()
        .position(|row| {
            aionui_conversation::is_unfinished_text_row(row)
                && latest_finished_at.saturating_sub(row.created_at) < STALE_STREAMING_MS
        })
        .unwrap_or(rows.len());
    let settled = &rows[..cutoff];
    if settled.is_empty() {
        return Ok(None);
    }

    let messages = aionui_conversation::lines_of(settled)
        .into_iter()
        .map(|line| RecallMessage {
            id: line.mark.id,
            created_at: line.mark.created_at,
            speaker: line.speaker.as_str().to_owned(),
            text: line.text,
        })
        .collect();
    let new_watermark = settled.last().map(|last| (last.created_at, last.id.clone()));

    Ok(Some(RecallConversationSync {
        conversation_id: conversation_id.to_owned(),
        source,
        messages,
        watermark: new_watermark,
    }))
}

/// Leave what the role told the user as a card in the role's one thread with
/// the user, when it was told from a run elsewhere, and show it to open
/// clients. The thread's next turn hands the same report to the agent. A
/// failure is logged; the report stands either way.
async fn card_in_thread(state: &RuntimeRoleState, user_id: &str, role_id: &str, report: &RoleReport) {
    let thread = match state.roles.thread(role_id).await {
        Ok(Some(thread)) if thread != report.conversation_id => thread,
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(role_id, error = %error, "role report: thread unreadable, no card left");
            return;
        }
    };
    let content = serde_json::json!({
        "role_id": role_id,
        "outcome": report.outcome,
        "summary": report.summary,
        "run_conversation_id": report.conversation_id,
        "at": report.at,
    });
    leave_thread_card(
        state.conversations.as_ref(),
        state.broadcaster.as_ref(),
        user_id,
        &thread,
        ROLE_REPORT_MESSAGE,
        content,
        report.at,
    )
    .await;
}

/// Leave a card of `message_type` in a role's thread and show it to open
/// clients. A failure is logged; `false` when the card was not left.
pub(crate) async fn leave_thread_card(
    conversations: &dyn IConversationRepository,
    broadcaster: &dyn EventBroadcaster,
    user_id: &str,
    thread: &str,
    message_type: &str,
    content: Value,
    at: i64,
) -> bool {
    let row = MessageRow {
        id: ConversationService::mint_msg_id(),
        conversation_id: thread.to_owned(),
        msg_id: None,
        r#type: message_type.into(),
        content: content.to_string(),
        position: Some("center".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: at,
        backend_turn_id: None,
    };
    if let Err(error) = conversations.insert_message(user_id, &row).await {
        // The thread may be gone; the user's next conversation with the role takes its place.
        tracing::warn!(conversation_id = %thread, message_type, error = %error, "role thread: card not left");
        return false;
    }
    tracing::info!(conversation_id = %thread, message_type, "role thread: card left");
    // Its own event rather than a stream frame: a card is no turn, and the
    // conversation's turn tracking must not take it for one.
    broadcaster.broadcast(WebSocketMessage::new(
        ROLE_THREAD_CARD_EVENT,
        serde_json::json!({
            "user_id": user_id,
            "id": row.id,
            "conversation_id": thread,
            "type": row.r#type,
            "content": content,
            "position": row.position,
            "status": row.status,
            "created_at": row.created_at,
        }),
    ));
    true
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
        RoleError::InvalidRequest(message) | RoleError::Conflict(message) => failure(
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
