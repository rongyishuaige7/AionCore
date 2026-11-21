//! Role service tests against a real temp directory: lifecycle, validation,
//! and the guarantees other crates lean on (memory survives dismissal, ids
//! cannot escape the root).

use super::*;

fn service(root: &tempfile::TempDir) -> RoleService {
    RoleService::new(root.path().join("roles"))
}

fn request(charter: &str, denied: &[&str]) -> UpsertRoleRequest {
    UpsertRoleRequest {
        charter: charter.to_owned(),
        denied_tools: denied.iter().map(|tool| (*tool).to_owned()).collect(),
        verifies: false,
        asks_when_scheduled: false,
    }
}

#[tokio::test]
async fn list_is_empty_before_any_role_exists() {
    let root = tempfile::tempdir().unwrap();
    assert!(service(&root).list().await.unwrap().is_empty());
}

#[tokio::test]
async fn upsert_creates_role_and_its_memory_directory() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);

    let role = service
        .upsert("custom-1700000000000-ab12", request("  Owns the release notes.  ", &[]))
        .await
        .unwrap();

    assert_eq!(role.id, "custom-1700000000000-ab12");
    assert_eq!(role.charter, "Owns the release notes.");
    assert!(role.denied_tools.is_empty());
    assert_eq!(role.memory, "");
    assert_eq!(role.created_at, role.updated_at);
    assert!(std::path::Path::new(&role.memory_dir).is_dir());
    assert_eq!(service.get("custom-1700000000000-ab12").await.unwrap(), role);
}

#[tokio::test]
async fn upsert_canonicalizes_the_permission_table() {
    let root = tempfile::tempdir().unwrap();
    let role = service(&root)
        .upsert(
            "reviewer",
            request("", &["team_task_update", " team_send_message ", "team_task_update"]),
        )
        .await
        .unwrap();
    assert_eq!(role.denied_tools, vec!["team_send_message", "team_task_update"]);
}

#[tokio::test]
async fn upsert_rejects_unknown_tools_and_oversized_charters() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);

    let unknown = service.upsert("reviewer", request("", &["rm_rf"])).await;
    assert!(matches!(unknown, Err(RoleError::InvalidRequest(_))));

    let long = "x".repeat(MAX_CHARTER_CHARS + 1);
    let oversized = service.upsert("reviewer", request(&long, &[])).await;
    assert!(matches!(oversized, Err(RoleError::InvalidRequest(_))));

    assert!(service.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn upsert_keeps_created_at_and_memory_of_an_existing_role() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    let first = service.upsert("reviewer", request("v1", &[])).await.unwrap();
    service.set_memory("reviewer", "- prefers small diffs\n").await.unwrap();

    let second = service
        .upsert("reviewer", request("v2", &["team_task_create"]))
        .await
        .unwrap();

    assert_eq!(second.created_at, first.created_at);
    assert_eq!(second.charter, "v2");
    assert_eq!(second.memory, "- prefers small diffs\n");
}

#[tokio::test]
async fn set_memory_requires_an_existing_role_and_respects_the_cap() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);

    let missing = service.set_memory("reviewer", "note").await;
    assert!(matches!(missing, Err(RoleError::NotFound(_))));

    service.upsert("reviewer", request("", &[])).await.unwrap();
    let oversized = service.set_memory("reviewer", &"x".repeat(MAX_MEMORY_BYTES + 1)).await;
    assert!(matches!(oversized, Err(RoleError::InvalidRequest(_))));
    assert_eq!(service.get("reviewer").await.unwrap().memory, "");
}

#[tokio::test]
async fn remove_dismisses_the_role_but_keeps_its_memory() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("reviewer", request("v1", &[])).await.unwrap();
    service.set_memory("reviewer", "learned").await.unwrap();

    service.remove("reviewer").await.unwrap();

    assert!(service.find("reviewer").await.unwrap().is_none());
    assert!(service.list().await.unwrap().is_empty());
    assert!(matches!(service.remove("reviewer").await, Err(RoleError::NotFound(_))));

    let again = service.upsert("reviewer", request("v2", &[])).await.unwrap();
    assert_eq!(again.memory, "learned");
}

#[tokio::test]
async fn ids_that_could_escape_the_root_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    let too_long = "a".repeat(MAX_ROLE_ID_LEN + 1);

    for id in [
        "",
        ".",
        "..",
        "../outside",
        "a/b",
        "a\\b",
        "a@b",
        ".hidden",
        "空",
        too_long.as_str(),
    ] {
        let result = service.upsert(id, request("", &[])).await;
        assert!(
            matches!(result, Err(RoleError::InvalidId(_))),
            "id {id:?} must be rejected"
        );
        assert!(matches!(service.find(id).await, Err(RoleError::InvalidId(_))));
    }
    assert!(!root.path().join("outside").exists());
}

#[tokio::test]
async fn a_bare_agent_assistant_can_be_a_role() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);

    let role = service
        .upsert("bare:8e1acf31", request("Writes the code.", &[]))
        .await
        .unwrap();
    service.set_memory("bare:8e1acf31", "- runs the tests").await.unwrap();

    assert_eq!(role.id, "bare:8e1acf31");
    assert!(
        root.path()
            .join("roles")
            .join("bare@8e1acf31")
            .join("role.json")
            .is_file()
    );
    assert!(!role.memory_dir.contains(':'));

    let listed = service.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "bare:8e1acf31");
    assert_eq!(listed[0].memory, "- runs the tests");
}

#[tokio::test]
async fn the_reviewer_flag_is_saved_and_a_role_file_without_it_reads_as_not_reviewing() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);

    let saved = service
        .upsert(
            "checker",
            UpsertRoleRequest {
                verifies: true,
                ..request("Checks work.", &[])
            },
        )
        .await
        .unwrap();
    assert!(saved.verifies);
    assert!(service.get("checker").await.unwrap().verifies);

    // A role file written before the flag existed.
    let old_dir = root.path().join("roles").join("old");
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::write(
        old_dir.join("role.json"),
        r#"{"charter":"c","denied_tools":[],"created_at":1,"updated_at":1}"#,
    )
    .unwrap();
    assert!(!service.get("old").await.unwrap().verifies);
}

