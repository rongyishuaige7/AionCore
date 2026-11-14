//! Review gate on the task board.
//!
//! A team with a member spawned from a reviewing role does not let the other
//! members close work on their own say-so: when one of them asks for
//! `completed`, the task is held for the reviewer instead. The hold is a mark
//! in the task's metadata on an `in_progress` task, so the task table keeps
//! its four statuses. A team without a reviewer behaves as before.

use aionui_common::TimestampMs;
use serde_json::{Map, Value, json};

use crate::ports::TeamRolePort;
use crate::types::{TaskStatus, TeamAgent, TeamTask};

const REVIEW_KEY: &str = "review";

/// Told to a member whose `completed` was turned into a hold.
pub(crate) const HELD_NOTE: &str = "Held for review: this task counts as completed once a reviewer passes it. Do not \
                                    wait for that; go on with your other work or end your turn.";

/// Where a task stands with its reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewState {
    /// Handed in, not yet looked at.
    Waiting,
    Passed,
    /// Sent back to its owner.
    Returned,
}

impl ReviewState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Passed => "passed",
            Self::Returned => "returned",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "waiting" => Some(Self::Waiting),
            "passed" => Some(Self::Passed),
            "returned" => Some(Self::Returned),
            _ => None,
        }
    }
}

/// The review mark a task carries, if any.
pub(crate) fn review_state(task: &TeamTask) -> Option<ReviewState> {
    task.metadata
        .as_ref()?
        .get(REVIEW_KEY)?
        .get("state")?
        .as_str()
        .and_then(ReviewState::parse)
}

/// `metadata` with its review mark set to `state`, or removed when `state` is
/// `None`. Every other key is kept.
pub(crate) fn with_review_mark(
    metadata: Option<&Value>,
    state: Option<ReviewState>,
    by_slot_id: &str,
    at: TimestampMs,
) -> Value {
    let mut object = match metadata {
        Some(Value::Object(object)) => object.clone(),
        _ => Map::new(),
    };
    match state {
        Some(state) => {
            object.insert(
                REVIEW_KEY.to_owned(),
                json!({ "state": state.as_str(), "by": by_slot_id, "at": at }),
            );
        }
        None => {
            object.remove(REVIEW_KEY);
        }
    }
    Value::Object(object)
}

/// What the gate makes of one `team_task_update` call that sets a status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewRuling {
    /// The gate has nothing to say: the update goes through as asked.
    Untouched,
    /// A member that does not review asked for `completed`: the task stays in
    /// progress and waits for a reviewer.
    Hold { already_waiting: bool },
    /// A reviewer completed a waiting task.
    Pass,
    /// A reviewer put a waiting task back to work.
    Return,
    /// The update goes through as asked and an open review mark is dropped.
    Withdraw,
}

/// Rule on a status change. `has_other_reviewer` is whether the team has a
/// reviewer besides the caller.
pub(crate) fn rule_on_update(
    requested: TaskStatus,
    current: Option<ReviewState>,
    caller_reviews: bool,
    has_other_reviewer: bool,
) -> ReviewRuling {
    let waiting = current == Some(ReviewState::Waiting);
    if caller_reviews && waiting {
        return match requested {
            TaskStatus::Completed => ReviewRuling::Pass,
            TaskStatus::Pending | TaskStatus::InProgress => ReviewRuling::Return,
            TaskStatus::Deleted => ReviewRuling::Withdraw,
        };
    }
    if !caller_reviews && requested == TaskStatus::Completed && has_other_reviewer {
        return ReviewRuling::Hold {
            already_waiting: waiting,
        };
    }
    match current {
        Some(ReviewState::Waiting | ReviewState::Returned) => ReviewRuling::Withdraw,
        Some(ReviewState::Passed) | None => ReviewRuling::Untouched,
    }
}

/// The members among `agents` that were spawned from a reviewing role. A role
/// that cannot be read is an error, so a broken role file never opens the gate.
pub(crate) async fn reviewers_among(
    agents: Vec<TeamAgent>,
    roles: &dyn TeamRolePort,
) -> Result<Vec<TeamAgent>, String> {
    let mut reviewers = Vec::new();
    for agent in agents {
        let Some(assistant_id) = agent.assistant_id.as_deref() else {
            continue;
        };
        match roles.role_brief(assistant_id).await {
            Ok(Some(brief)) if brief.verifies => reviewers.push(agent),
            Ok(_) => {}
            Err(error) => return Err(format!("the role of {} cannot be read: {error}", agent.name)),
        }
    }
    Ok(reviewers)
}

