use super::*;

async fn run_internal_failure_turn(
    events: Vec<AgentStreamEvent>,
) -> (crate::ConversationAgentTurnOutcome, Vec<String>) {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, _repo) = make_service_with_mock_task_manager(task_mgr.clone());
    let conv = svc
        .create("user_1", make_create_req_with_backend("claude"))
        .await
        .unwrap();
    let agent = Arc::new(ScriptedAgent::new(&conv.id, vec![events]));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent.clone()));
    let outcome = svc
        .run_agent_turn(ConversationAgentTurnRequest {
            user_id: "user_1".into(),
            conversation_id: conv.id,
            content: "queued team work".into(),
            files: vec![],
            inject_skills: vec![],
            required_runtime_mode: None,
            persist_user_message: false,
            user_message_hidden: false,
            on_started: None,
        })
        .await
        .unwrap();
    (outcome, agent.sent_contents())
}

#[tokio::test]
async fn subscription_limit_is_not_replayed_or_authorized_for_outer_retry() {
    let quota = AgentSendError::from_agent_error(AgentError::bad_gateway(
        "You've hit your session limit · resets 11am (America/New_York)",
    ));
    let (outcome, sends) = run_internal_failure_turn(vec![AgentStreamEvent::Error(quota.into_stream_error())]).await;
    assert_eq!(outcome.status, ConversationAgentTurnStatus::Failed);
    assert!(!outcome.auto_retry_allowed);
    assert_eq!(sends, vec!["queued team work"]);
}

#[tokio::test]
async fn clean_transient_provider_failure_authorizes_a_bounded_outer_retry() {
    let transient = AgentSendError::from_agent_error(AgentError::RateLimited);
    let (outcome, sends) =
        run_internal_failure_turn(vec![AgentStreamEvent::Error(transient.into_stream_error())]).await;
    assert_eq!(outcome.status, ConversationAgentTurnStatus::Failed);
    assert!(outcome.auto_retry_allowed);
    assert_eq!(sends.len(), 1);
}

#[tokio::test]
async fn partial_work_blocks_outer_retry_even_when_the_provider_error_is_retryable() {
    let transient = AgentSendError::from_agent_error(AgentError::RateLimited);
    let (outcome, sends) = run_internal_failure_turn(vec![
        AgentStreamEvent::Text(TextEventData {
            content: "partial work already performed".into(),
        }),
        AgentStreamEvent::Error(transient.into_stream_error()),
    ])
    .await;
    assert_eq!(outcome.status, ConversationAgentTurnStatus::Failed);
    assert!(!outcome.auto_retry_allowed);
    assert_eq!(sends.len(), 1);
}

#[tokio::test]
async fn cancellation_during_build_does_not_authorize_outer_retry() {
    let svc = ConversationService::new(
        std::env::temp_dir(),
        Arc::new(MockBroadcaster::new()),
        Arc::new(FixedSkillResolver { names: vec![] }),
        Arc::new(AgentErrorFailingBuildTaskManager::new(AgentError::RateLimited)),
        Arc::new(MockRepo::new()),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
    );
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let runtime = svc.runtime_state();
    let outcome = svc
        .run_agent_turn(ConversationAgentTurnRequest {
            user_id: "user_1".into(),
            conversation_id: conv.id,
            content: "cancelled queued work".into(),
            files: vec![],
            inject_skills: vec![],
            required_runtime_mode: None,
            persist_user_message: false,
            user_message_hidden: false,
            on_started: Some(Arc::new(move |started| {
                runtime.mark_cancelling(&started.conversation_id);
                Box::pin(std::future::ready(()))
            })),
        })
        .await
        .unwrap();
    assert_eq!(outcome.status, ConversationAgentTurnStatus::Failed);
    assert!(!outcome.auto_retry_allowed, "cancelled work cannot authorize replay");
}

