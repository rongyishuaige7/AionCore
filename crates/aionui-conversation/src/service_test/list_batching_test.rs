use super::*;
use aionui_db::models::AssistantDefinitionRow;
use aionui_db::{SqliteAcpSessionRepository, SqliteConversationRepository};

struct CountingSkillResolver {
    names: Vec<String>,
    auto_inject_calls: Arc<AtomicUsize>,
}

impl CountingSkillResolver {
    fn new(names: Vec<String>) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                names,
                auto_inject_calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait::async_trait]
impl SkillResolver for CountingSkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        self.auto_inject_calls.fetch_add(1, Ordering::SeqCst);
        self.names.clone()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<ResolvedAgentSkill> {
        vec![]
    }

    async fn sync_skill_view(&self, _user_id: &str, _conversation_id: &str, _skills: &[ResolvedAgentSkill]) -> usize {
        0
    }

    async fn remove_skill_view(&self, _user_id: &str, _conversation_id: &str) {}
}

struct CountingAgentMetadataRepo {
    list_for_user_calls: Arc<AtomicUsize>,
}

impl CountingAgentMetadataRepo {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                list_for_user_calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait::async_trait]
impl IAgentMetadataRepository for CountingAgentMetadataRepo {
    async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo.list_all().await
    }

    async fn list_all_for_user(&self, user_id: &str) -> Result<Vec<AgentMetadataRow>, DbError> {
        self.list_for_user_calls.fetch_add(1, Ordering::SeqCst);
        StubAgentMetadataRepo.list_all_for_user(user_id).await
    }

    async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo.get(id).await
    }

    async fn get_for_user(&self, user_id: &str, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo.get_for_user(user_id, id).await
    }

    async fn find_by_source_and_name(
        &self,
        agent_source: &str,
        name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo.find_by_source_and_name(agent_source, name).await
    }

    async fn find_by_source_and_name_for_user(
        &self,
        user_id: &str,
        agent_source: &str,
        name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo
            .find_by_source_and_name_for_user(user_id, agent_source, name)
            .await
    }

    async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo.find_builtin_by_backend(backend).await
    }

    async fn find_builtin_by_backend_for_user(
        &self,
        user_id: &str,
        backend: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo
            .find_builtin_by_backend_for_user(user_id, backend)
            .await
    }

    async fn upsert(&self, params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
        StubAgentMetadataRepo.upsert(params).await
    }

    async fn upsert_for_user(
        &self,
        user_id: &str,
        params: &UpsertAgentMetadataParams<'_>,
    ) -> Result<AgentMetadataRow, DbError> {
        StubAgentMetadataRepo.upsert_for_user(user_id, params).await
    }

    async fn apply_handshake(
        &self,
        id: &str,
        params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo.apply_handshake(id, params).await
    }

    async fn apply_handshake_for_user(
        &self,
        user_id: &str,
        id: &str,
        params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo
            .apply_handshake_for_user(user_id, id, params)
            .await
    }

    async fn update_availability_snapshot(
        &self,
        id: &str,
        params: &UpdateAgentAvailabilitySnapshotParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo.update_availability_snapshot(id, params).await
    }

    async fn update_availability_snapshot_for_user(
        &self,
        user_id: &str,
        id: &str,
        params: &UpdateAgentAvailabilitySnapshotParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        StubAgentMetadataRepo
            .update_availability_snapshot_for_user(user_id, id, params)
            .await
    }

    async fn update_agent_overrides(
        &self,
        id: &str,
        command_override: Option<&str>,
        env_override: Option<&str>,
    ) -> Result<(), DbError> {
        StubAgentMetadataRepo
            .update_agent_overrides(id, command_override, env_override)
            .await
    }

    async fn update_agent_overrides_for_user(
        &self,
        user_id: &str,
        id: &str,
        command_override: Option<&str>,
        env_override: Option<&str>,
    ) -> Result<(), DbError> {
        StubAgentMetadataRepo
            .update_agent_overrides_for_user(user_id, id, command_override, env_override)
            .await
    }

    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, DbError> {
        StubAgentMetadataRepo.set_enabled(id, enabled).await
    }

    async fn set_enabled_for_user(&self, user_id: &str, id: &str, enabled: bool) -> Result<bool, DbError> {
        StubAgentMetadataRepo.set_enabled_for_user(user_id, id, enabled).await
    }

    async fn delete(&self, id: &str) -> Result<bool, DbError> {
        StubAgentMetadataRepo.delete(id).await
    }

    async fn delete_for_user(&self, user_id: &str, id: &str) -> Result<bool, DbError> {
        StubAgentMetadataRepo.delete_for_user(user_id, id).await
    }
}

