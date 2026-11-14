// `ApiError` is the intended error type at this HTTP boundary (routes map the
// crate-owned `RoleError` to it here), so the disallowed_types lint that steers
// service code away from `ApiError` does not apply to this module.
#![allow(clippy::disallowed_types)]

//! Resident-role HTTP routes.
//!
//! - `GET /api/roles` — every defined role.
//! - `GET /api/roles/{id}` — one role, with its memory.
//! - `PUT /api/roles/{id}` — create the role or replace its definition.
//! - `PUT /api/roles/{id}/memory` — replace the role's `MEMORY.md`.
//! - `DELETE /api/roles/{id}` — dismiss the role (its memory stays on disk).
//!
//! `{id}` is the id of the assistant the role is built on. All routes require
//! authentication, applied by the caller in `aionui-app`.

use aionui_api_types::{ApiResponse, RoleListResponse, RoleResponse, UpdateRoleMemoryRequest, UpsertRoleRequest};
use aionui_common::ApiError;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};

use crate::error::RoleError;
use crate::state::RoleRouterState;

/// Build the role router.
pub fn role_routes(state: RoleRouterState) -> Router {
    Router::new()
        .route("/api/roles", get(list_roles))
        .route("/api/roles/{id}", get(get_role).put(put_role).delete(delete_role))
        .route("/api/roles/{id}/memory", put(put_memory))
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
    Path(id): Path<String>,
    Json(body): Json<UpsertRoleRequest>,
) -> Result<Json<ApiResponse<RoleResponse>>, ApiError> {
    let role = state.service.upsert(&id, body).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(role)))
}

async fn put_memory(
    State(state): State<RoleRouterState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRoleMemoryRequest>,
) -> Result<Json<ApiResponse<RoleResponse>>, ApiError> {
    let role = state
        .service
        .set_memory(&id, &body.memory)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(role)))
}

async fn delete_role(
    State(state): State<RoleRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.remove(&id).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

fn to_api_error(err: RoleError) -> ApiError {
    match err {
        RoleError::InvalidId(_) => ApiError::coded(StatusCode::BAD_REQUEST, "role_invalid_id", "invalid role id", None),
        RoleError::InvalidRequest(msg) => ApiError::coded(StatusCode::BAD_REQUEST, "role_bad_request", msg, None),
        RoleError::NotFound(_) => ApiError::coded(StatusCode::NOT_FOUND, "role_not_found", "role not found", None),
        RoleError::Corrupt(_) | RoleError::Io(_) => {
            tracing::error!(error = %err, "role: internal error");
            ApiError::coded(StatusCode::INTERNAL_SERVER_ERROR, "role_internal", "internal error", None)
        }
    }
}

#[cfg(test)]
#[path = "routes_test.rs"]
mod routes_test;
