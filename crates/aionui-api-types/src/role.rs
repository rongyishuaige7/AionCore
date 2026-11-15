//! Resident-role DTOs (`/api/roles`).
//!
//! A role is an assistant promoted to a standing team member: on top of the
//! assistant's own definition it carries a charter, a memory that outlives any
//! single team, and a table of team tools it may not call.

use serde::{Deserialize, Serialize};

/// One resident role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleResponse {
    /// Id of the assistant this role is built on.
    pub id: String,
    /// What the role owns and what it refuses, in the user's own words.
    pub charter: String,
    /// Team tools this role may not call, enforced where team tools execute.
    pub denied_tools: Vec<String>,
    /// Whether this role reviews work: in a team it has joined, a task another
    /// member marks completed is held until this role passes it.
    #[serde(default)]
    pub verifies: bool,
    /// Contents of the role's `MEMORY.md`.
    pub memory: String,
    /// Absolute path of the role's memory directory on the backend host.
    pub memory_dir: String,
    /// Work the role has under way, most recently touched first.
    #[serde(default)]
    pub progress: Vec<RoleProgress>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Where one piece of a role's ongoing work stands. Each task is a file in the
/// role's memory directory (`progress/<task>.md`) that the role replaces as the
/// work moves and removes when it is finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleProgress {
    /// Name of the task, which is also its file name.
    pub task: String,
    /// What is done and what is left, as the role last wrote it.
    pub state: String,
    /// When the state was last written, in milliseconds since the epoch.
    pub updated_at: i64,
}

/// Response of `GET /api/roles`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleListResponse {
    pub roles: Vec<RoleResponse>,
}

/// Body of `PUT /api/roles/{id}` — create the role or replace its definition.
/// The memory is left untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpsertRoleRequest {
    #[serde(default)]
    pub charter: String,
    #[serde(default)]
    pub denied_tools: Vec<String>,
    #[serde(default)]
    pub verifies: bool,
}

/// Body of `PUT /api/roles/{id}/memory` — replace the role's `MEMORY.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRoleMemoryRequest {
    #[serde(default)]
    pub memory: String,
}