/// The status a member sees for a task: the board status, with the review
/// mark spelled out while it matters.
pub(crate) fn status_label(task: &TeamTask) -> String {
    match (task.status, review_state(task)) {
        (TaskStatus::Pending | TaskStatus::InProgress, Some(ReviewState::Waiting)) => {
            format!("{} (waiting for review)", task.status)
        }
        (TaskStatus::Pending | TaskStatus::InProgress, Some(ReviewState::Returned)) => {
            format!("{} (returned from review)", task.status)
        }
        _ => task.status.to_string(),
    }
}

/// Mailbox notice for a reviewer when a task is handed in.
pub(crate) fn review_request_notice(task: &TeamTask, submitted_by: &str) -> String {
    let mut notice = format!("🔎 Task waiting for your review (#{}): {}", task.id, task.subject);
    if let Some(description) = task.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        notice.push_str("\n\n");
        notice.push_str(description);
    }
    notice.push_str(&format!(
        "\n\nHanded in by {submitted_by}. Check the work itself, not the account of it. To pass it, call \
         team_task_update with task_id \"{id}\" and status \"completed\". To return it, call team_task_update with \
         status \"in_progress\" and tell the owner what is missing with team_send_message.",
        id = task.id
    ));
    notice
}

/// Mailbox notice for a task's owner when a reviewer sends it back.
pub(crate) fn review_returned_notice(task: &TeamTask, reviewer: &str) -> String {
    format!(
        "↩️ Task returned from review (#{}): {}\n\n{reviewer} did not pass it. Read their message for what is \
         missing, fix it, then mark the task completed again.",
        task.id, task.subject
    )
}

