use super::*;
use crate::{ConversationKeeperBrief, ConversationRolePort};

const BRIEF_TEXT: &str = "<project-keeper-brief>\nrobots.txt blocks /cart\n</project-keeper-brief>";

/// A role port whose project `p_store` is kept by `seo`.
struct KeptProject;

#[async_trait::async_trait]
impl ConversationRolePort for KeptProject {
    async fn direct_role_section(&self, _assistant_id: &str) -> Option<String> {
        None
    }

    async fn keeper_brief(
        &self,
        project_id: &str,
        _opening: &str,
        assistant_id: Option<&str>,
    ) -> Option<ConversationKeeperBrief> {
        (project_id == "p_store" && assistant_id != Some("seo")).then(|| ConversationKeeperBrief {
            role_id: "seo".into(),
            text: BRIEF_TEXT.into(),
            card: json!({ "role_id": "seo", "project_id": "p_store", "memory": ["robots.txt blocks /cart"] }),
        })
    }
}

fn finished_reply(text: &str) -> Vec<AgentStreamEvent> {
    vec![
        AgentStreamEvent::Text(TextEventData { content: text.into() }),
        AgentStreamEvent::Finish(FinishEventData::default()),
    ]
}

/// A conversation in the kept project, with `extra` merged into its create
/// request, and the agent that will answer its two turns.
async fn kept_conversation(
    extra: serde_json::Value,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<MockTaskManager>,
    Arc<ScriptedAgent>,
    String,
) {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, broadcaster, repo) = make_service_with_mock_task_manager(task_mgr.clone());
    svc.with_role_port(Arc::new(KeptProject));
    let mut req = make_create_req();
    if let (Some(into), Some(from)) = (req.extra.as_object_mut(), extra.as_object()) {
        into.extend(from.clone());
    }
    let conv = svc.create("user_1", req).await.unwrap();
    repo.update(
        "user_1",
        &conv.id,
        &ConversationRowUpdate {
            project_id: Some("p_store".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut agent = ScriptedAgent::new(&conv.id, vec![finished_reply("on it"), finished_reply("done")]);
    agent.workspace_override = Some(ensure_test_workspace_path());
    let agent = Arc::new(agent);
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent.clone()));
    let id = conv.id;
    (svc, broadcaster, repo, task_mgr, agent, id)
}

async fn send(svc: &ConversationService, task_mgr: &Arc<MockTaskManager>, conversation_id: &str) {
    let task_mgr: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", conversation_id, make_send_req(), &task_mgr)
        .await
        .unwrap();
    wait_for_turn_released(svc, conversation_id).await;
}

#[tokio::test]
async fn the_first_turn_in_a_kept_project_opens_with_the_keepers_brief_and_only_the_first() {
    let (svc, broadcaster, repo, task_mgr, agent, id) = kept_conversation(json!({})).await;

    send(&svc, &task_mgr, &id).await;
    send(&svc, &task_mgr, &id).await;

    let sent = agent.sent_contents();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0], format!("{BRIEF_TEXT}\n\nHello"));
    assert_eq!(sent[1], "Hello");

    let messages = repo_messages_asc(&repo, &id, 50).await;
    let user = messages
        .iter()
        .find(|m| m.r#type == "text" && m.position.as_deref() == Some("right"));
    let card = messages
        .iter()
        .find(|m| m.r#type == "keeper_brief")
        .expect("brief card");
    assert_eq!(
        user.map(|m| m.content.as_str()).map(|c| c.contains("\"Hello\"")),
        Some(true)
    );
    assert_eq!(card.position.as_deref(), Some("center"));
    assert!(card.created_at > user.unwrap().created_at);
    assert_eq!(messages.iter().filter(|m| m.r#type == "keeper_brief").count(), 1);

    let row = repo.get("user_1", &id).await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
    assert!(extra["keeper_briefed_at"].as_i64().is_some());
    assert_eq!(extra["keeper_brief_role"], "seo");

    assert!(
        broadcaster
            .take_events()
            .iter()
            .any(|e| e.name == "role.keeper_brief" && e.data["conversation_id"] == id.as_str())
    );
}

#[tokio::test]
async fn a_conversation_opened_with_the_brief_off_is_not_briefed() {
    let (svc, _broadcaster, repo, task_mgr, agent, id) = kept_conversation(json!({ "keeper_brief_off": true })).await;

    send(&svc, &task_mgr, &id).await;

    assert_eq!(agent.sent_contents(), vec!["Hello".to_owned()]);
    let messages = repo_messages_asc(&repo, &id, 50).await;
    assert!(messages.iter().all(|m| m.r#type != "keeper_brief"));
}
