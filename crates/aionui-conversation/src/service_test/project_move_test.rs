use super::*;
use aionui_api_types::MoveConversationToProjectRequest;
use aionui_db::{SqliteAcpSessionRepository, SqliteConversationRepository, SqliteProjectStore};
use aionui_project::{ProjectService, canonical};
use std::{future::Future, pin::Pin};

struct MoveManager {
    inner: MockTaskManager,
    fail: AtomicBool,
    hold: AtomicBool,
    started: Notify,
    release: Notify,
    built_workspaces: Mutex<Vec<String>>,
}

impl MoveManager {
    fn new() -> Self {
        Self {
            inner: MockTaskManager::new(),
            fail: AtomicBool::new(false),
            hold: AtomicBool::new(false),
            started: Notify::new(),
            release: Notify::new(),
            built_workspaces: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for MoveManager {
    fn get_task(&self, id: &str) -> Option<AgentInstance> {
        self.inner.get_task(id)
    }
    async fn get_or_build_task(&self, id: &str, options: BuildTaskOptions) -> Result<AgentInstance, AgentError> {
        self.built_workspaces
            .lock()
            .unwrap()
            .push(options.context.workspace.path.clone());
        self.started.notify_one();
        if self.hold.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(AgentError::bad_gateway("test startup failure"));
        }
        let mut agent = MockAgent::new(id);
        agent.workspace_override = Some(options.context.workspace.path);
        let instance = AgentInstance::Mock(Arc::new(agent));
        self.inner.insert_agent(id, instance.clone());
        Ok(instance)
    }
    fn kill(&self, id: &str, reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.inner.kill(id, reason)
    }
    fn kill_and_wait(&self, id: &str, reason: Option<AgentKillReason>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        self.inner.kill_and_wait(id, reason)
    }
    async fn clear(&self) {
        self.inner.clear().await;
    }
    fn active_count(&self) -> usize {
        self.inner.active_count()
    }
    fn active_conversation_ids(&self) -> Vec<String> {
        self.inner.active_conversation_ids()
    }
    fn collect_idle(&self, threshold: TimestampMs) -> Vec<String> {
        self.inner.collect_idle(threshold)
    }
}

struct MoveFixture {
    service: ConversationService,
    repo: Arc<SqliteConversationRepository>,
    manager: Arc<MoveManager>,
    target: String,
    source: String,
    destination: String,
    _db: aionui_db::Database,
    _source_dir: tempfile::TempDir,
    _destination_dir: tempfile::TempDir,
}

impl MoveFixture {
    async fn new() -> Self {
        let db = init_database_memory().await.unwrap();
        for user in ["user_1", "user_2"] {
            sqlx::query("INSERT INTO users (id,user_type,username,password_hash,status,session_generation,created_at,updated_at) VALUES (?,'local',?,'hash','active',0,1,1)")
                .bind(user).bind(user).execute(db.pool()).await.unwrap();
        }
        let source_dir = tempfile::tempdir().unwrap();
        let destination_dir = tempfile::tempdir().unwrap();
        let projects = Arc::new(ProjectService::new(
            Arc::new(SqliteProjectStore::new(db.pool().clone())),
            source_dir.path().join("managed"),
        ));
        let target = projects
            .resolve_existing("user_1", canonical::to_file_uri(destination_dir.path()).unwrap())
            .await
            .unwrap()
            .project
            .project_id;
        let source = source_dir.path().to_string_lossy().into_owned();
        let destination = destination_dir.path().to_string_lossy().into_owned();
        let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        repo.create(&ConversationRow {
            id: "moving".into(),
            user_id: "user_1".into(),
            name: "Keep this title".into(),
            r#type: "acp".into(),
            model: None,
            extra: json!({"workspace":source,"backend":"claude","skills":["retained-skill"]}).to_string(),
            status: Some("finished".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: true,
            pinned_at: Some(1),
            created_at: 1,
            updated_at: 1,
            project_id: None,
            folder_id: None,
            name_source: Some("user".into()),
        })
        .await
        .unwrap();
        repo.insert_message(
            "user_1",
            &MessageRow {
                id: "history".into(),
                conversation_id: "moving".into(),
                msg_id: None,
                r#type: "text".into(),
                content: json!({"content":"Remember earlier work"}).to_string(),
                position: Some("right".into()),
                status: Some("finish".into()),
                hidden: false,
                created_at: 2,
                backend_turn_id: Some("native-history-anchor".into()),
            },
        )
        .await
        .unwrap();
        let manager = Arc::new(MoveManager::new());
        let service = ConversationService::new(
            source_dir.path().join("app"),
            Arc::new(MockBroadcaster::new()),
            Arc::new(FixedSkillResolver { names: vec![] }),
            manager.clone(),
            repo.clone(),
            Arc::new(StubAgentMetadataRepo),
            Arc::new(SqliteAcpSessionRepository::new(db.pool().clone())),
        );
        service.with_project_service(projects);
        Self {
            service,
            repo,
            manager,
            target,
            source,
            destination,
            _db: db,
            _source_dir: source_dir,
            _destination_dir: destination_dir,
        }
    }
    fn request(&self) -> MoveConversationToProjectRequest {
        MoveConversationToProjectRequest {
            project_id: self.target.clone(),
            expected_workspace: self.source.clone(),
        }
    }
    fn manager(&self) -> Arc<dyn IWorkerTaskManager> {
        self.manager.clone()
    }
}

#[tokio::test]
async fn moves_the_runtime_and_binding_while_preserving_history_pin_and_skills() {
    let f = MoveFixture::new().await;
    let moved = f
        .service
        .move_to_project("user_1", "moving", f.request(), &f.manager())
        .await
        .unwrap();
    assert_eq!(moved.project_id.as_deref(), Some(f.target.as_str()));
    assert_eq!(
        f.manager.built_workspaces.lock().unwrap().as_slice(),
        std::slice::from_ref(&f.destination)
    );
    let row = f.repo.get("user_1", "moving").await.unwrap().unwrap();
    assert!(row.pinned);
    assert_eq!(row.name, "Keep this title");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&row.extra).unwrap()["skills"],
        json!(["retained-skill"])
    );
    let history = f
        .repo
        .list_messages_page(
            "user_1",
            "moving",
            &MessagePageParams {
                limit: 10,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        history.items[0].backend_turn_id.as_deref(),
        Some("native-history-anchor")
    );
}

#[tokio::test]
async fn failed_startup_keeps_the_original_unbound_workspace_and_releases_the_gate() {
    let f = MoveFixture::new().await;
    f.manager.fail.store(true, Ordering::SeqCst);
    let result = f
        .service
        .move_to_project("user_1", "moving", f.request(), &f.manager())
        .await;
    assert!(matches!(result, Err(ConversationError::BadGateway { .. })));
    let row = f.repo.get("user_1", "moving").await.unwrap().unwrap();
    assert_eq!(row.project_id, None);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&row.extra).unwrap()["workspace"],
        f.source
    );
    assert!(!f.service.runtime_state().is_restarting("moving"));
    assert!(f.manager.get_task("moving").is_none());
}

