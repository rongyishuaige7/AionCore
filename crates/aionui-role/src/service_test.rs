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