#[tokio::test]
async fn asking_when_scheduled_is_saved_and_a_role_file_without_it_reads_as_running_unasked() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);

    let saved = service
        .upsert(
            "careful",
            UpsertRoleRequest {
                asks_when_scheduled: true,
                ..request("Touches production.", &[])
            },
        )
        .await
        .unwrap();
    assert!(saved.asks_when_scheduled);
    assert!(service.get("careful").await.unwrap().asks_when_scheduled);

    // Saving the role again without the flag turns it off.
    service
        .upsert("careful", request("Touches production.", &[]))
        .await
        .unwrap();
    assert!(!service.get("careful").await.unwrap().asks_when_scheduled);

    // A role file written before the flag existed.
    let old_dir = root.path().join("roles").join("old");
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::write(
        old_dir.join("role.json"),
        r#"{"charter":"c","denied_tools":[],"created_at":1,"updated_at":1}"#,
    )
    .unwrap();
    assert!(!service.get("old").await.unwrap().asks_when_scheduled);
}

#[tokio::test]
async fn remember_appends_one_dated_line_after_what_is_already_there() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("reviewer", request("", &[])).await.unwrap();
    service
        .set_memory("reviewer", "- written by the user\n\n")
        .await
        .unwrap();

    let noted = service
        .remember(
            "reviewer",
            "  the user wants tests run,\n  not described ",
            "2026-11-14",
        )
        .await
        .unwrap();

    assert_eq!(noted.line, "- 2026-11-14 the user wants tests run, not described");
    assert!(!noted.already_recorded);
    assert_eq!(
        service.get("reviewer").await.unwrap().memory,
        "- written by the user\n- 2026-11-14 the user wants tests run, not described\n"
    );
}

#[tokio::test]
async fn remember_does_not_write_the_same_note_twice() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("reviewer", request("", &[])).await.unwrap();
    service.set_memory("reviewer", "- run the tests\n").await.unwrap();

    // Already there without a day, then again on a later day.
    let first = service
        .remember("reviewer", "run the tests", "2026-11-14")
        .await
        .unwrap();
    service
        .remember("reviewer", "ask before pushing", "2026-11-14")
        .await
        .unwrap();
    let again = service
        .remember("reviewer", "ask before pushing", "2026-11-20")
        .await
        .unwrap();

    assert!(first.already_recorded);
    assert_eq!(first.line, "- run the tests");
    assert!(again.already_recorded);
    assert_eq!(again.line, "- 2026-11-14 ask before pushing");
    assert_eq!(
        service.get("reviewer").await.unwrap().memory,
        "- run the tests\n- 2026-11-14 ask before pushing\n"
    );
}

#[tokio::test]
async fn remember_strips_a_leading_bullet_and_day_when_the_caller_already_wrote_them() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("reviewer", request("", &[])).await.unwrap();

    let first = service
        .remember("reviewer", "2026-11-17 keep exits pinned", "2026-11-17")
        .await
        .unwrap();
    assert_eq!(first.line, "- 2026-11-17 keep exits pinned");
    assert!(!first.already_recorded);

    let second = service
        .remember("reviewer", "- 2026-11-18 keep exits pinned", "2026-11-18")
        .await
        .unwrap();
    assert!(second.already_recorded);
    assert_eq!(second.line, "- 2026-11-17 keep exits pinned");

    assert!(matches!(
        service.remember("reviewer", "- 2026-11-17", "2026-11-17").await,
        Err(RoleError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn remember_refuses_bad_notes_a_full_memory_and_an_assistant_that_is_no_role() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("reviewer", request("", &[])).await.unwrap();

    let too_long = "记".repeat(MAX_NOTE_CHARS + 1);
    for note in ["", " \n ", too_long.as_str()] {
        let result = service.remember("reviewer", note, "2026-11-14").await;
        assert!(matches!(result, Err(RoleError::InvalidRequest(_))), "note {note:?}");
    }
    assert!(matches!(
        service.remember("stranger", "a note", "2026-11-14").await,
        Err(RoleError::NotFound(_))
    ));

    service
        .set_memory("reviewer", &"x".repeat(MAX_MEMORY_BYTES - 5))
        .await
        .unwrap();
    let full = service.remember("reviewer", "one more", "2026-11-14").await;
    assert!(matches!(full, Err(RoleError::InvalidRequest(_))));
    assert_eq!(
        service.get("reviewer").await.unwrap().memory.len(),
        MAX_MEMORY_BYTES - 5
    );
}

#[tokio::test]
async fn list_skips_a_corrupt_definition_and_upsert_can_repair_it() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("good", request("ok", &[])).await.unwrap();
    let broken_dir = root.path().join("roles").join("broken");
    std::fs::create_dir_all(&broken_dir).unwrap();
    std::fs::write(broken_dir.join("role.json"), "{ not json").unwrap();

    let listed = service.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "good");
    assert!(matches!(service.get("broken").await, Err(RoleError::Corrupt(_))));

    service.upsert("broken", request("repaired", &[])).await.unwrap();
    assert_eq!(service.list().await.unwrap().len(), 2);
}

#[tokio::test]
async fn progress_keeps_one_file_per_task_and_replaces_its_state() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    let role = service.upsert("scout", request("", &[])).await.unwrap();
    assert!(role.progress.is_empty());

    let first = service
        .set_progress(
            "scout",
            "Weekly report",
            "  Draft written.\nFigures for week 46 still missing.  ",
        )
        .await
        .unwrap();
    assert_eq!(
        first,
        ProgressUpdate {
            task: "weekly-report".into(),
            closed: false
        }
    );
    service
        .set_progress("scout", "weekly-report", "Sent to the user.")
        .await
        .unwrap();

    let role = service.get("scout").await.unwrap();
    assert_eq!(role.progress.len(), 1);
    assert_eq!(role.progress[0].task, "weekly-report");
    assert_eq!(role.progress[0].state, "Sent to the user.\n");
    assert!(role.progress[0].updated_at > 0);
    let file = std::path::Path::new(&role.memory_dir).join("progress/weekly-report.md");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "Sent to the user.\n");
}

#[tokio::test]
async fn progress_lists_the_most_recently_written_task_first() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    let role = service.upsert("scout", request("", &[])).await.unwrap();
    service.set_progress("scout", "older", "started").await.unwrap();
    service.set_progress("scout", "newer", "started").await.unwrap();

    // Push the first file an hour back, so the order does not hang on the clock's resolution.
    let older = std::path::Path::new(&role.memory_dir).join("progress/older.md");
    let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(older)
        .unwrap()
        .set_modified(an_hour_ago)
        .unwrap();

    let tasks: Vec<String> = service
        .get("scout")
        .await
        .unwrap()
        .progress
        .into_iter()
        .map(|progress| progress.task)
        .collect();
    assert_eq!(tasks, vec!["newer", "older"]);
}

