//! Synchronizes a resident role's thread conversation (`thread_conversation_id`)
//! whenever its underlying assistant runtime (`agent_id` or `defaults.model` /
//! `permission` / `thought_level`) changes.
//!
//! Ordinary assistants leave historical conversations untouched as static
//! snapshots because users open new conversations from `/guid`. A resident role
//! is the opposite: `talkTo` (`找它聊`), `@@@` mentions, and `role send` always
//! reopen the role's single thread conversation. Keeping that thread's runtime
//! binding, `acp_session` row, and `conversation_assistant_snapshots` row in
//! sync ensures runtime switches take effect immediately while preserving the
//! transcript and S2 `<role-thread-handoff>` continuity.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_common::{AgentKillReason, AgentType, ConversationSource, ErrorChain, TurnCancelCause, now_ms};
use aionui_db::SaveRuntimeStateParams;
use aionui_db::models::{ConversationRow, UpsertConversationAssistantSnapshotParams};
use tracing::{info, warn};

use crate::convert::string_to_enum;
use crate::error::ConversationError;
use crate::service::{
    AssistantConversationOverrides, AssistantSnapshot, ConversationService, is_temp_session_workspace,
    team_id_from_extra,
};

impl ConversationService {
    /// Synchronize the resident role `role_id`'s recorded thread conversation
    /// with its current assistant definition and defaults.
    ///
    /// When `only_when_agent_changed` is `true` (used by startup drift healing
    /// and lazy `ensure_role_thread` reads), synchronization runs only if the
    /// thread's stored `agent_id` or `type` differs from the current assistant
    /// definition, leaving per-session chat-header model overrides untouched.
    /// When `false` (explicit assistant runtime update), both agent and model
    /// defaults are synchronized immediately.
    pub async fn sync_role_thread_runtime(
        &self,
        user_id: &str,
        role_id: &str,
        only_when_agent_changed: bool,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<bool, ConversationError> {
        let role_id = role_id.trim();
        if role_id.is_empty() {
            return Ok(false);
        }
        let Some(port) = self.role_port() else {
            return Ok(false);
        };
        let lock = self.role_threads.ensure_lock(role_id);
        let _guard = lock.lock().await;

        if !port.is_resident_role(role_id).await {
            return Ok(false);
        }
        let Some(thread_id) = port
            .role_thread(role_id)
            .await
            .map_err(|reason| ConversationError::internal(format!("role thread lookup failed: {reason}")))?
        else {
            return Ok(false);
        };
        let Some(existing) = self.conversation_repo().get(user_id, &thread_id).await? else {
            return Ok(false);
        };
        if team_id_from_extra(&existing.extra).is_some() {
            return Ok(false);
        }

        let existing_extra: serde_json::Value =
            serde_json::from_str(&existing.extra).unwrap_or_else(|_| serde_json::json!({}));
        let Some(mut snapshot) = self
            .resolve_assistant_snapshot(
                user_id,
                role_id,
                None,
                &AssistantConversationOverrides::default(),
                &existing_extra,
            )
            .await?
        else {
            return Ok(false);
        };
        self.add_standing_role(&mut snapshot, &existing_extra).await;

        self.apply_role_thread_runtime_snapshot(
            user_id,
            role_id,
            &existing,
            &snapshot,
            only_when_agent_changed,
            task_manager,
        )
        .await
    }

    /// Best-effort pre-turn drift guard invoked from `rotate_thread_if_due`: if
    /// the thread's `agent_id` drifted while no turn was running, heal `row`
    /// in-place before `build_task_options` reads it (and before checking
    /// `row.r#type`, so an `acp`/`antigravity` switch also triggers `Unanchored`
    /// handoff).
    pub(crate) async fn sync_role_thread_row_if_agent_drifted(
        &self,
        user_id: &str,
        row: &mut ConversationRow,
        assistant_id: &str,
    ) {
        if team_id_from_extra(&row.extra).is_some() {
            return;
        }
        let existing_extra: serde_json::Value =
            serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        let Ok(Some(mut snapshot)) = self
            .resolve_assistant_snapshot(
                user_id,
                assistant_id,
                None,
                &AssistantConversationOverrides::default(),
                &existing_extra,
            )
            .await
        else {
            return;
        };
        self.add_standing_role(&mut snapshot, &existing_extra).await;

        // Avoid cancelling the current turn's own claim during pre-turn setup:
        // only evict any leftover worker task if the agent actually drifted.
        match self
            .apply_role_thread_runtime_snapshot_inner(
                user_id,
                assistant_id,
                row,
                &snapshot,
                true,
                false,
                self.task_manager(),
            )
            .await
        {
            Ok(true) => {
                if let Ok(Some(refreshed)) = self.conversation_repo().get(user_id, &row.id).await {
                    *row = refreshed;
                }
            }
            Ok(false) => {}
            Err(error) => {
                warn!(
                    conversation_id = %row.id,
                    assistant_id,
                    error = %ErrorChain(&error),
                    "role thread: failed to heal drifted runtime before turn"
                );
            }
        }
    }

    pub(crate) async fn apply_role_thread_runtime_snapshot(
        &self,
        user_id: &str,
        role_id: &str,
        existing: &ConversationRow,
        snapshot: &AssistantSnapshot,
        only_when_agent_changed: bool,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<bool, ConversationError> {
        self.apply_role_thread_runtime_snapshot_inner(
            user_id,
            role_id,
            existing,
            snapshot,
            only_when_agent_changed,
            true,
            task_manager,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_role_thread_runtime_snapshot_inner(
        &self,
        user_id: &str,
        role_id: &str,
        existing: &ConversationRow,
        snapshot: &AssistantSnapshot,
        only_when_agent_changed: bool,
        cancel_active_turn: bool,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<bool, ConversationError> {
        let existing_type: AgentType = string_to_enum(&existing.r#type)?;
        let existing_snapshot = self
            .conversation_repo()
            .get_assistant_snapshot(user_id, &existing.id)
            .await?;
        let mut extra: serde_json::Value =
            serde_json::from_str(&existing.extra).unwrap_or_else(|_| serde_json::json!({}));
        if !extra.is_object() {
            extra = serde_json::json!({});
        }

        let previous_agent_id = existing_snapshot
            .as_ref()
            .map(|row| row.agent_id.as_str())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                extra
                    .get("agent_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
            })
            .map(ToOwned::to_owned);
        let agent_changed =
            existing_type != snapshot.agent_type || previous_agent_id.as_deref() != Some(snapshot.agent_id.as_str());
        if only_when_agent_changed && !agent_changed {
            return Ok(false);
        }

        if cancel_active_turn {
            if let Some(turn_id) = self.runtime_state().active_turn_id_for(&existing.id) {
                let _ = self
                    .cancel_with_cause(
                        user_id,
                        &existing.id,
                        &turn_id,
                        task_manager,
                        TurnCancelCause::RuntimeRestart,
                    )
                    .await;
            }
            task_manager
                .kill_and_wait(&existing.id, Some(AgentKillReason::RuntimeRestart))
                .await;
            self.runtime_state().clear_turn_state_for_restart(&existing.id);
        } else {
            task_manager
                .kill_and_wait(&existing.id, Some(AgentKillReason::RuntimeRestart))
                .await;
        }

        if let Some(ws) = extra
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            && is_temp_session_workspace(Path::new(ws))
        {
            let _ = std::fs::create_dir_all(ws);
        }

        if let Some(obj) = extra.as_object_mut() {
            if !snapshot.runtime_backend.is_empty() {
                obj.insert(
                    "backend".to_owned(),
                    serde_json::Value::String(snapshot.runtime_backend.clone()),
                );
            } else {
                obj.remove("backend");
            }
            if !snapshot.agent_id.is_empty() {
                obj.insert(
                    "agent_id".to_owned(),
                    serde_json::Value::String(snapshot.agent_id.clone()),
                );
            } else {
                obj.remove("agent_id");
            }
            if !snapshot.agent_source.is_empty() {
                obj.insert(
                    "agent_source".to_owned(),
                    serde_json::Value::String(snapshot.agent_source.clone()),
                );
            } else {
                obj.remove("agent_source");
            }
            if agent_changed {
                obj.remove("agent_name");
                obj.remove("cli_path");
            }

            // When healing a pre-existing agent drift (`only_when_agent_changed && agent_changed`),
            // `assistant_preferences` may still carry `last_model_id` / `last_permission_value` /
            // `last_thought_level_value` recorded on the previous agent before the drift occurred.
            // Only carry values explicitly pinned via `"fixed"` mode onto the drifted agent.
            let ignore_stale_auto_prefs = only_when_agent_changed && agent_changed;
            let resolved_model = snapshot
                .resolved_defaults
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .filter(|_| !ignore_stale_auto_prefs || snapshot.default_modes.model == "fixed");
            let resolved_permission = snapshot
                .resolved_defaults
                .permission
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .filter(|_| !ignore_stale_auto_prefs || snapshot.default_modes.permission == "fixed");
            let resolved_thought = snapshot
                .resolved_defaults
                .thought_level
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .filter(|_| !ignore_stale_auto_prefs || snapshot.default_modes.thought_level == "fixed");

            if snapshot.agent_type == AgentType::Aionrs {
                obj.remove("model");
                obj.remove("current_model_id");
                obj.remove("current_mode_id");
                if let Some(permission) = resolved_permission {
                    obj.insert(
                        "session_mode".to_owned(),
                        serde_json::Value::String(permission.to_owned()),
                    );
                } else if agent_changed {
                    obj.remove("session_mode");
                }
            } else {
                if let Some(model_id) = resolved_model {
                    obj.insert(
                        "current_model_id".to_owned(),
                        serde_json::Value::String(model_id.to_owned()),
                    );
                } else {
                    obj.remove("current_model_id");
                }

                if let Some(permission) = resolved_permission {
                    obj.insert(
                        "session_mode".to_owned(),
                        serde_json::Value::String(permission.to_owned()),
                    );
                    if matches!(snapshot.agent_type, AgentType::Acp | AgentType::Antigravity) {
                        obj.insert(
                            "current_mode_id".to_owned(),
                            serde_json::Value::String(permission.to_owned()),
                        );
                    }
                } else if agent_changed {
                    obj.remove("session_mode");
                    obj.remove("current_mode_id");
                }
            }

            if let Some(thought) = resolved_thought {
                obj.insert(
                    "thought_level".to_owned(),
                    serde_json::Value::String(thought.to_owned()),
                );
            } else if agent_changed {
                obj.remove("thought_level");
            }

            if !snapshot.rules.content.trim().is_empty() {
                match snapshot.agent_type {
                    AgentType::Acp | AgentType::Antigravity => {
                        obj.insert(
                            "preset_context".to_owned(),
                            serde_json::Value::String(snapshot.rules.content.clone()),
                        );
                        obj.remove("preset_rules");
                    }
                    AgentType::Aionrs => {
                        obj.insert(
                            "preset_rules".to_owned(),
                            serde_json::Value::String(snapshot.rules.content.clone()),
                        );
                        obj.remove("preset_context");
                    }
                    AgentType::Gemini
                    | AgentType::Codex
                    | AgentType::OpenclawGateway
                    | AgentType::Remote
                    | AgentType::Nanobot => {}
                }
            }
        }

        if agent_changed {
            let mcp_snapshot = self
                .build_runtime_mcp_snapshot(
                    user_id,
                    Some(&snapshot.resolved_defaults.mcp_ids),
                    &[],
                    &[],
                    &snapshot.agent_type,
                    &extra,
                )
                .await?;
            if let Some(obj) = extra.as_object_mut() {
                obj.insert(
                    "mcp_server_ids".to_owned(),
                    serde_json::Value::Array(
                        mcp_snapshot
                            .mcp_server_ids
                            .into_iter()
                            .map(serde_json::Value::String)
                            .collect(),
                    ),
                );
                obj.insert(
                    "mcp_servers".to_owned(),
                    serde_json::Value::Array(
                        mcp_snapshot
                            .mcp_servers
                            .into_iter()
                            .map(serde_json::Value::String)
                            .collect(),
                    ),
                );
                obj.insert(
                    "mcp_statuses".to_owned(),
                    serde_json::to_value(&mcp_snapshot.mcp_statuses).map_err(|e| {
                        ConversationError::internal(format!("Failed to serialize MCP status snapshot: {e}"))
                    })?,
                );
            }
        }

        let ignore_stale_auto_prefs = only_when_agent_changed && agent_changed;
        let effective_model = snapshot
            .resolved_defaults
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|_| !ignore_stale_auto_prefs || snapshot.default_modes.model == "fixed");
        let effective_permission = snapshot
            .resolved_defaults
            .permission
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|_| !ignore_stale_auto_prefs || snapshot.default_modes.permission == "fixed");
        let effective_thought = snapshot
            .resolved_defaults
            .thought_level
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|_| !ignore_stale_auto_prefs || snapshot.default_modes.thought_level == "fixed");

        let new_type_str = snapshot.agent_type.serde_name().to_owned();
        let model_json = if snapshot.agent_type == AgentType::Aionrs {
            match self
                .resolve_aionrs_model_for_role(user_id, role_id, effective_model)
                .await
            {
                Ok(Some(provider_model)) => Some(
                    serde_json::to_string(&provider_model)
                        .map_err(|e| ConversationError::internal(format!("Failed to serialize model: {e}")))?,
                ),
                Ok(None) => None,
                Err(error) => {
                    warn!(
                        conversation_id = %existing.id,
                        role_id,
                        error = %error,
                        "role thread sync: could not resolve enabled provider model for aionrs"
                    );
                    None
                }
            }
        } else {
            None
        };
        let extra_json = serde_json::to_string(&extra)
            .map_err(|e| ConversationError::internal(format!("Failed to serialize extra: {e}")))?;

        self.conversation_repo()
            .update_runtime_binding(
                user_id,
                &existing.id,
                &new_type_str,
                model_json.as_deref(),
                &extra_json,
                now_ms(),
            )
            .await?;

        let resolved_skill_ids = serde_json::to_string(&snapshot.resolved_defaults.skill_ids)
            .map_err(|e| ConversationError::internal(format!("Failed to serialize assistant skill snapshot: {e}")))?;
        let resolved_disabled_builtin_skill_ids =
            serde_json::to_string(&snapshot.resolved_defaults.disabled_builtin_skill_ids).map_err(|e| {
                ConversationError::internal(format!(
                    "Failed to serialize assistant disabled builtin skill snapshot: {e}"
                ))
            })?;
        let resolved_mcp_ids = serde_json::to_string(&snapshot.resolved_defaults.mcp_ids)
            .map_err(|e| ConversationError::internal(format!("Failed to serialize assistant MCP snapshot: {e}")))?;

        self.conversation_repo()
            .upsert_assistant_snapshot(
                user_id,
                &UpsertConversationAssistantSnapshotParams {
                    conversation_id: &existing.id,
                    assistant_definition_id: &snapshot.assistant_definition_id,
                    assistant_id: &snapshot.assistant_id,
                    assistant_source: &snapshot.assistant_source,
                    agent_id: &snapshot.agent_id,
                    rules_content: &snapshot.rules.content,
                    default_model_mode: &snapshot.default_modes.model,
                    resolved_model_id: effective_model,
                    default_permission_mode: &snapshot.default_modes.permission,
                    resolved_permission_value: effective_permission,
                    default_thought_level_mode: &snapshot.default_modes.thought_level,
                    resolved_thought_level_value: effective_thought,
                    default_skills_mode: &snapshot.default_modes.skills,
                    resolved_skill_ids: &resolved_skill_ids,
                    resolved_disabled_builtin_skill_ids: &resolved_disabled_builtin_skill_ids,
                    default_mcps_mode: &snapshot.default_modes.mcps,
                    resolved_mcp_ids: &resolved_mcp_ids,
                },
            )
            .await?;

        match snapshot.agent_type {
            AgentType::Aionrs => {
                let _ = self.acp_session_repo().delete_for_user(user_id, &existing.id).await;
            }
            AgentType::Acp | AgentType::Antigravity => {
                let has_acp_row = self
                    .acp_session_repo()
                    .get_for_user(user_id, &existing.id)
                    .await
                    .map_err(|e| ConversationError::internal(format!("Failed to inspect acp_session row: {e}")))?
                    .is_some();
                if agent_changed || !has_acp_row {
                    let _ = self.acp_session_repo().delete_for_user(user_id, &existing.id).await;
                    self.create_acp_session_row(user_id, &existing.id, &extra, Some(snapshot))
                        .await?;
                } else {
                    let runtime_state = self
                        .acp_session_repo()
                        .load_runtime_state_for_user(user_id, &existing.id)
                        .await
                        .map_err(|e| {
                            ConversationError::internal(format!("Failed to load acp_session runtime state: {e}"))
                        })?;
                    let mut selections = runtime_state
                        .and_then(|state| state.config_selections_json)
                        .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(&raw).ok())
                        .unwrap_or_default();
                    if let Some(model_id) = effective_model {
                        selections.insert("model".to_owned(), model_id.to_owned());
                    } else {
                        selections.remove("model");
                    }
                    if let Some(mode_id) = effective_permission {
                        selections.insert("mode".to_owned(), mode_id.to_owned());
                    }
                    let selections_json = serde_json::to_string(&selections).map_err(|e| {
                        ConversationError::internal(format!("Failed to serialize runtime selections: {e}"))
                    })?;
                    let params = SaveRuntimeStateParams {
                        current_model_id: Some(effective_model),
                        current_mode_id: effective_permission.map(Some),
                        config_selections_json: Some(Some(&selections_json)),
                        context_usage_json: None,
                    };
                    self.acp_session_repo()
                        .save_runtime_state_for_user(user_id, &existing.id, &params)
                        .await
                        .map_err(|e| {
                            ConversationError::internal(format!("Failed to update acp_session runtime state: {e}"))
                        })?;
                }
            }
            AgentType::Gemini
            | AgentType::Codex
            | AgentType::OpenclawGateway
            | AgentType::Remote
            | AgentType::Nanobot => {}
        }

        let source = existing
            .source
            .as_deref()
            .and_then(|raw| string_to_enum::<ConversationSource>(raw).ok());
        self.broadcast_list_changed(user_id, &existing.id, "updated", source.as_ref());

        info!(
            conversation_id = %existing.id,
            role_id,
            agent_id = %snapshot.agent_id,
            agent_type = %new_type_str,
            agent_changed,
            "synchronized resident role thread runtime"
        );
        Ok(true)
    }
}
