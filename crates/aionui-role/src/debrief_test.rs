//! Automatic saves: what lands in memory, what is skipped, and taking items back.

use super::*;
use aionui_api_types::{
    KeeperBriefTask, KeeperDebriefDraftWarning, RoleProjectClaim, SetRoleProjectsRequest, UpsertRoleRequest,
};

const DAY: &str = "2026-11-21";

fn notes(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

async fn keeper(root: &tempfile::TempDir) -> RoleService {
    let service = RoleService::new(root.path().join("roles"));
    service
        .upsert(
            "seo",
            UpsertRoleRequest {
                charter: "Keeps the store findable.".into(),
                denied_tools: Vec::new(),
                verifies: false,
                asks_when_scheduled: false,
            },
        )
        .await
        .unwrap();
    service
        .set_projects(
            "seo",
            SetRoleProjectsRequest {
                projects: vec![
                    RoleProjectClaim {
                        project_id: "p_store".into(),
                        name: "store".into(),
                    },
                    RoleProjectClaim {
                        project_id: "p_journal".into(),
                        name: "journal".into(),
                    },
                ],
                transfer: false,
            },
            1_000,
        )
        .await
        .unwrap();
    service
}

fn record<'a>(draft: Option<&'a KeeperDebriefDraft>, status: KeeperDebriefStatus, at: i64) -> DebriefRecord<'a> {
    DebriefRecord {
        role_id: "seo",
        project_id: "p_store",
        conversation_id: "conv-1",
        message_id: "msg-1",
        status,
        draft,
        day: DAY,
        at,
    }
}

#[test]
fn notes_are_tagged_with_their_project_and_never_written_twice() {
    let memory = "- 2026-11-01 [store] sitemap lives in public/\n- 2026-11-02 robots.txt blocks /cart\n";
    let (updated, written) = with_notes(
        memory,
        DAY,
        "store",
        &notes(&[
            "sitemap lives in public/",
            "- 2026-11-20 robots.txt blocks /cart",
            "[store]  Product pages   use JSON-LD",
            "ok",
            "Product pages use JSON-LD",
        ]),
    );
    assert_eq!(
        written,
        vec![(
            "- 2026-11-21 [store] Product pages use JSON-LD".to_owned(),
            "Product pages use JSON-LD".to_owned()
        )]
    );
    assert_eq!(
        updated,
        format!("{memory}- 2026-11-21 [store] Product pages use JSON-LD\n")
    );
}

#[test]
fn a_save_keeps_at_most_five_notes_and_an_unnamed_project_leaves_them_untagged() {
    let many: Vec<String> = (0..8).map(|index| format!("note number {index}")).collect();
    let (updated, written) = with_notes("", DAY, "", &many);
    assert_eq!(written.len(), 5);
    assert!(updated.starts_with("- 2026-11-21 note number 0\n"));
}

#[test]
fn a_note_taken_back_leaves_by_its_line_or_by_its_text_once_rewritten() {
    let memory = "- 2026-11-01 keep\n- 2026-11-21 [store] JSON-LD on product pages\n- 2026-11-02 also keep\n";
    assert_eq!(
        without_note(memory, "- 2026-11-21 [store] JSON-LD on product pages").as_deref(),
        Some("- 2026-11-01 keep\n- 2026-11-02 also keep\n")
    );
    let rewritten = "- 2026-11-25 [store] JSON-LD on product pages\n";
    assert_eq!(
        without_note(rewritten, "- 2026-11-21 [store] JSON-LD on product pages").as_deref(),
        Some("")
    );
    assert_eq!(without_note("- 2026-11-01 keep\n", "- 2026-11-21 gone"), None);
}