#[tokio::test]
async fn an_empty_state_closes_the_task_and_closing_twice_is_fine() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("scout", request("", &[])).await.unwrap();
    service.set_progress("scout", "migration", "half done").await.unwrap();

    let closed = service.set_progress("scout", "migration", "  ").await.unwrap();
    assert_eq!(
        closed,
        ProgressUpdate {
            task: "migration".into(),
            closed: true
        }
    );
    assert!(service.get("scout").await.unwrap().progress.is_empty());
    assert!(service.set_progress("scout", "migration", "").await.unwrap().closed);
}

#[tokio::test]
async fn a_task_name_becomes_a_file_name_that_cannot_leave_the_progress_directory() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    let role = service.upsert("scout", request("", &[])).await.unwrap();

    let named = |task: &'static str| {
        let service = service.clone();
        async move { service.set_progress("scout", task, "x").await.map(|update| update.task) }
    };
    assert_eq!(named("  Q4 report: draft #2 ").await.unwrap(), "q4-report-draft-2");
    assert_eq!(named("舆情 周报").await.unwrap(), "舆情-周报");
    assert_eq!(named("../../role").await.unwrap(), "role");
    assert_eq!(named("snake_case").await.unwrap(), "snake_case");
    assert!(matches!(named("../..").await, Err(RoleError::InvalidRequest(_))));
    assert!(matches!(named("   ").await, Err(RoleError::InvalidRequest(_))));
    let long: &'static str = Box::leak("a".repeat(MAX_TASK_NAME_CHARS + 1).into_boxed_str());
    assert!(matches!(named(long).await, Err(RoleError::InvalidRequest(_))));

    // Everything written went into the progress directory and nowhere else.
    let memory_dir = std::path::Path::new(&role.memory_dir);
    assert_eq!(std::fs::read_dir(memory_dir.join("progress")).unwrap().count(), 4);
    assert_eq!(std::fs::read_dir(memory_dir).unwrap().count(), 1);
    assert!(memory_dir.parent().unwrap().join("role.json").is_file());
}

#[tokio::test]
async fn progress_refuses_an_oversized_state_too_many_tasks_and_an_assistant_that_is_no_role() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    assert!(matches!(
        service.set_progress("scout", "task", "x").await,
        Err(RoleError::NotFound(_))
    ));
    service.upsert("scout", request("", &[])).await.unwrap();

    let oversized = "x".repeat(MAX_PROGRESS_STATE_CHARS + 1);
    assert!(matches!(
        service.set_progress("scout", "task", &oversized).await,
        Err(RoleError::InvalidRequest(_))
    ));

    for index in 0..MAX_PROGRESS_TASKS {
        service
            .set_progress("scout", &format!("task-{index}"), "x")
            .await
            .unwrap();
    }
    assert!(matches!(
        service.set_progress("scout", "one-too-many", "x").await,
        Err(RoleError::InvalidRequest(_))
    ));
    // A task already under way can still be updated, and closing one makes room.
    service.set_progress("scout", "task-0", "moved on").await.unwrap();
    service.set_progress("scout", "task-1", "").await.unwrap();
    service.set_progress("scout", "one-too-many", "x").await.unwrap();
}

#[tokio::test]
async fn curating_replaces_the_memory_and_keeps_what_it_replaced() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("reviewer", request("Reviews.", &[])).await.unwrap();
    service
        .set_memory("reviewer", "- run the tests\n- run the tests first\n- uses pnpm\n")
        .await
        .unwrap();

    let curated = service
        .curate_memory(
            "reviewer",
            "- run the tests first\n- uses pnpm\n\n\n",
            "2026-11-15-090000",
        )
        .await
        .unwrap();

    assert_eq!(curated.archived_as.as_deref(), Some("MEMORY-2026-11-15-090000.md"));
    assert_eq!(
        curated.bytes_before,
        "- run the tests\n- run the tests first\n- uses pnpm\n".len()
    );
    let role = service.get("reviewer").await.unwrap();
    assert_eq!(role.memory, "- run the tests first\n- uses pnpm\n");
    assert_eq!(curated.bytes_after, role.memory.len());
    let archived = std::path::Path::new(&role.memory_dir)
        .join("archive")
        .join("MEMORY-2026-11-15-090000.md");
    assert_eq!(
        std::fs::read_to_string(archived).unwrap(),
        "- run the tests\n- run the tests first\n- uses pnpm\n"
    );
}

#[tokio::test]
async fn curating_to_the_same_memory_writes_and_archives_nothing() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("reviewer", request("Reviews.", &[])).await.unwrap();
    service.set_memory("reviewer", "- uses pnpm\n").await.unwrap();

    let curated = service
        .curate_memory("reviewer", "- uses pnpm", "2026-11-15-090000")
        .await
        .unwrap();

    assert_eq!(curated.archived_as, None);
    let role = service.get("reviewer").await.unwrap();
    assert!(!std::path::Path::new(&role.memory_dir).join("archive").exists());
}

#[tokio::test]
async fn curating_refuses_an_empty_or_oversized_memory_and_a_stamp_that_is_no_file_name() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("reviewer", request("Reviews.", &[])).await.unwrap();
    service.set_memory("reviewer", "- uses pnpm\n").await.unwrap();

    for (memory, stamp) in [
        ("  \n", "2026-11-15-090000"),
        (&"x".repeat(MAX_MEMORY_BYTES + 1)[..], "2026-11-15-090000"),
        ("- tidy", "../escape"),
        ("- tidy", ""),
    ] {
        assert!(
            matches!(
                service.curate_memory("reviewer", memory, stamp).await,
                Err(RoleError::InvalidRequest(_))
            ),
            "stamp {stamp:?}"
        );
    }
    assert!(matches!(
        service.curate_memory("stranger", "- tidy", "2026-11-15-090000").await,
        Err(RoleError::NotFound(_))
    ));
    assert_eq!(service.get("reviewer").await.unwrap().memory, "- uses pnpm\n");
}

