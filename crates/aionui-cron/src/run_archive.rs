//! Keeps a scheduled job's old runs out of the conversation list.
//!
//! A job that starts a conversation per run leaves one behind each time; a
//! half-hourly job leaves hundreds a week, and every list request carries
//! them. Once a new run starts, the job's runs beyond its newest few that
//! nobody has touched for a day move to the archive, where they can still be
//! read and answered.

use aionui_common::{TimestampMs, now_ms};
use aionui_db::IConversationRepository;
use tracing::{info, warn};

/// Runs of a job that stay in the conversation list whatever their age.
const KEEP_RECENT_RUNS: u32 = 10;
/// A run nobody has touched for this long may go to the archive.
const ARCHIVE_IDLE_AFTER_MS: TimestampMs = 24 * 60 * 60 * 1000;

/// Archives the stale runs of `job_id`, never `own_conversation_id` (the
/// conversation the job belongs to). Best-effort: a failure is logged and the
/// new run goes ahead.
pub(crate) async fn archive_stale_runs(
    repo: &dyn IConversationRepository,
    user_id: &str,
    job_id: &str,
    own_conversation_id: &str,
) {
    let now = now_ms();
    match repo
        .archive_stale_cron_runs(
            user_id,
            job_id,
            own_conversation_id,
            KEEP_RECENT_RUNS,
            now - ARCHIVE_IDLE_AFTER_MS,
            now,
        )
        .await
    {
        Ok(ids) if !ids.is_empty() => {
            info!(job_id = %job_id, archived = ids.len(), "Archived earlier runs of cron job");
        }
        Ok(_) => {}
        Err(error) => {
            warn!(job_id = %job_id, error = %error, "Archiving earlier runs of cron job failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::models::ConversationRow;
    use aionui_db::{SqliteConversationRepository, init_database_memory};

    const USER: &str = "system_default_user";
    const HOUR_MS: TimestampMs = 60 * 60 * 1000;

    fn run(id: &str, created_at: TimestampMs, updated_at: TimestampMs) -> ConversationRow {
        ConversationRow {
            id: id.into(),
            user_id: USER.into(),
            name: "Patrol".into(),
            r#type: "acp".into(),
            extra: r#"{"cron_job_id":"cron-1"}"#.into(),
            model: None,
            status: Some("finished".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at,
            updated_at,
            project_id: None,
            folder_id: None,
            name_source: None,
        }
    }

    async fn archived_ids(db: &aionui_db::Database) -> Vec<String> {
        sqlx::query_scalar("SELECT id FROM conversations WHERE archived_at IS NOT NULL ORDER BY created_at")
            .fetch_all(db.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn keeps_the_newest_ten_runs_and_any_run_touched_within_a_day() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteConversationRepository::new(db.pool().clone());
        let now = now_ms();
        let two_days_ago = now - 48 * HOUR_MS;

        // Oldest first: one run answered an hour ago, then twelve untouched ones.
        repo.create(&run("touched", two_days_ago, now - HOUR_MS)).await.unwrap();
        for index in 0..12 {
            let id = format!("run-{index:02}");
            repo.create(&run(&id, two_days_ago + 1 + index, two_days_ago + 1 + index))
                .await
                .unwrap();
        }

        archive_stale_runs(&repo, USER, "cron-1", "thread").await;

        assert_eq!(archived_ids(&db).await, vec!["run-00", "run-01"]);
    }
}