#[test]
fn only_saves_that_ran_the_engine_count_against_the_day() {
    let at = |at: i64, status: KeeperDebriefStatus| KeeperDebrief {
        id: format!("d-{at}"),
        role_id: "seo".into(),
        project_id: "p_store".into(),
        project_name: String::new(),
        conversation_id: "c".into(),
        message_id: String::new(),
        at,
        status,
        last_stop: None,
        items: Vec::new(),
        warnings: Vec::new(),
    };
    let saves = vec![
        at(5, KeeperDebriefStatus::Saved),
        at(20, KeeperDebriefStatus::Saved),
        at(21, KeeperDebriefStatus::Nothing),
        at(22, KeeperDebriefStatus::Failed),
        at(23, KeeperDebriefStatus::Unsupported),
    ];
    assert_eq!(saves_since(&saves, 10), 3);
    assert_eq!(new_debrief_id(20, &saves), "d-20-2");
    assert_eq!(new_debrief_id(30, &saves), "d-30");
}

#[tokio::test]
async fn a_save_lands_in_memory_playbook_progress_and_the_project_ledger() {
    let root = tempfile::tempdir().unwrap();
    let service = keeper(&root).await;
    service
        .apply_retrospective(
            "seo",
            &[PlaybookDeltaOp::Add {
                content: "Check robots.txt on staging before shipping".into(),
            }],
            &[],
            "user",
            "seed",
            2_000,
        )
        .await
        .unwrap();
    service
        .set_progress("seo", "sitemap", "generator drafted")
        .await
        .unwrap();

    let draft = KeeperDebriefDraft {
        last_stop: "Sitemap submitted; waiting on Search Console".into(),
        notes: notes(&["Product pages use JSON-LD"]),
        lessons: notes(&["Regenerate the sitemap after every catalog import"]),
        progress: vec![KeeperBriefTask {
            task: "sitemap".into(),
            state: "submitted to Search Console".into(),
        }],
        warnings: vec![
            KeeperDebriefDraftWarning {
                lesson_id: "p-0001".into(),
                files: vec!["public/robots.txt".into(), " ".into()],
                note: "robots.txt changed without a staging check".into(),
            },
            KeeperDebriefDraftWarning {
                lesson_id: "p-0099".into(),
                files: Vec::new(),
                note: "made up".into(),
            },
        ],
    };
    let saved = service
        .record_debrief(record(Some(&draft), KeeperDebriefStatus::Saved, 3_000))
        .await
        .unwrap();

    assert_eq!(saved.status, KeeperDebriefStatus::Saved);
    assert_eq!(saved.project_name, "store");
    assert_eq!(saved.message_id, "msg-1");
    let kinds: Vec<_> = saved.items.iter().map(|item| (item.kind, item.key.as_str())).collect();
    assert_eq!(
        kinds,
        vec![
            (
                KeeperDebriefItemKind::Note,
                "- 2026-11-21 [store] Product pages use JSON-LD"
            ),
            (KeeperDebriefItemKind::Lesson, "p-0002"),
            (KeeperDebriefItemKind::Progress, "sitemap"),
        ]
    );
    assert_eq!(
        saved.items[0].text, "Product pages use JSON-LD",
        "shown without day or tag"
    );
    assert_eq!(saved.items[2].previous.as_deref(), Some("generator drafted"));
    assert_eq!(
        saved.last_stop.as_deref(),
        Some("Sitemap submitted; waiting on Search Console")
    );
    assert_eq!(saved.warnings.len(), 1);
    assert_eq!(saved.warnings[0].lesson, "Check robots.txt on staging before shipping");
    assert_eq!(saved.warnings[0].files, vec!["public/robots.txt"]);

    let role = service.get("seo").await.unwrap();
    assert!(role.memory.contains("- 2026-11-21 [store] Product pages use JSON-LD"));
    assert!(role.playbook.iter().any(|entry| entry.id == "p-0002"));
    assert_eq!(role.progress[0].state.trim(), "submitted to Search Console");
    let ledger = service
        .store
        .read_project_ledger("seo", "p_store")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ledger.last_stop.map(|stop| (stop.text, stop.conversation_id)),
        Some(("Sitemap submitted; waiting on Search Console".into(), "conv-1".into()))
    );
    let history = service.timeline("seo", 5).await.unwrap();
    assert_eq!(history[0].actor, "keeper-debrief");
    assert!(
        std::fs::read_to_string(root.path().join("roles/seo/.gitignore"))
            .unwrap()
            .contains("debriefs.jsonl")
    );

    // Everything again: all of it is known now.
    let again = service
        .record_debrief(record(
            Some(&KeeperDebriefDraft {
                warnings: Vec::new(),
                ..draft.clone()
            }),
            KeeperDebriefStatus::Saved,
            4_000,
        ))
        .await
        .unwrap();
    assert_eq!(again.status, KeeperDebriefStatus::Nothing);
    assert!(again.items.is_empty());
    assert_eq!(again.last_stop, None, "the same stop is not news");

    // Only where the work stopped moved on: still a save.
    let moved = service
        .record_debrief(record(
            Some(&KeeperDebriefDraft {
                last_stop: "Search Console accepted the sitemap".into(),
                ..KeeperDebriefDraft::default()
            }),
            KeeperDebriefStatus::Saved,
            5_000,
        ))
        .await
        .unwrap();
    assert_eq!(moved.status, KeeperDebriefStatus::Saved);
    assert!(moved.items.is_empty());
    assert_eq!(moved.last_stop.as_deref(), Some("Search Console accepted the sitemap"));
}

