// `ApiError` is the intended error type at this HTTP boundary (routes map the
// crate-owned `RoleError` to it here), so the disallowed_types lint that steers
// service code away from `ApiError` does not apply to this module.
#![allow(clippy::disallowed_types)]

//! Resident-role HTTP routes.
//!
//! - `GET /api/roles` — every defined role.
//! - `GET /api/roles/{id}` — one role, with its memory, playbook, and proposals.
//! - `PUT /api/roles/{id}` — create or update role (agent major changes become proposals).
//! - `PUT /api/roles/{id}/memory` — replace the role's `MEMORY.md` (or seed on hire).
//! - `DELETE /api/roles/{id}/progress/{task}` — close one task in progress.
//! - `DELETE /api/roles/{id}/playbook/{entry_id}` — remove one playbook item.
//! - `POST /api/roles/{id}/proposals/{proposal_id}/approve` — user approves a proposal.
//! - `POST /api/roles/{id}/proposals/{proposal_id}/reject` — user rejects a proposal.
//! - `GET /api/roles/{id}/timeline` — git commit history with diffs.
//! - `POST /api/roles/{id}/revert` — create a forward git revert commit.
//! - `POST /api/roles/{id}/dream` — run S5 dreamer maintenance on demand.
//! - `DELETE /api/roles/{id}` — dismiss the role (and pause/clean its cron jobs).

use aionui_api_types::{
    ApiResponse, RoleDreamResponse, RoleListResponse, RoleProposalChange, RoleResponse, RoleRevertRequest,
    RoleTimelineResponse, UpdateRoleMemoryRequest, UpsertRoleRequest,
};
use aionui_common::{ApiError, AuthenticatedUserId, RuntimeHelperCaller, now_ms};
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};

use crate::error::RoleError;
use crate::state::RoleRouterState;

const DEFAULT_USER_ID: &str = "system_default_user";

/// Build the role router.
pub fn role_routes(state: RoleRouterState) -> Router {
    Router::new()
        .route("/api/roles", get(list_roles))
        .route("/api/roles/{id}", get(get_role).put(put_role).delete(delete_role))
        .route("/api/roles/{id}/memory", put(put_memory))
        .route("/api/roles/{id}/progress/{task}", delete(delete_progress))
        .route("/api/roles/{id}/playbook/{entry_id}", delete(delete_playbook_entry))
        .route(
            "/api/roles/{id}/proposals/{proposal_id}/approve",
            post(approve_proposal),
        )
        .route("/api/roles/{id}/proposals/{proposal_id}/reject", post(reject_proposal))
        .route("/api/roles/{id}/timeline", get(get_timeline))
        .route("/api/roles/{id}/revert", post(revert_role))
        .route("/api/roles/{id}/dream", post(dream_role))
        .with_state(state)
}

async fn list_roles(State(state): State<RoleRouterState>) -> Result<Json<ApiResponse<RoleListResponse>>, ApiError> {
    let roles = state.service.list().await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(RoleListResponse { roles })))
}

async fn get_role(
    State(state): State<RoleRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RoleResponse>>, ApiError> {
    let role = state.service.get(&id).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(role)))
}

async fn put_role(
    State(state): State<RoleRouterState>,
    helper: Option<Extension<RuntimeHelperCaller>>,
    Path(id): Path<String>,
    Json(body): Json<UpsertRoleRequest>,
) -> Result<Json<ApiResponse<RoleResponse>>, ApiError> {
    let agent_conv = helper.as_ref().map(|Extension(caller)| caller.conversation_id.as_str());
    let role = state
        .service
        .upsert_by_caller(&id, body, agent_conv)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(role)))
}

async fn put_memory(
    State(state): State<RoleRouterState>,
    helper: Option<Extension<RuntimeHelperCaller>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRoleMemoryRequest>,
) -> Result<Json<ApiResponse<RoleResponse>>, ApiError> {
    if helper.is_some()
        && let Some(existing) = state.service.find(&id).await.map_err(to_api_error)?
        && !existing.memory.trim().is_empty()
    {
        return Err(ApiError::Forbidden(
            "agents may only seed an empty memory during hire; use `role remember` or `role curate` instead".into(),
        ));
    }
    let role = state
        .service
        .set_memory(&id, &body.memory)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(role)))
}

/// Close a task the role left in progress, for the user to clear what the role
/// did not. Answers with the role as it stands afterwards.
async fn delete_progress(
    State(state): State<RoleRouterState>,
    Path((id, task)): Path<(String, String)>,
) -> Result<Json<ApiResponse<RoleResponse>>, ApiError> {
    state.service.set_progress(&id, &task, "").await.map_err(to_api_error)?;
    let role = state.service.get(&id).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(role)))
}

