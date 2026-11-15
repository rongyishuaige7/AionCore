//! A follow-up: a one-shot timer an agent sets for itself from inside a
//! conversation, so a matter it cannot finish now comes back to it later.
//!
//! It is stored as an ordinary job — agent-created, one-shot, bound to the
//! conversation that asked — so it survives a restart and shows up beside the
//! other scheduled tasks while it waits. Nothing else an agent creates is
//! one-shot, which is what tells a follow-up apart without a column of its own.
//! Once delivered it is removed rather than left behind disabled.

use aionui_common::TimestampMs;

use crate::types::{CreatedBy, CronJob, CronSchedule};

/// How many follow-ups one conversation may have waiting at a time.
pub const MAX_PENDING_FOLLOW_UPS: usize = 5;
/// The furthest ahead a follow-up may be set: a week.
pub const MAX_FOLLOW_UP_MINUTES: u32 = 7 * 24 * 60;
/// The longest note a follow-up carries.
pub const MAX_FOLLOW_UP_NOTE_CHARS: usize = 2_000;
/// A follow-up that finds its conversation busy keeps trying for about ten
/// minutes before it is given up on, where a recurring task simply waits for
/// its next occurrence.
pub(crate) const FOLLOW_UP_MAX_RETRIES: i64 = 20;

const NAME_CHARS: usize = 60;

/// A follow-up that was just set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUp {
    pub job_id: String,
    pub due_at: TimestampMs,
    /// How many follow-ups the conversation now has waiting, this one included.
    pub pending: usize,
}

/// Whether `job` is a follow-up an agent set for itself.
pub fn is_follow_up(job: &CronJob) -> bool {
    job.created_by == CreatedBy::Agent && matches!(job.schedule, CronSchedule::At { .. })
}

/// The name a follow-up is listed under: the first line of its note, cut short.
pub(crate) fn follow_up_name(note: &str) -> String {
    let first_line = note.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    let mut name: String = first_line.chars().take(NAME_CHARS).collect();
    if first_line.chars().count() > NAME_CHARS {
        name.push('…');
    }
    name
}

/// What the agent reads when its follow-up comes due.
pub fn build_follow_up_prompt(note: &str) -> String {
    format!(
        "[Follow-up you set for yourself]\nThis is not a message from the user. Earlier you asked to be woken now, with this note:\n\n{note}\n\nPick the matter up from where you left it. If it turns out there is nothing to do yet, set another follow-up rather than waiting."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ExecutionMode;

    fn job(created_by: CreatedBy, schedule: CronSchedule) -> CronJob {
        CronJob {
            id: "cron_1".into(),
            user_id: "u1".into(),
            name: "n".into(),
            enabled: true,
            schedule,
            message: "m".into(),
            execution_mode: ExecutionMode::Existing,
            agent_config: None,
            conversation_id: "conv_1".into(),
            conversation_title: None,
            agent_type: "acp".into(),
            created_by,
            skill_content: None,
            description: None,
            created_at: 0,
            updated_at: 0,
            next_run_at: None,
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
            queue_enabled: false,
        }
    }

    fn once() -> CronSchedule {
        CronSchedule::At {
            at_ms: 1,
            description: None,
        }
    }

    fn recurring() -> CronSchedule {
        CronSchedule::Every {
            every_ms: 60_000,
            description: None,
        }
    }

    #[test]
    fn only_a_one_shot_job_an_agent_made_is_a_follow_up() {
        assert!(is_follow_up(&job(CreatedBy::Agent, once())));
        assert!(!is_follow_up(&job(CreatedBy::Agent, recurring())));
        assert!(!is_follow_up(&job(CreatedBy::User, once())));
    }

    #[test]
    fn a_follow_up_is_listed_under_the_first_line_of_its_note() {
        assert_eq!(
            follow_up_name("\n  check the deploy  \nand the logs"),
            "check the deploy"
        );
        let long = "x".repeat(NAME_CHARS + 5);
        let name = follow_up_name(&long);
        assert_eq!(name.chars().count(), NAME_CHARS + 1);
        assert!(name.ends_with('…'));
    }

    #[test]
    fn the_wake_up_carries_the_note_and_says_the_user_did_not_write_it() {
        let prompt = build_follow_up_prompt("check the deploy");
        assert!(prompt.contains("check the deploy"));
        assert!(prompt.contains("not a message from the user"));
    }
}
