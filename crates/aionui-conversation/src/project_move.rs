//! Moving a conversation changes its runtime workspace as well as its sidebar owner.
//! Prepare the replacement against a candidate row BEFORE persisting the binding:
//! startup failures therefore leave the original binding and history untouched.

use super::*;
use aionui_api_types::MoveConversationToProjectRequest;
use aionui_common::AgentKillReason;
use aionui_project::{RuntimeStatus, canonical};

struct MoveGuard {
    state: Arc<ConversationRuntimeStateService>,
    conversation_id: String,
}

impl Drop for MoveGuard {
    fn drop(&mut self) {
        self.state.clear_restarting(&self.conversation_id);
    }
}

impl ConversationService {
    /// Move an idle standalone conversation without replacing its visible history.
    /// The owned task finishes even if the requesting browser disconnects.
    pub async fn move_to_project(
        &self,
        user_id: &str,
        conversation_id: &str,
        request: MoveConversationToProjectRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<ConversationResponse, ConversationError> {
        // Authorize before acquiring a gate or touching any runtime.
        let row = self
            .conversation_repo
            .get(user_id, conversation_id)
            .await?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;
        if team_id_from_extra(&row.extra).is_some() {
            return Err(ConversationError::BadRequest {
                reason: "Team member conversations cannot be moved independently".into(),
            });
        }
        if request.project_id.is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "A target project is required".into(),
            });
        }
        self.runtime_state.begin_restart(conversation_id)?;
        let guard = MoveGuard {
            state: self.runtime_state.clone(),
            conversation_id: conversation_id.to_owned(),
        };
        let service = self.clone();
        let user_id = user_id.to_owned();
        let manager = task_manager.clone();
        tokio::spawn(async move {
            let _guard = guard;
            service.move_to_project_inner(&user_id, row, request, &manager).await
        })
        .await
        .map_err(|_| ConversationError::internal("Project move task failed"))?
    }

    async fn move_to_project_inner(
        &self,
        user_id: &str,
        row: aionui_db::models::ConversationRow,
        request: MoveConversationToProjectRequest,
        manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<ConversationResponse, ConversationError> {
        let id = row.id.clone();
        let mut row = self
            .conversation_repo
            .get(user_id, &id)
            .await?
            .ok_or_else(|| ConversationError::NotFound { id: id.clone() })?;
        if self.runtime_state.is_claimed(&id) || self.runtime_state.is_cancelling(&id) {
            return Err(ConversationError::Busy {
                reason: "Finish the current turn before moving this conversation".into(),
            });
        }
        if let Some(agent) = manager.get_task(&id)
            && (!agent.get_confirmations().is_empty() || agent.live_background_tasks() > 0)
        {
            return Err(ConversationError::Busy {
                reason: "Resolve pending confirmations before moving this conversation".into(),
            });
        }
        let mut extra: serde_json::Value = serde_json::from_str(&row.extra)
            .map_err(|_| ConversationError::internal("Invalid conversation metadata"))?;
        if extra.get("workspace").and_then(serde_json::Value::as_str) != Some(request.expected_workspace.as_str()) {
            return Err(ConversationError::Busy {
                reason: "The conversation workspace changed; refresh before moving it".into(),
            });
        }
        let projects = self
            .project_service
            .read()
            .ok()
            .and_then(|p| p.clone())
            .ok_or_else(|| ConversationError::internal("Project service is unavailable"))?;
        let target = projects.get_project(user_id, &request.project_id).await.map_err(|_| {
            ConversationError::NotFoundReason {
                reason: "Target project is unavailable".into(),
            }
        })?;
        if target.kind != "standard" {
            return Err(ConversationError::BadRequest {
                reason: "The target must be a standard project".into(),
            });
        }
        let workspace = target
            .explorer
            .entries
            .iter()
            .find(|e| e.role == "workspace")
            .ok_or_else(|| ConversationError::internal("Project workspace is unavailable"))?;
        if workspace.folder.runtime_status != RuntimeStatus::Available {
            return Err(ConversationError::BadRequest {
                reason: "The target workspace is not accessible".into(),
            });
        }
        let path = canonical::uri_to_path(&workspace.folder.resource_uri)
            .map_err(|_| ConversationError::internal("Invalid project workspace"))?;
        let path = path.to_string_lossy().into_owned();
        if row.project_id.as_deref() == Some(&request.project_id)
            && extra.get("workspace").and_then(serde_json::Value::as_str) == Some(path.as_str())
        {
            return row_to_response(row, &self.workspace_root);
        }
        extra["workspace"] = serde_json::json!(path);
        extra["custom_workspace"] = serde_json::json!(true);
        extra["is_temporary_workspace"] = serde_json::json!(false);
        row.extra = extra.to_string();
        row.project_id = Some(target.id);
        row.folder_id = Some(workspace.folder_id.clone());
        row.updated_at = now_ms();
        let mut options = self.build_task_options(&row).await?;
        self.apply_conversation_runtime_context(&mut options, user_id, &id);
        self.ensure_session_skill_view(&options.context).await;
        info!(conversation_id = %id, target_project_id = %request.project_id, "Preparing conversation project move");
        manager.kill_and_wait(&id, Some(AgentKillReason::RuntimeRestart)).await;
        // The persisted backend resume anchor is deliberately not reset. Startup
        // must recover it; a failed startup must not manufacture an empty chat.
        let prepared = async {
            let agent = manager.get_or_build_task(&id, options).await?;
            if Path::new(agent.workspace()) != Path::new(&path) {
                return Err(ConversationError::internal(
                    "Replacement runtime selected an unexpected workspace",
                ));
            }
            agent.get_config_options().await?;
            let response = row_to_response(row.clone(), &self.workspace_root)?;
            self.conversation_repo
                .update(
                    user_id,
                    &id,
                    &ConversationRowUpdate {
                        project_id: row.project_id.clone(),
                        folder_id: row.folder_id.clone(),
                        extra: Some(row.extra.clone()),
                        updated_at: Some(row.updated_at),
                        ..Default::default()
                    },
                )
                .await?;
            self.ensure_background_watcher(user_id, &id, &agent);
            Ok::<_, ConversationError>(response)
        }
        .await;
        match prepared {
            Ok(response) => {
                self.broadcast_list_changed(user_id, &id, "updated", response.source.as_ref());
                info!(conversation_id = %id, target_project_id = %request.project_id, "Conversation project move completed");
                Ok(response)
            }
            Err(error) => {
                manager.kill_and_wait(&id, Some(AgentKillReason::RuntimeRestart)).await;
                warn!(conversation_id = %id, "Conversation project move failed; original binding retained");
                Err(error)
            }
        }
    }
}