#[tokio::test]
async fn only_the_newest_replaced_memories_are_kept() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("reviewer", request("Reviews.", &[])).await.unwrap();
    service.set_memory("reviewer", "- version 0\n").await.unwrap();

    for round in 1..=MAX_MEMORY_ARCHIVES + 2 {
        service
            .curate_memory(
                "reviewer",
                &format!("- version {round}"),
                &format!("2026-11-15-{round:06}"),
            )
            .await
            .unwrap();
    }

    let role = service.get("reviewer").await.unwrap();
    let mut kept: Vec<String> = std::fs::read_dir(std::path::Path::new(&role.memory_dir).join("archive"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    kept.sort();
    assert_eq!(kept.len(), MAX_MEMORY_ARCHIVES);
    assert_eq!(kept[0], "MEMORY-2026-11-15-000003.md");
    assert_eq!(
        kept[MAX_MEMORY_ARCHIVES - 1],
        format!("MEMORY-2026-11-15-{:06}.md", MAX_MEMORY_ARCHIVES + 2)
    );
}

#[tokio::test]
async fn what_a_role_told_the_user_is_kept_newest_first_with_the_conversation_it_came_from() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("builtin-office", request("", &[])).await.unwrap();

    let first = service
        .record_report(
            "builtin-office",
            RoleReportOutcome::Say,
            "  the weekly figures\nare in  ",
            "conv_1",
            1_000,
        )
        .await
        .unwrap()
        .expect("a report that says something is kept");
    assert_eq!(first.summary, "the weekly figures are in");
    service
        .record_report(
            "builtin-office",
            RoleReportOutcome::Ask,
            "send it to finance as well?",
            "conv_2",
            2_000,
        )
        .await
        .unwrap();

    let reported = service.get("builtin-office").await.unwrap().reported;
    assert_eq!(reported.len(), 2);
    assert_eq!(reported[0].outcome, RoleReportOutcome::Ask);
    assert_eq!(reported[0].conversation_id, "conv_2");
    assert_eq!(reported[1], first);
}

#[tokio::test]
async fn a_silent_run_leaves_nothing_behind() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("builtin-office", request("", &[])).await.unwrap();

    let kept = service
        .record_report(
            "builtin-office",
            RoleReportOutcome::Silent,
            "nothing new",
            "conv_1",
            1_000,
        )
        .await
        .unwrap();

    assert_eq!(kept, None);
    assert!(service.get("builtin-office").await.unwrap().reported.is_empty());
}

#[tokio::test]
async fn a_report_needs_one_short_line_and_a_role_to_file_it_under() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("builtin-office", request("", &[])).await.unwrap();

    for outcome in [RoleReportOutcome::Say, RoleReportOutcome::Ask] {
        assert!(matches!(
            service
                .record_report("builtin-office", outcome, " \n ", "conv_1", 1)
                .await,
            Err(RoleError::InvalidRequest(_))
        ));
    }
    let long = "x".repeat(MAX_REPORT_SUMMARY_CHARS + 1);
    assert!(matches!(
        service
            .record_report("builtin-office", RoleReportOutcome::Say, &long, "conv_1", 1)
            .await,
        Err(RoleError::InvalidRequest(_))
    ));
    assert!(matches!(
        service
            .record_report("builtin-research", RoleReportOutcome::Say, "done", "conv_1", 1)
            .await,
        Err(RoleError::NotFound(_))
    ));
}

#[tokio::test]
async fn only_the_newest_reports_are_kept_and_a_line_that_does_not_parse_costs_only_itself() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    let role = service.upsert("builtin-office", request("", &[])).await.unwrap();

    for at in 0..(MAX_KEPT_REPORTS as i64 + 3) {
        service
            .record_report(
                "builtin-office",
                RoleReportOutcome::Say,
                &format!("report {at}"),
                "conv_1",
                at,
            )
            .await
            .unwrap();
    }
    let reported = service.get("builtin-office").await.unwrap().reported;
    assert_eq!(reported.len(), MAX_KEPT_REPORTS);
    assert_eq!(reported[0].summary, format!("report {}", MAX_KEPT_REPORTS + 2));
    assert_eq!(reported.last().unwrap().summary, "report 3");

    let file = std::path::Path::new(&role.memory_dir).join("reported.jsonl");
    let mut raw = std::fs::read_to_string(&file).unwrap();
    raw.push_str("not json\n");
    std::fs::write(&file, raw).unwrap();
    assert_eq!(
        service.get("builtin-office").await.unwrap().reported.len(),
        MAX_KEPT_REPORTS
    );
}

#[tokio::test]
async fn a_role_keeps_one_thread_and_hands_it_what_its_runs_told_the_user_once() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("builtin-office", request("", &[])).await.unwrap();
    service
        .record_report(
            "builtin-office",
            RoleReportOutcome::Say,
            "before the thread",
            "run_0",
            500,
        )
        .await
        .unwrap();

    // The first conversation to take a turn becomes the thread, and starts from
    // the standing section rather than from old reports.
    assert_eq!(
        service
            .open_thread_turn("builtin-office", "chat_1", false, 1_000)
            .await
            .unwrap(),
        ThreadTurn::Thread {
            reports: Vec::new(),
            adopted: true
        }
    );
    assert_eq!(
        service
            .get("builtin-office")
            .await
            .unwrap()
            .thread_conversation_id
            .as_deref(),
        Some("chat_1")
    );
    // Another conversation is a side conversation while the thread lives.
    assert_eq!(
        service
            .open_thread_turn("builtin-office", "chat_2", false, 1_100)
            .await
            .unwrap(),
        ThreadTurn::Aside
    );

    let ask = service
        .record_report(
            "builtin-office",
            RoleReportOutcome::Ask,
            "send it to finance?",
            "run_1",
            2_000,
        )
        .await
        .unwrap()
        .unwrap();
    // A report from the thread itself was seen there; it is not brought again.
    service
        .record_report(
            "builtin-office",
            RoleReportOutcome::Say,
            "said in the thread",
            "chat_1",
            2_100,
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .open_thread_turn("builtin-office", "chat_1", false, 3_000)
            .await
            .unwrap(),
        ThreadTurn::Thread {
            reports: vec![ask],
            adopted: false
        }
    );
    assert_eq!(
        service
            .open_thread_turn("builtin-office", "chat_1", false, 4_000)
            .await
            .unwrap(),
        ThreadTurn::Thread {
            reports: Vec::new(),
            adopted: false
        }
    );

    // Once the thread is gone, the next conversation takes its place.
    assert_eq!(
        service
            .open_thread_turn("builtin-office", "chat_2", true, 5_000)
            .await
            .unwrap(),
        ThreadTurn::Thread {
            reports: Vec::new(),
            adopted: true
        }
    );
    assert!(matches!(
        service.open_thread_turn("builtin-gone", "chat_9", false, 5_000).await,
        Err(RoleError::NotFound(_))
    ));
}

