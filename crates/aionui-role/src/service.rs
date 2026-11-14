//! Business rules for resident roles: id and input validation, and the mapping
//! between the on-disk definition and the wire shape.

use std::path::PathBuf;

use aionui_api_types::{RoleResponse, TeamToolName, UpsertRoleRequest};
use aionui_common::now_ms;

use crate::error::RoleError;
use crate::store::{RoleDefinition, RoleStore};

/// Cap on a charter, in characters.
pub const MAX_CHARTER_CHARS: usize = 4_000;
/// Cap on `MEMORY.md`, in bytes.
pub const MAX_MEMORY_BYTES: usize = 64 * 1024;
const MAX_ROLE_ID_LEN: usize = 128;

#[derive(Debug, Clone)]
pub struct RoleService {
    store: RoleStore,
}

impl RoleService {
    /// `root` is the directory that holds one sub-directory per role
    /// (`<data_dir>/roles`). It is created on first write.
    pub fn new(root: PathBuf) -> Self {
        Self {
            store: RoleStore::new(root),
        }
    }

    /// Every defined role, oldest first. A role whose definition file cannot be
    /// read is skipped (and logged) rather than failing the whole list.
    pub async fn list(&self) -> Result<Vec<RoleResponse>, RoleError> {
        let mut roles = Vec::new();
        for id in self.store.list_ids().await? {
            if validate_role_id(&id).is_err() {
                continue;
            }
            match self.find(&id).await {
                Ok(Some(role)) => roles.push(role),
                Ok(None) => {}
                Err(error) => tracing::warn!(role_id = %id, error = %error, "role: skipped unreadable role"),
            }
        }
        roles.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        Ok(roles)
    }

    pub async fn get(&self, id: &str) -> Result<RoleResponse, RoleError> {
        self.find(id).await?.ok_or_else(|| RoleError::NotFound(id.to_owned()))
    }

    /// The role built on `id`, or `None` when that assistant is not a role.
    pub async fn find(&self, id: &str) -> Result<Option<RoleResponse>, RoleError> {
        validate_role_id(id)?;
        let Some(definition) = self.store.read_definition(id).await? else {
            return Ok(None);
        };
        Ok(Some(self.to_response(id, definition).await?))
    }

    /// Create the role or replace its definition. The memory is left untouched,
    /// and so is `created_at` of an existing role.
    pub async fn upsert(&self, id: &str, request: UpsertRoleRequest) -> Result<RoleResponse, RoleError> {
        validate_role_id(id)?;
        let charter = request.charter.trim().to_owned();
        if charter.chars().count() > MAX_CHARTER_CHARS {
            return Err(RoleError::InvalidRequest(format!(
                "charter exceeds {MAX_CHARTER_CHARS} characters"
            )));
        }
        let denied_tools = normalize_denied_tools(&request.denied_tools)?;

        // A corrupt definition must not block the user from saving a good one.
        let existing = match self.store.read_definition(id).await {
            Ok(existing) => existing,
            Err(RoleError::Corrupt(_)) => None,
            Err(error) => return Err(error),
        };
        let now = now_ms();
        let definition = RoleDefinition {
            charter,
            denied_tools,
            created_at: existing.map_or(now, |existing| existing.created_at),
            updated_at: now,
        };
        self.store.write_definition(id, &definition).await?;
        self.to_response(id, definition).await
    }

    /// Replace the role's `MEMORY.md`.
    pub async fn set_memory(&self, id: &str, memory: &str) -> Result<RoleResponse, RoleError> {
        validate_role_id(id)?;
        if memory.len() > MAX_MEMORY_BYTES {
            return Err(RoleError::InvalidRequest(format!(
                "memory exceeds {MAX_MEMORY_BYTES} bytes"
            )));
        }
        let Some(definition) = self.store.read_definition(id).await? else {
            return Err(RoleError::NotFound(id.to_owned()));
        };
        self.store.write_memory(id, memory).await?;
        self.to_response(id, definition).await
    }

    /// Dismiss the role. Its memory directory stays on disk and is picked up
    /// again if the role is defined anew.
    pub async fn remove(&self, id: &str) -> Result<(), RoleError> {
        validate_role_id(id)?;
        if self.store.remove_definition(id).await? {
            Ok(())
        } else {
            Err(RoleError::NotFound(id.to_owned()))
        }
    }

    async fn to_response(&self, id: &str, definition: RoleDefinition) -> Result<RoleResponse, RoleError> {
        Ok(RoleResponse {
            id: id.to_owned(),
            charter: definition.charter,
            denied_tools: definition.denied_tools,
            memory: self.store.read_memory(id).await?,
            memory_dir: self.store.memory_dir(id).to_string_lossy().into_owned(),
            created_at: definition.created_at,
            updated_at: definition.updated_at,
        })
    }
}

/// A role id becomes a directory name, so it is restricted to one plain path
/// component: `[A-Za-z0-9._-]`, not starting with a dot. Assistant ids
/// (`word-creator`, `custom-<ms>-<hex>`) all fit.
fn validate_role_id(id: &str) -> Result<(), RoleError> {
    let plain = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if id.is_empty() || id.len() > MAX_ROLE_ID_LEN || !plain || id.starts_with('.') {
        return Err(RoleError::InvalidId(id.chars().take(MAX_ROLE_ID_LEN).collect()));
    }
    Ok(())
}

/// Trim, reject anything that is not a real team tool, then sort and dedupe so
/// the stored table is canonical.
fn normalize_denied_tools(tools: &[String]) -> Result<Vec<String>, RoleError> {
    let mut normalized = Vec::with_capacity(tools.len());
    for tool in tools {
        let tool = tool.trim();
        if TeamToolName::parse(tool).is_none() {
            return Err(RoleError::InvalidRequest(format!("unknown team tool: {tool}")));
        }
        normalized.push(tool.to_owned());
    }
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
