use super::*;

fn saved(conversation_id: &str) -> aionui_api_types::KeeperDebrief {
    serde_json::from_value(json!({
        "id": "d-1",
        "role_id": "seo",
        "project_id": "p_store",
        "conversation_id": conversation_id,
        "message_id": "msg_card",
        "at": 1,
        "status": "saved",
        "items": [{ "key": "sitemap", "kind": "progress", "text": "submitted" }],
    }))
    .unwrap()
}

#[tokio::test]
async fn a_save_leaves_its_card_under_the_reply_and_announces_where_it_sits() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, broadcaster, repo) = make_service_with_mock_task_manager(task_mgr);
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    svc.leave_debrief_card("user_1", &saved(&conv.id), 42).await;

    let rows = repo_messages_asc(&repo, &conv.id, 10).await;
    let card = rows.iter().find(|row| row.id == "msg_card").expect("card stored");
    assert_eq!(card.r#type, "keeper_debrief");
    assert_eq!(card.msg_id.as_deref(), Some("msg_card"));
    assert_eq!(card.created_at, 42);
    let read = svc
        .latest_message_of_type("user_1", &conv.id, "keeper_debrief")
        .await
        .unwrap()
        .expect("reads back");
    assert_eq!(read.r#type, aionui_common::MessageType::KeeperDebrief);
    assert_eq!(read.content["items"][0]["key"], "sitemap");

    let events = broadcaster.take_events();
    let done = events
        .iter()
        .find(|event| event.name == "role.keeper_debrief")
        .expect("done event");
    assert_eq!(done.data["phase"], "done");
    assert_eq!(done.data["created_at"], 42);
    assert_eq!(done.data["conversation_id"], conv.id.as_str());
}