#[tokio::test]
async fn a_thread_keeps_its_summaries_in_order_and_only_moving_forward() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("builtin-office", request("", &[])).await.unwrap();
    let mark = |created_at, id: &str| ThreadMark {
        created_at,
        id: id.to_owned(),
    };

    // No thread yet: nothing to summarize, nothing kept.
    assert_eq!(service.thread_upkeep("builtin-office", "chat_1").await.unwrap(), None);
    assert!(
        !service
            .add_thread_leaf("builtin-office", "chat_1", "early", mark(10, "m1"))
            .await
            .unwrap()
    );

    service
        .open_thread_turn("builtin-office", "chat_1", false, 1_000)
        .await
        .unwrap();
    assert!(
        service
            .add_thread_leaf("builtin-office", "chat_1", "first stretch", mark(10, "m1"))
            .await
            .unwrap()
    );
    // A summary that reaches no further than the last one is not kept.
    assert!(
        !service
            .add_thread_leaf("builtin-office", "chat_1", "again", mark(10, "m1"))
            .await
            .unwrap()
    );
    assert!(
        service
            .add_thread_leaf("builtin-office", "chat_1", "second stretch", mark(20, "m2"))
            .await
            .unwrap()
    );
    assert!(
        service
            .mark_thread_rotated("builtin-office", "chat_1", 3_000)
            .await
            .unwrap()
    );
    assert_eq!(
        service.thread_upkeep("builtin-office", "chat_1").await.unwrap(),
        Some(ThreadUpkeep {
            summarized_to: Some(mark(20, "m2")),
            rotated_at: 3_000,
            leaves: vec!["first stretch".to_owned(), "second stretch".to_owned()],
        })
    );

    // Another conversation is not the thread, and an id that could leave the
    // role's directory is refused outright.
    assert_eq!(service.thread_upkeep("builtin-office", "chat_2").await.unwrap(), None);
    assert!(matches!(
        service.thread_upkeep("builtin-office", "../escape").await,
        Err(RoleError::InvalidId(_))
    ));

    // A thread that takes over starts with no summaries of its own.
    service
        .open_thread_turn("builtin-office", "chat_2", true, 4_000)
        .await
        .unwrap();
    assert_eq!(
        service.thread_upkeep("builtin-office", "chat_2").await.unwrap(),
        Some(ThreadUpkeep {
            summarized_to: None,
            rotated_at: 0,
            leaves: Vec::new(),
        })
    );
}

#[tokio::test]
async fn recall_indexes_thread_runs_leaves_and_supports_trigram_and_short_like() {
    use crate::{RecallConversationSync, RecallMessage, RecallSync, leaf_ref_id, message_ref_id};
    use aionui_api_types::{RoleRecallBound, RoleRecallKind, RoleRecallScope, RoleRecallSource};

    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("builtin-office", request("", &[])).await.unwrap();
    service
        .open_thread_turn("builtin-office", "thread_1", false, 1_000)
        .await
        .unwrap();
    service
        .add_thread_leaf(
            "builtin-office",
            "thread_1",
            "<!-- 2026-04-10 09:00 → 2026-04-10 10:00 · 12 messages -->\n- 敲定了季度预算与供应商名录",
            ThreadMark {
                created_at: 2_000,
                id: "m_early".into(),
            },
        )
        .await
        .unwrap();

    service
        .sync_recall(
            "builtin-office",
            RecallSync {
                active_thread: Some("thread_1".into()),
                active_runs: vec!["run_1".into()],
                batches: vec![
                    RecallConversationSync {
                        conversation_id: "thread_1".into(),
                        source: RoleRecallSource::Thread,
                        messages: vec![
                            RecallMessage {
                                id: "m1".into(),
                                created_at: 10_000,
                                speaker: "user".into(),
                                text: "请帮我做数据源核对，特别是 Q3 周报的口径".into(),
                            },
                            RecallMessage {
                                id: "m2".into(),
                                created_at: 11_000,
                                speaker: "assistant".into(),
                                text: "好的，数据源核对已完成，差异率为 100%_verified%".into(),
                            },
                        ],
                        watermark: Some((11_000, "m2".into())),
                    },
                    RecallConversationSync {
                        conversation_id: "run_1".into(),
                        source: RoleRecallSource::Run,
                        messages: vec![RecallMessage {
                            id: "r1".into(),
                            created_at: 20_000,
                            speaker: "assistant".into(),
                            text: "早间巡检发现网关超时告警，数据源核对正常".into(),
                        }],
                        watermark: Some((20_000, "r1".into())),
                    },
                ],
            },
        )
        .await
        .unwrap();

    // 3+ char Chinese trigram match across thread + runs.
    let trigram = service
        .recall(
            "builtin-office",
            &RoleRecallRequest {
                query: "数据源核对".into(),
                limit: None,
                scope: None,
                before: None,
                after: None,
            },
            25_000,
        )
        .await
        .unwrap();
    assert_eq!(trigram.hits.len(), 3);

    // Scope filtering separates thread from scheduled runs.
    let runs_only = service
        .recall(
            "builtin-office",
            &RoleRecallRequest {
                query: "数据源核对".into(),
                limit: None,
                scope: Some(RoleRecallScope::Runs),
                before: None,
                after: None,
            },
            25_000,
        )
        .await
        .unwrap();
    assert_eq!(runs_only.hits.len(), 1);
    assert_eq!(runs_only.hits[0].ref_id, message_ref_id("run_1", "r1"));
    assert_eq!(runs_only.hits[0].source, RoleRecallSource::Run);

    // 2-char Chinese query falls back to LIKE cleanly.
    let short = service
        .recall(
            "builtin-office",
            &RoleRecallRequest {
                query: "周报".into(),
                limit: None,
                scope: None,
                before: None,
                after: None,
            },
            25_000,
        )
        .await
        .unwrap();
    assert_eq!(short.hits.len(), 1);
    assert_eq!(short.hits[0].ref_id, message_ref_id("thread_1", "m1"));

    // Short LIKE terms escape SQL wildcards (`%` and `_`): `"%_"` matches only
    // `m2` (`100%_verified%`), while `"%%"` matches nothing instead of everything.
    let literal_wildcard = service
        .recall(
            "builtin-office",
            &RoleRecallRequest {
                query: "%_".into(),
                limit: None,
                scope: None,
                before: None,
                after: None,
            },
            25_000,
        )
        .await
        .unwrap();
    assert_eq!(literal_wildcard.hits.len(), 1);
    assert_eq!(literal_wildcard.hits[0].ref_id, message_ref_id("thread_1", "m2"));

    let escaped = service
        .recall(
            "builtin-office",
            &RoleRecallRequest {
                query: "%%".into(),
                limit: None,
                scope: None,
                before: None,
                after: None,
            },
            25_000,
        )
        .await
        .unwrap();
    assert!(escaped.hits.is_empty());

    // Summary leaves are indexed with their span header preserved.
    let leaf_hit = service
        .recall(
            "builtin-office",
            &RoleRecallRequest {
                query: "供应商名录".into(),
                limit: None,
                scope: Some(RoleRecallScope::Thread),
                before: None,
                after: None,
            },
            25_000,
        )
        .await
        .unwrap();
    assert_eq!(leaf_hit.hits.len(), 1);
    assert_eq!(leaf_hit.hits[0].ref_id, leaf_ref_id("thread_1", "00001"));
    assert_eq!(leaf_hit.hits[0].kind, RoleRecallKind::Leaf);
    assert_eq!(
        leaf_hit.hits[0].span.as_deref(),
        Some("2026-04-10 09:00 → 2026-04-10 10:00 · 12 messages")
    );

    // Time bounds (`after` / `before`) filter accurately.
    let bounded = service
        .recall(
            "builtin-office",
            &RoleRecallRequest {
                query: "数据源核对".into(),
                limit: None,
                scope: None,
                before: Some(RoleRecallBound::Millis(15_000)),
                after: Some(RoleRecallBound::Millis(10_500)),
            },
            25_000,
        )
        .await
        .unwrap();
    assert_eq!(bounded.hits.len(), 1);
    assert_eq!(bounded.hits[0].ref_id, message_ref_id("thread_1", "m2"));
}