async fn delete_playbook_entry(
    State(state): State<RoleRouterState>,
    helper: Option<Extension<RuntimeHelperCaller>>,
    Path((id, entry_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<RoleResponse>>, ApiError> {
    require_human_caller(&helper)?;
    let role = state
        .service
        .remove_playbook_entry(&id, &entry_id, now_ms())
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(role)))
}

async fn approve_proposal(
    State(state): State<RoleRouterState>,
    user: Option<Extension<AuthenticatedUserId>>,
    helper: Option<Extension<RuntimeHelperCaller>>,
    Path((id, proposal_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<RoleResponse>>, ApiError> {
    require_human_caller(&helper)?;
    let user_id = caller_user_id(&user);
    let proposal = state
        .service
        .take_proposal(&id, &proposal_id)
        .await
        .map_err(to_api_error)?;

    match &proposal.change {
        RoleProposalChange::UpdateRole { .. } => {
            let role = state
                .service
                .apply_approved_role_change(&id, &proposal.summary, &proposal.change, now_ms())
                .await
                .map_err(to_api_error)?;
            Ok(Json(ApiResponse::ok(role)))
        }
        RoleProposalChange::CreateCronJob { .. }
        | RoleProposalChange::UpdateCronJob { .. }
        | RoleProposalChange::DeleteCronJob { .. } => {
            let Some(ref external) = state.external else {
                return Err(ApiError::Internal("cron proposal port is not configured".into()));
            };
            if let Err(reason) = external.apply_cron_proposal(&user_id, &id, &proposal.change).await {
                // Re-stage the proposal so a transient failure never loses it.
                let _ = state
                    .service
                    .stage_proposal(
                        &id,
                        proposal.summary,
                        proposal.detail,
                        proposal.from_conversation_id,
                        proposal.change,
                        proposal.created_at,
                    )
                    .await;
                return Err(ApiError::coded(
                    StatusCode::BAD_REQUEST,
                    "role_proposal_failed",
                    reason,
                    None,
                ));
            }
            let role = state.service.get(&id).await.map_err(to_api_error)?;
            Ok(Json(ApiResponse::ok(role)))
        }
    }
}

async fn reject_proposal(
    State(state): State<RoleRouterState>,
    helper: Option<Extension<RuntimeHelperCaller>>,
    Path((id, proposal_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<RoleResponse>>, ApiError> {
    require_human_caller(&helper)?;
    let role = state
        .service
        .reject_proposal(&id, &proposal_id)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(role)))
}

async fn get_timeline(
    State(state): State<RoleRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RoleTimelineResponse>>, ApiError> {
    let commits = state.service.timeline(&id, 40).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(RoleTimelineResponse { commits })))
}

async fn revert_role(
    State(state): State<RoleRouterState>,
    helper: Option<Extension<RuntimeHelperCaller>>,
    Path(id): Path<String>,
    Json(body): Json<RoleRevertRequest>,
) -> Result<Json<ApiResponse<RoleResponse>>, ApiError> {
    require_human_caller(&helper)?;
    let role = state
        .service
        .revert(&id, &body.sha, now_ms())
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(role)))
}

async fn dream_role(
    State(state): State<RoleRouterState>,
    user: Option<Extension<AuthenticatedUserId>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RoleDreamResponse>>, ApiError> {
    let user_id = caller_user_id(&user);
    if let Some(ref external) = state.external {
        let report = external
            .run_dreamer(&user_id, &id)
            .await
            .map_err(|msg| ApiError::coded(StatusCode::BAD_REQUEST, "role_dream_failed", msg, None))?;
        return Ok(Json(ApiResponse::ok(report)));
    }
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let now = now_ms();
    let (decayed_playbook, archived_progress) = state
        .service
        .run_zero_llm_decay(&id, &today, now)
        .await
        .map_err(to_api_error)?;
    state.service.mark_dreamer_ran(&id, now).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(RoleDreamResponse {
        decayed_playbook,
        archived_progress,
        ..Default::default()
    })))
}

async fn delete_role(
    State(state): State<RoleRouterState>,
    user: Option<Extension<AuthenticatedUserId>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.remove(&id).await.map_err(to_api_error)?;
    if let Some(ref external) = state.external {
        external.on_role_dismissed(&caller_user_id(&user), &id).await;
    }
    Ok(Json(ApiResponse::ok(())))
}

fn require_human_caller(helper: &Option<Extension<RuntimeHelperCaller>>) -> Result<(), ApiError> {
    if helper.is_some() {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "role_user_review_required",
            "only the user may approve, reject, or revert role governance changes",
            None,
        ));
    }
    Ok(())
}

fn caller_user_id(user: &Option<Extension<AuthenticatedUserId>>) -> String {
    user.as_ref()
        .map(|Extension(AuthenticatedUserId(id))| id.clone())
        .unwrap_or_else(|| DEFAULT_USER_ID.to_owned())
}

fn to_api_error(err: RoleError) -> ApiError {
    match err {
        RoleError::InvalidId(_) => ApiError::coded(StatusCode::BAD_REQUEST, "role_invalid_id", "invalid role id", None),
        RoleError::InvalidRequest(msg) => ApiError::coded(StatusCode::BAD_REQUEST, "role_bad_request", msg, None),
        RoleError::NotFound(_) => ApiError::coded(StatusCode::NOT_FOUND, "role_not_found", "role not found", None),
        RoleError::Corrupt(_) | RoleError::Io(_) => {
            tracing::error!(error = %err, "role: internal error");
            ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "role_internal",
                "internal error",
                None,
            )
        }
    }
}

#[cfg(test)]
#[path = "routes_test.rs"]
mod routes_test;