#[tokio::test]
async fn failed_internal_replay_does_not_authorize_a_fresh_outer_retry_budget() {
    let (svc, broadcaster, repo, _) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let transient = ErrorEventData {
        retryable: Some(true),
        ..ErrorEventData::legacy("temporary agent disconnect", Some(AgentErrorCode::UnknownUpstreamError))
    };
    let first = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Error(transient.clone())]],
    ));
    let second = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Error(transient)]],
    ));
    let task_mgr = Arc::new(RebuildingScriptedTaskManager::new(vec![
        AgentInstance::Mock(first.clone()),
        AgentInstance::Mock(second.clone()),
    ]));
    let svc = ConversationService::new(
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        task_mgr.clone(),
        repo,
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
    );
    let outcome = svc
        .run_agent_turn(ConversationAgentTurnRequest {
            user_id: "user_1".into(),
            conversation_id: conv.id,
            content: "queued team work".into(),
            files: vec![],
            inject_skills: vec![],
            required_runtime_mode: None,
            persist_user_message: false,
            user_message_hidden: false,
            on_started: None,
        })
        .await
        .unwrap();
    assert_eq!(outcome.status, ConversationAgentTurnStatus::Failed);
    assert!(!outcome.auto_retry_allowed);
    assert_eq!(task_mgr.build_count(), 2);
    assert_eq!(first.sent_contents(), vec!["queued team work"]);
    assert_eq!(second.sent_contents(), vec!["queued team work"]);
}

#[tokio::test]
async fn send_message_evicts_acp_task_after_terminal_error() {
    let (svc, _broadcaster, _repo, _default_task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
            "mock terminal error",
            Some(AgentErrorCode::UnknownUpstreamError),
        ))]],
    ));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if task_mgr.kill_count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("ACP terminal error should evict the cached task");
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.active_count(), 0);
    assert_eq!(
        task_mgr.kill_records(),
        vec![(conv.id.clone(), Some(AgentKillReason::AgentErrorRecovery))]
    );
}

async fn assert_terminal_model_error_clears_persisted_acp_model(error_code: AgentErrorCode, error_label: &str) {
    let acp_session_repo = Arc::new(StubAcpSessionRepo::default());
    let (svc, _broadcaster, repo, _default_task_mgr) = make_service_with_resolver_and_acp_session_repo(
        Arc::new(FixedSkillResolver { names: vec![] }),
        acp_session_repo.clone(),
    );
    let task_mgr = Arc::new(MockTaskManager::new());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let workspace = ensure_test_workspace_path();
    repo.update(
        "user_1",
        &conv.id,
        &ConversationRowUpdate {
            extra: Some(
                serde_json::to_string(&json!({
                    "workspace": workspace,
                    "current_model_id": "deepseek-v4-pro",
                }))
                .unwrap(),
            ),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
            "The configured model cannot be used with the provider.",
            Some(error_code),
        ))]],
    ));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let saves = acp_session_repo.runtime_state_saves();
            if saves
                .iter()
                .any(|call| call.conversation_id == conv.id && call.current_model_id == Some(None))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{error_label} should clear persisted ACP model"));
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.active_count(), 0);
    assert_eq!(
        acp_session_repo.runtime_state_saves(),
        vec![RuntimeStateSaveCall {
            conversation_id: conv.id.clone(),
            current_mode_id: None,
            current_model_id: Some(None),
            config_selections_json: None,
        }]
    );

    let row = repo.get("user_1", &conv.id).await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
    assert!(extra.get("workspace").is_some());
    assert!(
        extra.get("current_model_id").is_none(),
        "{error_label} must clear conversation.extra.current_model_id so rebuild cannot reseed stale desired model"
    );
}

#[tokio::test]
async fn send_message_clears_persisted_acp_model_after_model_not_found() {
    assert_terminal_model_error_clears_persisted_acp_model(
        AgentErrorCode::UserLlmProviderModelNotFound,
        "model_not_found",
    )
    .await;
}

#[tokio::test]
async fn send_message_clears_persisted_acp_model_after_unsupported_model() {
    assert_terminal_model_error_clears_persisted_acp_model(
        AgentErrorCode::UserLlmProviderUnsupportedModel,
        "unsupported_model",
    )
    .await;
}

#[tokio::test]
async fn send_message_does_not_clear_persisted_acp_model_for_other_terminal_errors() {
    let acp_session_repo = Arc::new(StubAcpSessionRepo::default());
    let (svc, _broadcaster, _repo, _default_task_mgr) = make_service_with_resolver_and_acp_session_repo(
        Arc::new(FixedSkillResolver { names: vec![] }),
        acp_session_repo.clone(),
    );
    let task_mgr = Arc::new(MockTaskManager::new());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
            "Unknown upstream error.",
            Some(AgentErrorCode::UnknownUpstreamError),
        ))]],
    ));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.active_count(), 0);
    assert!(acp_session_repo.runtime_state_saves().is_empty());
}