#[tokio::test]
async fn recall_isolates_roles_expands_windows_and_rebuilds_when_db_deleted() {
    use crate::{RecallConversationSync, RecallMessage, RecallSync, leaf_ref_id, message_ref_id};
    use aionui_api_types::{RoleRecallKind, RoleRecallSource};

    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("role-a", request("", &[])).await.unwrap();
    service.upsert("role-b", request("", &[])).await.unwrap();
    service
        .open_thread_turn("role-a", "thread_a", false, 1_000)
        .await
        .unwrap();
    service
        .add_thread_leaf(
            "role-a",
            "thread_a",
            "<!-- 2026-04-11 08:00 → 2026-04-11 09:00 · 4 messages -->\n- 归档结论：采用蓝绿发布方案",
            ThreadMark {
                created_at: 500,
                id: "m0".into(),
            },
        )
        .await
        .unwrap();

    let huge = format!("HEAD-{}-TAIL", "x".repeat(5_000));
    service
        .sync_recall(
            "role-a",
            RecallSync {
                active_thread: Some("thread_a".into()),
                active_runs: Vec::new(),
                batches: vec![RecallConversationSync {
                    conversation_id: "thread_a".into(),
                    source: RoleRecallSource::Thread,
                    messages: vec![
                        RecallMessage {
                            id: "m1".into(),
                            created_at: 1_000,
                            speaker: "user".into(),
                            text: "第一条上下文".into(),
                        },
                        RecallMessage {
                            id: "m2".into(),
                            created_at: 2_000,
                            speaker: "assistant".into(),
                            text: "命中核心机密密钥 alpha-secret-token".into(),
                        },
                        RecallMessage {
                            id: "m3".into(),
                            created_at: 3_000,
                            speaker: "user".into(),
                            text: huge,
                        },
                    ],
                    watermark: Some((3_000, "m3".into())),
                }],
            },
        )
        .await
        .unwrap();

    // Role B cannot recall or expand Role A's documents.
    let b_hits = service
        .recall(
            "role-b",
            &RoleRecallRequest {
                query: "alpha-secret-token".into(),
                limit: None,
                scope: None,
                before: None,
                after: None,
            },
            5_000,
        )
        .await
        .unwrap();
    assert!(b_hits.hits.is_empty());
    let b_expand = service
        .expand(
            "role-b",
            &RoleExpandRequest {
                ref_id: message_ref_id("thread_a", "m2"),
                before: None,
                after: None,
            },
        )
        .await;
    assert!(matches!(b_expand, Err(RoleError::InvalidRequest(_))));

    // Role A expands message window and truncates oversized neighbor cleanly.
    let expanded = service
        .expand(
            "role-a",
            &RoleExpandRequest {
                ref_id: message_ref_id("thread_a", "m2"),
                before: Some(1),
                after: Some(1),
            },
        )
        .await
        .unwrap();
    assert_eq!(expanded.kind, RoleRecallKind::Message);
    assert_eq!(expanded.messages.len(), 3);
    assert!(!expanded.messages[0].hit);
    assert!(expanded.messages[1].hit);
    assert!(!expanded.messages[2].hit);
    assert!(expanded.truncated);
    assert!(expanded.messages[2].text.contains("characters left out"));

    // Expanding a leaf returns its full body and span.
    let leaf_expanded = service
        .expand(
            "role-a",
            &RoleExpandRequest {
                ref_id: leaf_ref_id("thread_a", "00001"),
                before: None,
                after: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(leaf_expanded.kind, RoleRecallKind::Leaf);
    assert_eq!(
        leaf_expanded.span.as_deref(),
        Some("2026-04-11 08:00 → 2026-04-11 09:00 · 4 messages")
    );
    assert_eq!(leaf_expanded.leaf.as_deref(), Some("- 归档结论：采用蓝绿发布方案"));

    // Watermark idempotency: a no-op sync keeps watermarks and avoids duplicates.
    let wm = service.recall_watermarks("role-a").await.unwrap();
    assert_eq!(wm["thread_a"].created_at, 3_000);
    assert_eq!(wm["thread_a"].id, "m3");
    assert_eq!(wm["thread_a"].leaves_count, 1);

    // Deleting recall.db resets watermarks and allows clean live rebuild.
    std::fs::remove_file(root.path().join("roles/recall.db")).unwrap();
    assert!(service.recall_watermarks("role-a").await.unwrap().is_empty());
    service
        .sync_recall(
            "role-a",
            RecallSync {
                active_thread: Some("thread_a".into()),
                active_runs: Vec::new(),
                batches: Vec::new(),
            },
        )
        .await
        .unwrap();
    let rebuilt = service
        .recall(
            "role-a",
            &RoleRecallRequest {
                query: "蓝绿发布方案".into(),
                limit: None,
                scope: None,
                before: None,
                after: None,
            },
            5_000,
        )
        .await
        .unwrap();
    assert_eq!(rebuilt.hits.len(), 1);
}

#[tokio::test]
async fn playbook_deltas_boss_profile_decay_condensation_and_git_revert_preserve_runtime_files() {
    use aionui_api_types::PlaybookDeltaOp;

    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service
        .upsert("analyst", request("Owns weekly growth metrics.", &[]))
        .await
        .unwrap();

    // Runtime files (thread, leaves, progress) must survive git commits and reverts untouched.
    service
        .open_thread_turn("analyst", "thread_live", false, 1_000)
        .await
        .unwrap();
    service
        .set_progress("analyst", "q4-audit", "Step 1 done")
        .await
        .unwrap();

    // S4: Apply incremental ACE retrospective deltas to PLAYBOOK.md + shared _boss/USER.md.
    let day0 = 1_000_000_000_i64;
    let (applied, boss_changed) = service
        .apply_retrospective(
            "analyst",
            &[
                PlaybookDeltaOp::Add {
                    content: "发周报前先核对数据源".into(),
                },
                PlaybookDeltaOp::Add {
                    content: "异常波动需附同比与环比".into(),
                },
            ],
            &[PlaybookDeltaOp::Add {
                content: "老板阅读偏好：先给结论，再列数据".into(),
            }],
            "reflector",
            "thread rotation review",
            day0,
        )
        .await
        .unwrap();
    assert_eq!(applied, 2);
    assert!(boss_changed);
    assert!(service.boss_profile().await.unwrap().contains("[u-0001] 老板阅读偏好"));

    // Update p-0001 (+3 helpful) and mark p-0002 harmful (-2) so zero-LLM decay distinguishes them.
    service
        .apply_retrospective(
            "analyst",
            &[
                PlaybookDeltaOp::Update {
                    id: "p-0001".into(),
                    content: None,
                    helpful_delta: 3,
                    harmful_delta: 0,
                },
                PlaybookDeltaOp::Update {
                    id: "p-0002".into(),
                    content: None,
                    helpful_delta: 0,
                    harmful_delta: 2,
                },
            ],
            &[],
            "reflector",
            "user correction",
            day0 + 1_000,
        )
        .await
        .unwrap();

    // S5: Zero-LLM decay archives contradicted p-0002 while keeping reinforced p-0001,
    // and commits the change so it can even be reverted from timeline!
    let (decayed, _) = service
        .run_zero_llm_decay("analyst", "2026-11-17", day0 + 2_000)
        .await
        .unwrap();
    assert_eq!(decayed, 1);
    let after_decay = service.get("analyst").await.unwrap();
    assert_eq!(after_decay.playbook.len(), 1);
    assert_eq!(after_decay.playbook[0].id, "p-0001");
    assert_eq!(after_decay.playbook[0].helpful, 4);

    // Reverting the decay commit restores p-0002 without touching thread.json or progress/!
    let timeline = service.timeline("analyst", 20).await.unwrap();
    assert!(timeline[0].message.contains("decay:"));
    let restored = service.revert("analyst", &timeline[0].sha, day0 + 3_000).await.unwrap();
    assert_eq!(restored.playbook.len(), 2);
    assert_eq!(
        restored.thread_conversation_id.as_deref(),
        Some("thread_live"),
        "thread.json must not be disturbed by git revert"
    );
    assert_eq!(
        restored.progress.len(),
        1,
        "progress files must not be disturbed by git revert"
    );

    // S5: LCM leaf condensation merges 6 D0 leaves into condensed/00001.md while keeping tail D0 leaves.
    for idx in 1..=7 {
        service
            .add_thread_leaf(
                "analyst",
                "thread_live",
                &format!("<!-- 2026-11-17 0{idx}:00 → 2026-11-17 0{idx}:30 · 4 messages -->\n- leaf {idx}"),
                ThreadMark {
                    created_at: (idx * 100) as i64,
                    id: format!("m{idx}"),
                },
            )
            .await
            .unwrap();
    }
    let (conv_id, start, batch) = service
        .next_uncondensed_leaf_batch("analyst", 6)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conv_id, "thread_live");
    assert_eq!(start, 0);
    assert_eq!(batch.len(), 6);
    assert!(
        service
            .record_condensed_leaf(
                "analyst",
                "thread_live",
                0,
                6,
                "<!-- 2026-11-17 01:00 → 2026-11-17 06:30 · condensed 6 summaries -->\n- merged 1..6\n",
            )
            .await
            .unwrap()
    );
    let upkeep = service.thread_upkeep("analyst", "thread_live").await.unwrap().unwrap();
    assert_eq!(upkeep.leaves.len(), 2);
    assert!(upkeep.leaves[0].contains("merged 1..6"));
    assert!(upkeep.leaves[1].contains("leaf 7"));
}

fn claims(projects: &[(&str, &str)]) -> SetRoleProjectsRequest {
    SetRoleProjectsRequest {
        projects: projects
            .iter()
            .map(|(project_id, name)| RoleProjectClaim {
                project_id: (*project_id).to_owned(),
                name: (*name).to_owned(),
            })
            .collect(),
        transfer: false,
    }
}

#[tokio::test]
async fn a_role_keeps_projects_and_saving_its_charter_leaves_them_alone() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service
        .upsert("seo", request("Watches search traffic.", &[]))
        .await
        .unwrap();

    let role = service
        .set_projects(
            "seo",
            claims(&[("project_a", "  yipan\nstore "), ("project_a", "dup")]),
            1_000,
        )
        .await
        .unwrap();
    assert_eq!(
        role.projects,
        vec![RoleKeptProject {
            project_id: "project_a".into(),
            name: "yipan store".into(),
            claimed_at: 1_000,
        }]
    );

    let role = service
        .set_projects("seo", claims(&[("project_b", "gallery"), ("project_a", "")]), 2_000)
        .await
        .unwrap();
    let kept: Vec<(&str, &str, i64)> = role
        .projects
        .iter()
        .map(|k| (k.project_id.as_str(), k.name.as_str(), k.claimed_at))
        .collect();
    assert_eq!(
        kept,
        vec![("project_b", "gallery", 2_000), ("project_a", "yipan store", 1_000)]
    );

    let role = service
        .upsert("seo", request("Watches search traffic daily.", &[]))
        .await
        .unwrap();
    assert_eq!(role.projects.len(), 2);

    let keepers = service.keepers().await.unwrap();
    let pairs: Vec<(&str, &str)> = keepers
        .iter()
        .map(|k| (k.project_id.as_str(), k.role_id.as_str()))
        .collect();
    assert_eq!(pairs, vec![("project_a", "seo"), ("project_b", "seo")]);

    let timeline = service.timeline("seo", 10).await.unwrap();
    let messages: Vec<&str> = timeline.iter().map(|c| c.message.as_str()).collect();
    assert!(messages.contains(&"projects: took on gallery"), "{messages:?}");
    assert!(messages.contains(&"projects: took on yipan store"), "{messages:?}");
}