#[tokio::test]
async fn active_turn_and_stale_drag_are_rejected_without_killing_a_runtime() {
    let f = MoveFixture::new().await;
    let claim = f.service.runtime_state().try_claim_turn("moving", "turn-1").unwrap();
    assert!(matches!(
        f.service
            .move_to_project("user_1", "moving", f.request(), &f.manager())
            .await,
        Err(ConversationError::Busy { .. })
    ));
    drop(claim);
    let mut stale = f.request();
    stale.expected_workspace = "/outdated".into();
    assert!(matches!(
        f.service.move_to_project("user_1", "moving", stale, &f.manager()).await,
        Err(ConversationError::Busy { .. })
    ));
    assert_eq!(f.manager.inner.kill_count(), 0);
}

#[tokio::test]
async fn foreign_conversation_and_foreign_project_are_rejected_before_runtime_mutation() {
    let f = MoveFixture::new().await;
    assert!(matches!(
        f.service
            .move_to_project("user_2", "moving", f.request(), &f.manager())
            .await,
        Err(ConversationError::NotFound { .. })
    ));
    let mut foreign = f.request();
    foreign.project_id = "not-owned".into();
    assert!(matches!(
        f.service
            .move_to_project("user_1", "moving", foreign, &f.manager())
            .await,
        Err(ConversationError::NotFoundReason { .. })
    ));
    assert_eq!(f.manager.inner.kill_count(), 0);
}

#[tokio::test]
async fn a_move_excludes_sends_and_other_moves_until_the_new_workspace_is_ready() {
    let f = MoveFixture::new().await;
    f.manager.hold.store(true, Ordering::SeqCst);
    let service = f.service.clone();
    let manager = f.manager();
    let request = f.request();
    let first = tokio::spawn(async move { service.move_to_project("user_1", "moving", request, &manager).await });
    f.manager.started.notified().await;
    assert!(matches!(
        f.service.runtime_state().try_claim_turn("moving", "new-turn"),
        Err(ConversationError::RuntimeRestarting { .. })
    ));
    assert!(matches!(
        f.service
            .move_to_project("user_1", "moving", f.request(), &f.manager())
            .await,
        Err(ConversationError::RuntimeRestarting { .. })
    ));
    f.manager.release.notify_one();
    first.await.unwrap().unwrap();
    assert!(!f.service.runtime_state().is_restarting("moving"));
}

#[tokio::test]
async fn disconnecting_the_http_request_does_not_abandon_a_move_halfway() {
    let f = MoveFixture::new().await;
    f.manager.hold.store(true, Ordering::SeqCst);
    let service = f.service.clone();
    let manager = f.manager();
    let request = f.request();
    let request_task =
        tokio::spawn(async move { service.move_to_project("user_1", "moving", request, &manager).await });
    f.manager.started.notified().await;
    request_task.abort();
    f.manager.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        while f.service.runtime_state().is_restarting("moving") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.repo
            .get("user_1", "moving")
            .await
            .unwrap()
            .unwrap()
            .project_id
            .as_deref(),
        Some(f.target.as_str())
    );
}
