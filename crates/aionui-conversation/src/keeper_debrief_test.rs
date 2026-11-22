use aionui_api_types::{KeeperBriefLesson, KeeperBriefTask, KeeperDebriefContext};
use aionui_common::AgentType;
use aionui_db::models::MessageRow;
use serde_json::json;

use super::*;

fn row(id: &str, kind: &str, content: serde_json::Value) -> MessageRow {
    MessageRow {
        id: id.into(),
        conversation_id: "c1".into(),
        msg_id: None,
        r#type: kind.into(),
        content: content.to_string(),
        position: Some("left".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 1,
        backend_turn_id: None,
    }
}

fn diff(path: &str) -> serde_json::Value {
    json!({ "type": "diff", "path": path, "oldText": "a", "newText": "b" })
}

#[test]
fn answer_is_kept_only_when_it_ends_on_the_end_line() {
    let whole = "{\"last_stop\": \"sitemap half submitted\", \"notes\": [\"Staging runs on port 4100\"]}\n[end]";
    let draft = parse_debrief(whole).expect("a whole answer parses");
    assert_eq!(draft.last_stop, "sitemap half submitted");
    assert_eq!(draft.notes, vec!["Staging runs on port 4100".to_owned()]);

    assert!(
        parse_debrief("{\"notes\": [\"cut off\"]}").is_none(),
        "no [end]: cut off"
    );
    assert!(parse_debrief("[end]").is_none(), "nothing before [end]");
    assert!(parse_debrief("{\"notes\": [\"x\"]\n[end]").is_none(), "broken JSON");
}

#[test]
fn answer_may_come_fenced_or_after_thinking() {
    let fenced = "```json\n{\"lessons\": [\"Check robots.txt on staging first\"]}\n```\n[end]\n";
    assert_eq!(
        parse_debrief(fenced).expect("fenced").lessons,
        vec!["Check robots.txt on staging first".to_owned()]
    );
    let thinking = "<think>the user {maybe}</think>{\"notes\": []}\n[end]";
    assert!(parse_debrief(thinking).expect("thinking dropped").notes.is_empty());
}

#[test]
fn only_the_users_own_project_conversations_are_saved() {
    let plain = json!({ "workspace": "/Users/me/store" });
    assert!(debrief_eligible(Some("p_store"), &plain));
    assert!(!debrief_eligible(None, &plain), "no project");
    assert!(!debrief_eligible(Some(" "), &plain), "blank project");
    for key in [
        "cron_job_id",
        "cronJobId",
        "teamId",
        "team_id",
        "companion_of",
        "jot_trial",
    ] {
        let extra = json!({ "workspace": "/Users/me/store", key: "x" });
        assert!(
            !debrief_eligible(Some("p_store"), &extra),
            "{key} is not the user's own"
        );
    }
    assert!(debrief_eligible(
        Some("p_store"),
        &json!({ "workspace": "/Users/me/store", "companion_of": null })
    ));
    assert!(!debrief_eligible(
        Some("p_store"),
        &json!({ "workspace": "/Users/me/store", "keeper_brief_off": true })
    ));
    assert!(!debrief_eligible(
        Some("p_store"),
        &json!({ "workspace": "/Users/me/store", "is_temporary_workspace": true })
    ));
}

#[test]
fn claude_and_codex_cannot_write_a_digest() {
    assert!(!digests_on("claude", AgentType::Acp));
    assert!(!digests_on("Codex", AgentType::Acp));
    assert!(!digests_on("anything", AgentType::Codex));
    assert!(digests_on("gemini", AgentType::Acp));
    assert!(digests_on("", AgentType::Aionrs));
}

#[test]
fn changed_files_come_from_diffs_once_each_in_order() {
    let rows = vec![
        row("1", "text", json!({ "content": "edit two files" })),
        row(
            "2",
            "acp_tool_call",
            json!({ "update": { "content": [diff("src/robots.ts"), { "type": "content" }, diff("src/sitemap.ts")] } }),
        ),
        row(
            "3",
            "acp_tool_call",
            json!({ "update": { "content": [diff("src/robots.ts"), diff(" ")] } }),
        ),
        row("4", "acp_tool_call", json!("not an object")),
    ];
    assert_eq!(changed_files(&rows), vec!["src/robots.ts", "src/sitemap.ts"]);
}

#[test]
fn prompt_carries_what_the_keeper_knows_and_the_files() {
    let context = KeeperDebriefContext {
        role_id: "seo".into(),
        project_id: "p_store".into(),
        project_name: "YiPan-Global-Store".into(),
        charter: "Keeps the store findable.".into(),
        notes: vec!["2026-11-17 [YiPan-Global-Store] Core pages indexed".into()],
        lessons: vec![KeeperBriefLesson {
            id: "p-0007".into(),
            text: "Verify robots.txt on staging first".into(),
        }],
        progress: vec![KeeperBriefTask {
            task: "sitemap".into(),
            state: "half submitted".into(),
        }],
        saves_today: 0,
        thread_conversation_id: None,
    };
    let prompt = debrief_prompt(&context, &["src/robots.ts".into()], "user: go", true);
    assert!(prompt.contains("The project: YiPan-Global-Store"));
    assert!(prompt.contains("- [p-0007] Verify robots.txt on staging first"));
    assert!(prompt.contains("- sitemap: half submitted"));
    assert!(prompt.contains("- src/robots.ts"));
    assert!(prompt.contains("(its older part left out)"));
    assert!(prompt.ends_with("user: go\n</transcript>"));
}

#[test]
fn a_later_turn_replaces_the_pending_save() {
    let debriefs = KeeperDebriefs::default();
    let first = debriefs.bump("c1");
    let second = debriefs.bump("c1");
    assert!(!debriefs.is_current("c1", first));
    assert!(debriefs.is_current("c1", second));
    debriefs.settle("c1", first);
    assert!(
        debriefs.is_current("c1", second),
        "a stale save does not clear a newer one"
    );
    debriefs.settle("c1", second);
    assert!(!debriefs.is_current("c1", second));
}

#[test]
fn the_save_mark_reads_back() {
    let extra = json!({ "keeper_debriefed_to": { "created_at": 42, "id": "m9" } });
    assert_eq!(
        mark_of(&extra),
        Some(RoleThreadMark {
            created_at: 42,
            id: "m9".into()
        })
    );
    assert_eq!(mark_of(&json!({})), None);
}

#[tokio::test]
async fn passes_of_one_role_wait_for_each_other() {
    let held = lock_role_pass("seo").await;
    let other = tokio::time::timeout(Duration::from_millis(20), lock_role_pass("chief")).await;
    assert!(other.is_ok(), "another role is not held up");
    let same = tokio::time::timeout(Duration::from_millis(20), lock_role_pass("seo")).await;
    assert!(same.is_err(), "the same role waits");
    drop(held);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), lock_role_pass("seo"))
            .await
            .is_ok()
    );
}

#[test]
fn only_a_keepers_scratch_folder_is_removed_and_only_when_done() {
    let root = tempfile::tempdir().expect("tempdir");
    let scratch = root.path().join(format!("{SCRATCH_PREFIX}abc"));
    let other = root.path().join("my-project");
    for dir in [&scratch, &other] {
        std::fs::create_dir_all(dir.join(".engine")).expect("mkdir");
    }
    let extra = |path: &std::path::Path| json!({ "workspace": path.to_string_lossy() }).to_string();

    let held = Scratch::of(&extra(&scratch));
    assert!(scratch.exists(), "still there while the summary runs");
    drop(held);
    assert!(!scratch.exists(), "gone with what the engine left in it");

    drop(Scratch::of(&extra(&other)));
    assert!(other.exists(), "a folder not named like a scratch one stays");
    drop(Scratch::of("not json"));
}