#[tokio::test]
async fn a_project_has_one_keeper_and_changes_hands_only_on_transfer() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("chief", request("", &[])).await.unwrap();
    service.upsert("seo", request("", &[])).await.unwrap();
    service
        .set_projects(
            "chief",
            claims(&[("project_j", "journal"), ("project_k", "notes")]),
            1_000,
        )
        .await
        .unwrap();

    let refused = service
        .set_projects("seo", claims(&[("project_j", "journal")]), 2_000)
        .await;
    assert!(matches!(refused, Err(RoleError::Conflict(_))), "{refused:?}");
    assert!(service.get("seo").await.unwrap().projects.is_empty());

    let mut transfer = claims(&[("project_j", "journal")]);
    transfer.transfer = true;
    let seo = service.set_projects("seo", transfer, 3_000).await.unwrap();
    assert_eq!(seo.projects[0].claimed_at, 3_000);
    let chief = service.get("chief").await.unwrap();
    let left: Vec<&str> = chief.projects.iter().map(|k| k.project_id.as_str()).collect();
    assert_eq!(left, vec!["project_k"]);
    let handover = service.timeline("chief", 1).await.unwrap();
    assert_eq!(handover[0].message, "projects: handed journal to seo");

    let keepers = service.keepers().await.unwrap();
    let pairs: Vec<(&str, &str)> = keepers
        .iter()
        .map(|k| (k.project_id.as_str(), k.role_id.as_str()))
        .collect();
    assert_eq!(pairs, vec![("project_j", "seo"), ("project_k", "chief")]);
}

