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