#[tokio::test]
async fn taking_items_back_undoes_exactly_what_the_save_wrote() {
    let root = tempfile::tempdir().unwrap();
    let service = keeper(&root).await;
    service.set_memory("seo", "- 2026-11-01 keep me\n").await.unwrap();
    let draft = KeeperDebriefDraft {
        notes: notes(&["Product pages use JSON-LD"]),
        lessons: notes(&["Regenerate the sitemap after every catalog import"]),
        progress: vec![KeeperBriefTask {
            task: "Sitemap".into(),
            state: "submitted".into(),
        }],
        ..KeeperDebriefDraft::default()
    };
    let saved = service
        .record_debrief(record(Some(&draft), KeeperDebriefStatus::Saved, 3_000))
        .await
        .unwrap();

    for item in &saved.items {
        service
            .forget_debrief_item("seo", &saved.id, &item.key, 5_000)
            .await
            .unwrap();
    }
    let role = service.get("seo").await.unwrap();
    assert_eq!(role.memory, "- 2026-11-01 keep me\n");
    assert!(role.playbook.is_empty());
    assert!(role.progress.is_empty());
    assert_eq!(service.timeline("seo", 5).await.unwrap()[0].actor, "user");

    let listed = service.debriefs("seo").await.unwrap();
    assert!(listed[0].items.iter().all(|item| item.forgotten));
    // Twice is not an error, an unknown key is.
    let twice = service
        .forget_debrief_item("seo", &saved.id, &saved.items[0].key, 6_000)
        .await
        .unwrap();
    assert!(twice.items[0].forgotten);
    assert!(matches!(
        service.forget_debrief_item("seo", &saved.id, "nope", 6_000).await,
        Err(RoleError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn a_task_that_moved_on_is_left_alone_when_its_save_is_taken_back() {
    let root = tempfile::tempdir().unwrap();
    let service = keeper(&root).await;
    service.set_progress("seo", "sitemap", "drafted").await.unwrap();
    let draft = KeeperDebriefDraft {
        progress: vec![KeeperBriefTask {
            task: "sitemap".into(),
            state: "submitted".into(),
        }],
        ..KeeperDebriefDraft::default()
    };
    let saved = service
        .record_debrief(record(Some(&draft), KeeperDebriefStatus::Saved, 3_000))
        .await
        .unwrap();
    service.set_progress("seo", "sitemap", "indexed").await.unwrap();
    service
        .forget_debrief_item("seo", &saved.id, "sitemap", 4_000)
        .await
        .unwrap();
    assert_eq!(service.get("seo").await.unwrap().progress[0].state.trim(), "indexed");

    // Untouched since: back to where it stood.
    let saved = service
        .record_debrief(record(Some(&draft), KeeperDebriefStatus::Saved, 5_000))
        .await
        .unwrap();
    service
        .forget_debrief_item("seo", &saved.id, "sitemap", 6_000)
        .await
        .unwrap();
    assert_eq!(service.get("seo").await.unwrap().progress[0].state.trim(), "indexed");
}

#[tokio::test]
async fn an_outdated_lesson_counts_once_against_it() {
    let root = tempfile::tempdir().unwrap();
    let service = keeper(&root).await;
    service
        .apply_retrospective(
            "seo",
            &[PlaybookDeltaOp::Add {
                content: "Check robots.txt on staging before shipping".into(),
            }],
            &[],
            "user",
            "seed",
            2_000,
        )
        .await
        .unwrap();
    let draft = KeeperDebriefDraft {
        warnings: vec![KeeperDebriefDraftWarning {
            lesson_id: "p-0001".into(),
            files: vec!["public/robots.txt".into()],
            note: "changed robots.txt".into(),
        }],
        ..KeeperDebriefDraft::default()
    };
    let saved = service
        .record_debrief(record(Some(&draft), KeeperDebriefStatus::Saved, 3_000))
        .await
        .unwrap();
    assert_eq!(saved.status, KeeperDebriefStatus::Saved);

    for at in [4_000, 5_000] {
        let marked = service
            .mark_debrief_lesson_outdated("seo", &saved.id, "p-0001", at)
            .await
            .unwrap();
        assert!(marked.warnings[0].outdated);
    }
    let role = service.get("seo").await.unwrap();
    assert_eq!(role.playbook[0].harmful, 1);
    assert_eq!(service.timeline("seo", 1).await.unwrap()[0].actor, "user");
}

#[tokio::test]
async fn a_skipped_save_writes_nothing_but_is_recorded() {
    let root = tempfile::tempdir().unwrap();
    let service = keeper(&root).await;
    let skipped = service
        .record_debrief(record(None, KeeperDebriefStatus::Unsupported, 3_000))
        .await
        .unwrap();
    assert_eq!(skipped.status, KeeperDebriefStatus::Unsupported);
    let failed = service
        .record_debrief(record(None, KeeperDebriefStatus::Failed, 3_000))
        .await
        .unwrap();
    assert_eq!(failed.id, "d-3000-2");
    assert_eq!(service.get("seo").await.unwrap().memory, "");

    let context = service
        .keeper_debrief_context("p_store", Some("claude"), 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.saves_today, 1);
    assert_eq!(context.project_name, "store");
    assert_eq!(context.charter, "Keeps the store findable.");
    assert!(
        service
            .keeper_debrief_context("p_store", Some("seo"), 0)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        service
            .keeper_debrief_context("p_none", None, 0)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_record_keeps_only_the_latest_saves() {
    let root = tempfile::tempdir().unwrap();
    let service = keeper(&root).await;
    for at in 0..(MAX_KEPT_DEBRIEFS as i64 + 3) {
        service
            .record_debrief(record(None, KeeperDebriefStatus::Unsupported, at))
            .await
            .unwrap();
    }
    let listed = service.debriefs("seo").await.unwrap();
    assert_eq!(listed.len(), MAX_KEPT_DEBRIEFS);
    assert_eq!(listed[0].at, MAX_KEPT_DEBRIEFS as i64 + 2);
}

#[tokio::test]
async fn a_curation_made_from_an_older_memory_is_not_written() {
    let root = tempfile::tempdir().unwrap();
    let service = keeper(&root).await;
    service
        .set_memory("seo", "- 2026-11-01 a\n- 2026-11-02 b\n")
        .await
        .unwrap();
    let seen = service.get("seo").await.unwrap().memory;
    service.remember("seo", "c", DAY).await.unwrap();
    let stale = service
        .curate_memory_from(
            "seo",
            &seen,
            "- 2026-11-02 a and b\n",
            "2026-11-21-0101",
            "dreamer",
            "tidy",
        )
        .await
        .unwrap();
    assert_eq!(stale, None);
    assert!(service.get("seo").await.unwrap().memory.contains("- 2026-11-21 c"));

    let seen = service.get("seo").await.unwrap().memory;
    let fresh = service
        .curate_memory_from(
            "seo",
            &seen,
            "- 2026-11-21 a, b and c\n",
            "2026-11-21-0102",
            "dreamer",
            "tidy",
        )
        .await
        .unwrap();
    assert!(fresh.is_some_and(|done| done.archived_as.is_some()));
}
