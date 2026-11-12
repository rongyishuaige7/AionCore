//! Best-effort title summaries after completed, persisted replies.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aionui_ai_agent::title::TitleGenerator;
use aionui_db::models::{ConversationRow, MessageRow};
use aionui_db::{IConversationRepository, MessagePageDirection, MessagePageParams};
use aionui_realtime::EventBroadcaster;
use tokio::sync::Semaphore;
use tracing::{info, warn};

static RUN_SLOTS: std::sync::LazyLock<Semaphore> = std::sync::LazyLock::new(|| Semaphore::new(2));
static QUEUE_SLOTS: std::sync::LazyLock<Arc<Semaphore>> = std::sync::LazyLock::new(|| Arc::new(Semaphore::new(64)));

#[derive(Clone)]
pub(crate) struct ConversationTitles {
    repo: Arc<dyn IConversationRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    latest: Arc<Mutex<HashMap<String, (String, u64)>>>,
    sequence: Arc<AtomicU64>,
    deadline: std::time::Duration,
}

impl ConversationTitles {
    pub fn new(repo: Arc<dyn IConversationRepository>, broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self {
            repo,
            broadcaster,
            latest: Arc::default(),
            sequence: Arc::default(),
            deadline: std::time::Duration::from_secs(75),
        }
    }