/// Mailbox notice for the Lead when a reviewer passes a task.
pub(crate) fn review_passed_notice(task: &TeamTask, reviewer: &str) -> String {
    format!(
        "✅ Task passed review (#{}): {}\n\nPassed by {reviewer}; it is completed.",
        task.id, task.subject
    )
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::ports::TeamRoleBrief;
    use crate::types::TeammateRole;

    fn task(status: TaskStatus, metadata: Option<Value>) -> TeamTask {
        TeamTask {
            id: "tk1".into(),
            team_id: "t1".into(),
            subject: "Build the page".into(),
            description: Some("With tests.".into()),
            status,
            owner: Some("worker-1".into()),
            blocked_by: vec![],
            blocks: vec![],
            metadata,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn agent(slot_id: &str, assistant_id: Option<&str>) -> TeamAgent {
        TeamAgent {
            slot_id: slot_id.into(),
            name: slot_id.into(),
            role: TeammateRole::Teammate,
            conversation_id: format!("conv-{slot_id}"),
            backend: "claude".into(),
            model: String::new(),
            assistant_id: assistant_id.map(str::to_owned),
            status: None,
            conversation_type: None,
            cli_path: None,
        }
    }

    /// `reviewer` reviews, `broken` cannot be read, everything else is a plain role.
    struct FakeRoles;

    #[async_trait]
    impl TeamRolePort for FakeRoles {
        async fn role_brief(&self, assistant_id: &str) -> Result<Option<TeamRoleBrief>, String> {
            match assistant_id {
                "reviewer" => Ok(Some(TeamRoleBrief {
                    verifies: true,
                    ..TeamRoleBrief::default()
                })),
                "broken" => Err("role definition is unreadable".into()),
                "not-a-role" => Ok(None),
                _ => Ok(Some(TeamRoleBrief::default())),
            }
        }
    }

    #[test]
    fn a_review_mark_round_trips_and_keeps_the_other_metadata() {
        let marked = with_review_mark(
            Some(&json!({ "priority": "high" })),
            Some(ReviewState::Waiting),
            "worker-1",
            42,
        );
        assert_eq!(marked["priority"], "high");
        assert_eq!(
            marked["review"],
            json!({ "state": "waiting", "by": "worker-1", "at": 42 })
        );
        assert_eq!(
            review_state(&task(TaskStatus::InProgress, Some(marked.clone()))),
            Some(ReviewState::Waiting)
        );

        let cleared = with_review_mark(Some(&marked), None, "worker-1", 43);
        assert_eq!(cleared, json!({ "priority": "high" }));
        assert_eq!(review_state(&task(TaskStatus::InProgress, Some(cleared))), None);
        assert_eq!(review_state(&task(TaskStatus::InProgress, None)), None);
    }

    #[test]
    fn a_member_that_does_not_review_cannot_complete_while_a_reviewer_is_in_the_team() {
        assert_eq!(
            rule_on_update(TaskStatus::Completed, None, false, true),
            ReviewRuling::Hold { already_waiting: false }
        );
        assert_eq!(
            rule_on_update(TaskStatus::Completed, Some(ReviewState::Returned), false, true),
            ReviewRuling::Hold { already_waiting: false }
        );
        // Asking again does not hand it in twice.
        assert_eq!(
            rule_on_update(TaskStatus::Completed, Some(ReviewState::Waiting), false, true),
            ReviewRuling::Hold { already_waiting: true }
        );
    }

    #[test]
    fn a_team_without_a_reviewer_is_left_alone() {
        for requested in [TaskStatus::Pending, TaskStatus::InProgress, TaskStatus::Completed] {
            assert_eq!(rule_on_update(requested, None, false, false), ReviewRuling::Untouched);
        }
        // A hold left over from a reviewer who has since gone is dropped.
        assert_eq!(
            rule_on_update(TaskStatus::Completed, Some(ReviewState::Waiting), false, false),
            ReviewRuling::Withdraw
        );
    }

    #[test]
    fn a_reviewer_passes_or_returns_a_waiting_task_and_completes_its_own_directly() {
        let waiting = Some(ReviewState::Waiting);
        assert_eq!(
            rule_on_update(TaskStatus::Completed, waiting, true, false),
            ReviewRuling::Pass
        );
        assert_eq!(
            rule_on_update(TaskStatus::InProgress, waiting, true, false),
            ReviewRuling::Return
        );
        assert_eq!(
            rule_on_update(TaskStatus::Pending, waiting, true, true),
            ReviewRuling::Return
        );
        assert_eq!(
            rule_on_update(TaskStatus::Deleted, waiting, true, false),
            ReviewRuling::Withdraw
        );
        assert_eq!(
            rule_on_update(TaskStatus::Completed, None, true, true),
            ReviewRuling::Untouched
        );
    }

    #[test]
    fn taking_a_task_back_to_work_drops_an_open_mark() {
        assert_eq!(
            rule_on_update(TaskStatus::InProgress, Some(ReviewState::Waiting), false, true),
            ReviewRuling::Withdraw
        );
        assert_eq!(
            rule_on_update(TaskStatus::InProgress, Some(ReviewState::Returned), false, true),
            ReviewRuling::Withdraw
        );
        assert_eq!(
            rule_on_update(TaskStatus::InProgress, Some(ReviewState::Passed), false, true),
            ReviewRuling::Untouched
        );
    }

    #[tokio::test]
    async fn reviewers_are_the_members_spawned_from_a_reviewing_role() {
        let agents = vec![
            agent("lead-1", None),
            agent("worker-1", Some("coder")),
            agent("worker-2", Some("not-a-role")),
            agent("checker-1", Some("reviewer")),
        ];

        let reviewers = reviewers_among(agents, &FakeRoles).await.unwrap();

        assert_eq!(reviewers.len(), 1);
        assert_eq!(reviewers[0].slot_id, "checker-1");
    }

    #[tokio::test]
    async fn an_unreadable_role_is_an_error_rather_than_an_open_gate() {
        let agents = vec![agent("worker-1", Some("coder")), agent("checker-1", Some("broken"))];

        let error = reviewers_among(agents, &FakeRoles).await.unwrap_err();

        assert!(error.contains("checker-1"));
    }

    #[test]
    fn the_status_label_spells_out_an_open_review() {
        let waiting = with_review_mark(None, Some(ReviewState::Waiting), "worker-1", 1);
        let returned = with_review_mark(None, Some(ReviewState::Returned), "checker-1", 1);
        let passed = with_review_mark(None, Some(ReviewState::Passed), "checker-1", 1);

        assert_eq!(
            status_label(&task(TaskStatus::InProgress, Some(waiting))),
            "in_progress (waiting for review)"
        );
        assert_eq!(
            status_label(&task(TaskStatus::InProgress, Some(returned))),
            "in_progress (returned from review)"
        );
        assert_eq!(status_label(&task(TaskStatus::Completed, Some(passed))), "completed");
        assert_eq!(status_label(&task(TaskStatus::Pending, None)), "pending");
    }

    #[test]
    fn notices_name_the_task_and_say_what_to_do_next() {
        let task = task(TaskStatus::InProgress, None);

        let request = review_request_notice(&task, "worker-1");
        assert!(request.contains("(#tk1): Build the page"));
        assert!(request.contains("With tests."));
        assert!(request.contains("Handed in by worker-1"));
        assert!(request.contains("status \"completed\""));

        let returned = review_returned_notice(&task, "checker-1");
        assert!(returned.contains("(#tk1): Build the page"));
        assert!(returned.contains("checker-1 did not pass it"));

        let passed = review_passed_notice(&task, "checker-1");
        assert!(passed.contains("Passed by checker-1"));
    }
}