struct CountingAssistantDefinitionRepo {
    inner: SqliteAssistantDefinitionRepository,
    list_for_user_calls: Arc<AtomicUsize>,
    get_by_assistant_id_calls: Arc<AtomicUsize>,
}

impl CountingAssistantDefinitionRepo {
    fn new(inner: SqliteAssistantDefinitionRepository) -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let list_calls = Arc::new(AtomicUsize::new(0));
        let get_calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                inner,
                list_for_user_calls: list_calls.clone(),
                get_by_assistant_id_calls: get_calls.clone(),
            },
            list_calls,
            get_calls,
        )
    }
}

#[async_trait::async_trait]
impl IAssistantDefinitionRepository for CountingAssistantDefinitionRepo {
    async fn list(&self) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        self.inner.list().await
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        self.list_for_user_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.list_for_user(user_id).await
    }

    async fn list_including_deleted_for_user(&self, user_id: &str) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        self.inner.list_including_deleted_for_user(user_id).await
    }

    async fn get_by_assistant_id(&self, assistant_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.inner.get_by_assistant_id(assistant_id).await
    }

    async fn get_by_assistant_id_for_user(
        &self,
        user_id: &str,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_assistant_id_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.get_by_assistant_id_for_user(user_id, assistant_id).await
    }

    async fn get_by_assistant_id_including_deleted_for_user(
        &self,
        user_id: &str,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.inner
            .get_by_assistant_id_including_deleted_for_user(user_id, assistant_id)
            .await
    }

    async fn get_by_id(&self, id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.inner.get_by_id(id).await
    }

    async fn get_by_id_for_user(&self, user_id: &str, id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.inner.get_by_id_for_user(user_id, id).await
    }

    async fn get_by_source_ref(
        &self,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.inner.get_by_source_ref(source, source_ref).await
    }

    async fn get_by_source_ref_for_user(
        &self,
        user_id: &str,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.inner.get_by_source_ref_for_user(user_id, source, source_ref).await
    }

    async fn get_by_source_ref_including_deleted_for_user(
        &self,
        user_id: &str,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.inner
            .get_by_source_ref_including_deleted_for_user(user_id, source, source_ref)
            .await
    }

    async fn upsert(&self, params: &UpsertAssistantDefinitionParams<'_>) -> Result<AssistantDefinitionRow, DbError> {
        self.inner.upsert(params).await
    }

    async fn upsert_for_user(
        &self,
        user_id: &str,
        params: &UpsertAssistantDefinitionParams<'_>,
    ) -> Result<AssistantDefinitionRow, DbError> {
        self.inner.upsert_for_user(user_id, params).await
    }

    async fn soft_delete(&self, id: &str, deleted_at: i64) -> Result<bool, DbError> {
        self.inner.soft_delete(id, deleted_at).await
    }

    async fn soft_delete_for_user(&self, user_id: &str, id: &str, deleted_at: i64) -> Result<bool, DbError> {
        self.inner.soft_delete_for_user(user_id, id, deleted_at).await
    }
}