#[tokio::test]
async fn letting_go_is_recorded_and_bad_project_ids_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("seo", request("", &[])).await.unwrap();
    service
        .set_projects("seo", claims(&[("project_a", "store")]), 1_000)
        .await
        .unwrap();

    let role = service.set_projects("seo", claims(&[]), 2_000).await.unwrap();
    assert!(role.projects.is_empty());
    assert_eq!(
        service.timeline("seo", 1).await.unwrap()[0].message,
        "projects: let go of store"
    );
    assert!(service.keepers().await.unwrap().is_empty());

    for bad in ["", "../escape", "has space"] {
        let result = service.set_projects("seo", claims(&[(bad, "x")]), 3_000).await;
        assert!(matches!(result, Err(RoleError::InvalidRequest(_))), "{bad}: {result:?}");
    }
    let missing = service
        .set_projects("nobody", claims(&[("project_a", "")]), 3_000)
        .await;
    assert!(matches!(missing, Err(RoleError::NotFound(_))));
}

#[tokio::test]
async fn when_a_revert_brings_back_an_old_claim_the_later_claim_keeps_the_project() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service.upsert("chief", request("", &[])).await.unwrap();
    service.upsert("seo", request("", &[])).await.unwrap();
    service
        .set_projects("seo", claims(&[("project_j", "journal")]), 5_000)
        .await
        .unwrap();
    let mut stale = service.store.read_definition("chief").await.unwrap().unwrap();
    stale.projects = vec![RoleKeptProject {
        project_id: "project_j".into(),
        name: "journal".into(),
        claimed_at: 1_000,
    }];
    service.store.write_definition("chief", &stale).await.unwrap();

    let keepers = service.keepers().await.unwrap();
    assert_eq!(keepers.len(), 1);
    assert_eq!(keepers[0].role_id, "seo");
}

#[tokio::test]
async fn a_kept_project_is_briefed_by_its_keeper_but_never_the_keepers_own_conversation() {
    let root = tempfile::tempdir().unwrap();
    let service = service(&root);
    service
        .upsert("seo", request("Watches search traffic.\n\nNever edits prices.", &[]))
        .await
        .unwrap();
    assert_eq!(service.keeper_brief("project_a", "", None).await.unwrap(), None);

    service
        .set_projects("seo", claims(&[("project_a", "yipan-store")]), 1_000)
        .await
        .unwrap();
    // A keeper that knows nothing of the project hands nothing over.
    assert_eq!(service.keeper_brief("project_a", "", None).await.unwrap(), None);

    service
        .remember("seo", "robots.txt on yipan-store blocks /cart", "2026-11-16")
        .await
        .unwrap();
    let brief = service
        .keeper_brief("project_a", "fix robots", None)
        .await
        .unwrap()
        .expect("a brief");
    assert_eq!(brief.role_id, "seo");
    assert_eq!(brief.project_name, "yipan-store");
    assert_eq!(brief.charter, "Watches search traffic.");
    assert_eq!(brief.memory, vec!["2026-11-16 robots.txt on yipan-store blocks /cart"]);
    assert_eq!(brief.last_stop, None);

    // The keeper's own conversations open with its whole standing section instead.
    assert_eq!(service.keeper_brief("project_a", "", Some("seo")).await.unwrap(), None);
    // An id no project could have is no project.
    assert_eq!(service.keeper_brief("../seo", "", None).await.unwrap(), None);

    let ledger = root.path().join("roles/seo/projects");
    std::fs::create_dir_all(&ledger).unwrap();
    std::fs::write(
        ledger.join("project_a.json"),
        r#"{"last_stop":{"text":"sitemap half submitted","conversation_id":"c9","at":5}}"#,
    )
    .unwrap();
    let brief = service.keeper_brief("project_a", "", None).await.unwrap().unwrap();
    assert_eq!(
        brief.last_stop.map(|stop| (stop.text, stop.conversation_id)),
        Some(("sitemap half submitted".to_owned(), "c9".to_owned()))
    );
}