    /// Capture context after completion is persisted; generation never blocks the turn.
    pub async fn schedule(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
        generator: Arc<dyn TitleGenerator>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let revision = {
            let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
            if latest.get(conversation_id).is_some_and(|(id, _)| id == turn_id) {
                return None;
            }
            let revision = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
            if latest.len() >= 1024
                && !latest.contains_key(conversation_id)
                && let Some(oldest) = latest.iter().min_by_key(|(_, (_, n))| *n).map(|(id, _)| id.clone())
            {
                latest.remove(&oldest);
            }
            latest.insert(conversation_id.into(), (turn_id.into(), revision));
            revision
        };
        let Ok(queue_slot) = QUEUE_SLOTS.clone().try_acquire_owned() else {
            warn!(conversation_id, turn_id, "title queue full; retry on next reply");
            return None;
        };
        let snapshot = match self.snapshot(user_id, conversation_id).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return None,
            Err(_) => {
                warn!(conversation_id, turn_id, "title context could not be read");
                return None;
            }
        };
        let this = self.clone();
        let user_id = user_id.to_owned();
        let conversation_id = conversation_id.to_owned();
        let turn_id = turn_id.to_owned();
        Some(tokio::spawn(async move {
            let _queue_slot = queue_slot;
            let Ok(_slot) = RUN_SLOTS.acquire().await else { return };
            if !this.is_current(&conversation_id, revision) {
                return;
            }
            let (existing, description) = snapshot;
            info!(
                conversation_id,
                turn_id,
                context_len = description.len(),
                "conversation title summary started"
            );
            let result = tokio::time::timeout(this.deadline, generator.generate(&description)).await;
            let title = match result {
                Ok(Ok(text)) => match clean_title(&text) {
                    Some(title) => title,
                    None => {
                        warn!(conversation_id, turn_id, "title summary returned invalid text");
                        return;
                    }
                },
                Ok(Err(error)) => {
                    warn!(conversation_id, turn_id, error = %error, "title summary failed; retry on next reply");
                    return;
                }
                Err(_) => {
                    warn!(conversation_id, turn_id, "title summary timed out; retry on next reply");
                    return;
                }
            };
            if !this.is_current(&conversation_id, revision) {
                return;
            }
            match crate::service::persist_agent_title(
                &this.repo,
                &this.broadcaster,
                &user_id,
                &existing,
                &title,
                "summary",
            )
            .await
            {
                Ok(applied) => info!(conversation_id, turn_id, applied, "conversation title summary finished"),
                Err(_) => warn!(
                    conversation_id,
                    turn_id, "conversation title summary could not be saved"
                ),
            }
        }))
    }

    fn is_current(&self, conversation_id: &str, revision: u64) -> bool {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(conversation_id)
            .is_some_and(|(_, n)| *n == revision)
    }

    async fn snapshot(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<(ConversationRow, String)>, aionui_db::DbError> {
        let Some(existing) = self.repo.get(user_id, conversation_id).await? else {
            return Ok(None);
        };
        if existing.name_source.as_deref() == Some("user") {
            return Ok(None);
        }
        let page = self
            .repo
            .list_messages_page(
                user_id,
                conversation_id,
                &MessagePageParams {
                    limit: 64,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await?;
        let Some(description) = description(&existing.name, &page.items) else {
            return Ok(None);
        };
        Ok(Some((existing, description)))
    }
}

fn description(current_title: &str, messages: &[MessageRow]) -> Option<String> {
    let mut context = Vec::new();
    let mut has_assistant = false;
    // Keep the newest text within a fixed budget, excluding tools and hidden prompts.
    let mut budget = 8000;
    for row in messages.iter().rev() {
        if row.hidden || row.r#type != "text" || row.status.as_deref().is_some_and(|s| s != "finish") {
            continue;
        }
        let role = match row.position.as_deref() {
            Some("right") => "user",
            Some("left") => "assistant",
            _ => continue,
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&row.content) else {
            continue;
        };
        let Some(text) = value.get("content").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let text = crate::strip_think_tags(text);
        let bounded: String = text.trim().chars().take(budget.min(2000)).collect();
        if bounded.is_empty() {
            continue;
        }
        budget -= bounded.chars().count();
        has_assistant |= role == "assistant";
        context.push(serde_json::json!({"role":role,"text":bounded}));
        if budget == 0 || context.len() == 8 {
            break;
        }
    }
    if !has_assistant {
        return None;
    }
    context.reverse();
    Some(
        serde_json::json!({"current_title": current_title.chars().take(80).collect::<String>(), "messages":context})
            .to_string(),
    )
}

fn clean_title(text: &str) -> Option<String> {
    let title = text.trim().trim_matches(['"', '\'', '“', '”', '`']).trim();
    if title.is_empty()
        || title.chars().count() > 80
        || title.chars().any(char::is_control)
        || title.contains(['<', '>'])
    {
        return None;
    }
    Some(title.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_ai_agent::title::TitleGenerator;
    use aionui_db::{ConversationRowUpdate, SqliteConversationRepository, init_database_memory};
    use aionui_realtime::BroadcastEventBus;
    use aionui_session::BackendError;
    use tokio::sync::{Notify, Semaphore};

    const USER: &str = "system_default_user";
    struct Rig {
        repo: Arc<SqliteConversationRepository>,
        titles: ConversationTitles,
        bus: Arc<BroadcastEventBus>,
        _db: aionui_db::Database,
    }
    async fn rig() -> Rig {
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        repo.create(&ConversationRow {
            id: "conv".into(),
            user_id: USER.into(),
            name: "登录问题".into(),
            r#type: "acp".into(),
            extra: "{}".into(),
            model: None,
            status: Some("finished".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 1,
            project_id: None,
            folder_id: None,
            name_source: None,
        })
        .await
        .unwrap();
        let bus = Arc::new(BroadcastEventBus::new(64));
        let titles = ConversationTitles::new(repo.clone(), bus.clone());
        let rig = Rig {
            repo,
            titles,
            bus,
            _db: db,
        };
        rig.message("u1", "right", "修复登录跳转", 2).await;
        rig.message("a1", "left", "已修复登录跳转，补充回归验证", 3).await;
        rig
    }
    impl Rig {
        async fn message(&self, id: &str, position: &str, text: &str, time: i64) {
            self.repo
                .insert_message(
                    USER,
                    &MessageRow {
                        id: id.into(),
                        conversation_id: "conv".into(),
                        msg_id: None,
                        r#type: "text".into(),
                        content: serde_json::json!({"content":text}).to_string(),
                        position: Some(position.into()),
                        status: Some("finish".into()),
                        hidden: false,
                        created_at: time,
                        backend_turn_id: None,
                    },
                )
                .await
                .unwrap();
        }
        async fn name(&self) -> String {
            self.repo.get(USER, "conv").await.unwrap().unwrap().name
        }
    }
    struct Generator {
        answer: Option<String>,
        started: Notify,
        release: Semaphore,
        prompts: Mutex<Vec<String>>,
    }
    impl Generator {
        fn new(answer: Option<&str>, blocked: bool) -> Arc<Self> {
            Arc::new(Self {
                answer: answer.map(str::to_owned),
                started: Notify::new(),
                release: Semaphore::new(usize::from(!blocked)),
                prompts: Mutex::default(),
            })
        }
    }
    #[async_trait::async_trait]
    impl TitleGenerator for Generator {
        async fn generate(&self, description: &str) -> Result<String, BackendError> {
            self.prompts.lock().unwrap().push(description.into());
            self.started.notify_one();
            self.release.acquire().await.unwrap().forget();
            self.answer
                .clone()
                .ok_or_else(|| BackendError::Transport("test provider failure".into()))
        }
    }

    #[tokio::test]
    async fn every_completed_reply_summarizes_persisted_text_and_emits_refresh() {
        let rig = rig().await;
        let mut events = rig.bus.subscribe();
        let first = Generator::new(Some("登录跳转修复"), false);
        rig.titles
            .schedule(USER, "conv", "turn1", first.clone())
            .await
            .unwrap()
            .await
            .unwrap();
        assert_eq!(rig.name().await, "登录跳转修复");
        assert!(first.prompts.lock().unwrap()[0].contains("已修复登录跳转"));
        let event = events.recv().await.unwrap();
        assert_eq!(event.name, "conversation.nameUpdated");
        assert_eq!(event.data["user_id"], USER);
        assert_eq!(events.recv().await.unwrap().name, "conversation.listChanged");
        rig.message("u2", "right", "继续检查单点登录", 4).await;
        rig.message("a2", "left", "单点登录回调也已验证", 5).await;
        let second = Generator::new(Some("登录与单点登录验证"), false);
        rig.titles
            .schedule(USER, "conv", "turn2", second.clone())
            .await
            .unwrap()
            .await
            .unwrap();
        assert_eq!(rig.name().await, "登录与单点登录验证");
        assert!(second.prompts.lock().unwrap()[0].contains("单点登录回调也已验证"));
        assert_eq!(
            rig.repo
                .list_messages_page(
                    USER,
                    "conv",
                    &MessagePageParams {
                        limit: 64,
                        direction: MessagePageDirection::InitialLatest
                    }
                )
                .await
                .unwrap()
                .items
                .len(),
            4,
            "title calls must not create chat messages"
        );
        assert!(
            rig.titles.schedule(USER, "conv", "turn2", second).await.is_none(),
            "duplicate completion must be ignored"
        );
    }

    #[tokio::test]
    async fn manual_rename_while_generation_runs_wins() {
        let rig = rig().await;
        let generator = Generator::new(Some("Late automatic title"), true);
        let job = rig
            .titles
            .schedule(USER, "conv", "turn1", generator.clone())
            .await
            .unwrap();
        generator.started.notified().await;
        rig.repo
            .update(
                USER,
                "conv",
                &ConversationRowUpdate {
                    name: Some("我的标题".into()),
                    name_source: Some("user".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        generator.release.add_permits(1);
        job.await.unwrap();
        assert_eq!(rig.name().await, "我的标题");
        assert!(
            rig.titles
                .schedule(USER, "conv", "turn2", generator.clone())
                .await
                .is_none()
        );
        assert_eq!(generator.prompts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn older_job_finishing_after_newer_reply_cannot_overwrite_it() {
        let rig = rig().await;
        let first = Generator::new(Some("Old title"), true);
        let old = rig.titles.schedule(USER, "conv", "turn1", first.clone()).await.unwrap();
        first.started.notified().await;
        rig.message("u2", "right", "检查退出登录", 4).await;
        rig.message("a2", "left", "退出登录验证完成", 5).await;
        let second = Generator::new(Some("退出登录验证"), false);
        // Release the earlier run only after reserving the newer revision.
        let new = rig.titles.schedule(USER, "conv", "turn2", second).await.unwrap();
        first.release.add_permits(1);
        old.await.unwrap();
        new.await.unwrap();
        assert_eq!(rig.name().await, "退出登录验证");
    }

    #[tokio::test]
    async fn invalid_or_failed_summary_keeps_title_and_retries_next_reply() {
        let rig = rig().await;
        for (turn, answer) in [("t1", None), ("t2", Some("first line\nsecond line")), ("t3", Some(""))] {
            rig.titles
                .schedule(USER, "conv", turn, Generator::new(answer, false))
                .await
                .unwrap()
                .await
                .unwrap();
            assert_eq!(rig.name().await, "登录问题");
        }
        rig.titles
            .schedule(USER, "conv", "t4", Generator::new(Some("成功重试"), false))
            .await
            .unwrap()
            .await
            .unwrap();
        assert_eq!(rig.name().await, "成功重试");
    }

    #[test]
    fn title_validation_rejects_multiline_controls_and_oversize() {
        for bad in ["", " \n ", "one\ntwo", "<think>secret</think>", &"长".repeat(81)] {
            assert!(clean_title(bad).is_none(), "{bad:?}");
        }
        assert_eq!(clean_title("  “修复登录”  ").as_deref(), Some("修复登录"));
    }

    #[tokio::test]
    async fn timeout_keeps_name_and_drops_inflight_generation() {
        let mut rig = rig().await;
        rig.titles.deadline = std::time::Duration::from_millis(20);
        let generator = Generator::new(Some("Too late"), true);
        rig.titles
            .schedule(USER, "conv", "t1", generator.clone())
            .await
            .unwrap()
            .await
            .unwrap();
        assert_eq!(rig.name().await, "登录问题");
        assert_eq!(
            Arc::strong_count(&generator),
            1,
            "timed-out task must release its runner"
        );
    }

    #[test]
    fn context_filters_hidden_tools_thinking_and_incomplete_messages() {
        let row = |id: &str, text: &str| MessageRow {
            id: id.into(),
            conversation_id: "conv".into(),
            msg_id: None,
            r#type: "text".into(),
            content: serde_json::json!({"content":text}).to_string(),
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: 1,
            backend_turn_id: None,
        };
        let mut hidden = row("hidden", "hidden-injection");
        hidden.hidden = true;
        let mut tool = row("tool", "private-tool-output");
        tool.r#type = "tool_call".into();
        let mut pending = row("pending", "incomplete-answer");
        pending.status = Some("pending".into());
        let rows = vec![
            hidden,
            tool,
            pending,
            row("a", "<think>private-reasoning</think>回调验证完成"),
        ];
        let context = description("登录问题", &rows).unwrap();
        assert!(context.contains("回调验证完成"));
        for excluded in [
            "hidden-injection",
            "private-tool-output",
            "incomplete-answer",
            "private-reasoning",
        ] {
            assert!(!context.contains(excluded));
        }
    }
}