async fn seed_user(pool: &sqlx::SqlitePool, user_id: &str) {
    sqlx::query(
        "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
         VALUES (?, 'local', ?, 'hash', 'active', 0, 1, 1)",
    )
    .bind(user_id)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn list_skips_skill_resolver_and_batches_assistant_lookups() {
    let db = init_database_memory().await.unwrap();
    seed_user(db.pool(), "user_1").await;

    let conv_repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let raw_def_repo = SqliteAssistantDefinitionRepository::new(db.pool().clone());
    raw_def_repo
        .upsert_for_user(
            "user_1",
            &UpsertAssistantDefinitionParams {
                id: "def_alpha",
                assistant_id: "role-alpha",
                source: "user",
                owner_type: "user",
                source_ref: Some("role-alpha"),
                name: "Alpha Assistant",
                name_i18n: "{}",
                description: None,
                description_i18n: "{}",
                avatar_type: "emoji",
                avatar_value: Some("🦊"),
                agent_id: "8e1acf31",
                rule_resource_type: "user_file",
                rule_resource_ref: None,
                recommended_prompts: "[]",
                recommended_prompts_i18n: "{}",
                default_model_mode: "auto",
                default_model_value: None,
                default_permission_mode: "auto",
                default_permission_value: None,
                default_thought_level_mode: "auto",
                default_thought_level_value: None,
                default_skills_mode: "auto",
                default_skill_ids: "[]",
                custom_skill_names: "[]",
                default_disabled_builtin_skill_ids: "[]",
                default_mcps_mode: "auto",
                default_mcp_ids: "[]",
            },
        )
        .await
        .unwrap();

    raw_def_repo
        .upsert_for_user(
            "user_1",
            &UpsertAssistantDefinitionParams {
                id: "def_missing",
                assistant_id: "role-missing",
                source: "user",
                owner_type: "user",
                source_ref: Some("role-missing"),
                name: "Deleted Assistant",
                name_i18n: "{}",
                description: None,
                description_i18n: "{}",
                avatar_type: "emoji",
                avatar_value: None,
                agent_id: "8e1acf31",
                rule_resource_type: "user_file",
                rule_resource_ref: None,
                recommended_prompts: "[]",
                recommended_prompts_i18n: "{}",
                default_model_mode: "auto",
                default_model_value: None,
                default_permission_mode: "auto",
                default_permission_value: None,
                default_thought_level_mode: "auto",
                default_thought_level_value: None,
                default_skills_mode: "auto",
                default_skill_ids: "[]",
                custom_skill_names: "[]",
                default_disabled_builtin_skill_ids: "[]",
                default_mcps_mode: "auto",
                default_mcp_ids: "[]",
            },
        )
        .await
        .unwrap();
    raw_def_repo
        .soft_delete_for_user("user_1", "def_missing", 999)
        .await
        .unwrap();

    let (skill_resolver, auto_inject_calls) = CountingSkillResolver::new(vec!["auto-skill".into()]);
    let (agent_metadata_repo, agent_metadata_calls) = CountingAgentMetadataRepo::new();
    let (def_repo, def_list_calls, def_get_calls) = CountingAssistantDefinitionRepo::new(raw_def_repo);

    let tmp = tempfile::tempdir().unwrap();
    let service = ConversationService::new(
        tmp.path().to_path_buf(),
        Arc::new(MockBroadcaster::new()),
        Arc::new(skill_resolver),
        Arc::new(MockTaskManager::new()),
        conv_repo.clone(),
        Arc::new(agent_metadata_repo),
        Arc::new(SqliteAcpSessionRepository::new(db.pool().clone())),
    );
    service.with_assistant_definition_repo(Arc::new(def_repo));

    // Seed 5 conversations that already have `extra.skills`:
    // - 3 bound to existing `role-alpha`
    // - 2 bound to a soft-deleted `role-missing` (verifies negative caching avoids repeated fallback queries)
    for idx in 0..5 {
        let conv_id = format!("conv_batch_{idx}");
        let ts = 1_000 + idx as i64;
        conv_repo
            .create(&ConversationRow {
                id: conv_id.clone(),
                user_id: "user_1".into(),
                name: format!("Chat {idx}"),
                r#type: "acp".into(),
                model: None,
                extra: json!({"workspace": "/tmp/ws", "backend": "codex", "skills": ["existing-skill"]}).to_string(),
                status: Some("finished".into()),
                source: Some("aionui".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: ts,
                updated_at: ts,
                project_id: None,
                folder_id: None,
                name_source: None,
            })
            .await
            .unwrap();

        let (assistant_id, def_id) = if idx < 3 {
            ("role-alpha", "def_alpha")
        } else {
            ("role-missing", "def_missing")
        };
        conv_repo
            .upsert_assistant_snapshot(
                "user_1",
                &UpsertConversationAssistantSnapshotParams {
                    conversation_id: &conv_id,
                    assistant_definition_id: def_id,
                    assistant_id,
                    assistant_source: "user",
                    agent_id: "8e1acf31",
                    rules_content: "",
                    default_model_mode: "auto",
                    resolved_model_id: None,
                    default_permission_mode: "auto",
                    resolved_permission_value: None,
                    default_thought_level_mode: "auto",
                    resolved_thought_level_value: None,
                    default_skills_mode: "auto",
                    resolved_skill_ids: "[]",
                    resolved_disabled_builtin_skill_ids: "[]",
                    default_mcps_mode: "auto",
                    resolved_mcp_ids: "[]",
                },
            )
            .await
            .unwrap();
    }

    let page = service
        .list(
            "user_1",
            ListConversationsQuery {
                limit: Some(100),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 5);
    assert_eq!(auto_inject_calls.load(Ordering::SeqCst), 0);
    assert_eq!(agent_metadata_calls.load(Ordering::SeqCst), 1);
    assert_eq!(def_list_calls.load(Ordering::SeqCst), 1);
    // Only 1 fallback lookup for `role-missing`, reused by the second `role-missing` conversation.
    assert_eq!(def_get_calls.load(Ordering::SeqCst), 1);

    let alpha_item = page.items.iter().find(|c| c.id == "conv_batch_0").unwrap();
    let alpha_assistant = alpha_item.assistant.as_ref().expect("resolved assistant");
    assert_eq!(alpha_assistant.id, "role-alpha");
    assert_eq!(alpha_assistant.name, "Alpha Assistant");
    assert_eq!(alpha_assistant.backend, "codex");

    let missing_item = page.items.iter().find(|c| c.id == "conv_batch_4").unwrap();
    let missing_assistant = missing_item.assistant.as_ref().expect("fallback assistant");
    assert_eq!(missing_assistant.id, "role-missing");
    assert_eq!(missing_assistant.name, "role-missing");
    assert_eq!(missing_assistant.backend, "codex");
}

#[tokio::test]
async fn list_fetches_auto_inject_skills_once_across_multiple_legacy_rows() {
    let db = init_database_memory().await.unwrap();
    seed_user(db.pool(), "user_1").await;

    let conv_repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let (skill_resolver, auto_inject_calls) = CountingSkillResolver::new(vec!["core-skill".into()]);
    let (agent_metadata_repo, agent_metadata_calls) = CountingAgentMetadataRepo::new();

    let tmp = tempfile::tempdir().unwrap();
    let service = ConversationService::new(
        tmp.path().to_path_buf(),
        Arc::new(MockBroadcaster::new()),
        Arc::new(skill_resolver),
        Arc::new(MockTaskManager::new()),
        conv_repo.clone(),
        Arc::new(agent_metadata_repo),
        Arc::new(SqliteAcpSessionRepository::new(db.pool().clone())),
    );

    // Seed 4 legacy rows missing `extra.skills` and without assistant snapshots.
    for idx in 0..4 {
        let ts = 2_000 + idx as i64;
        conv_repo
            .create(&ConversationRow {
                id: format!("conv_legacy_{idx}"),
                user_id: "user_1".into(),
                name: format!("Legacy {idx}"),
                r#type: "acp".into(),
                model: None,
                extra: json!({"workspace": "/tmp/ws", "backend": "codex"}).to_string(),
                status: Some("finished".into()),
                source: Some("aionui".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: ts,
                updated_at: ts,
                project_id: None,
                folder_id: None,
                name_source: None,
            })
            .await
            .unwrap();
    }

    let page = service
        .list(
            "user_1",
            ListConversationsQuery {
                limit: Some(100),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 4);
    assert_eq!(auto_inject_calls.load(Ordering::SeqCst), 1);
    assert_eq!(agent_metadata_calls.load(Ordering::SeqCst), 0);
    for item in &page.items {
        assert_eq!(item.extra["skills"], json!(["core-skill"]));
    }
}
