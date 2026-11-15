//! Wakes the members a quiet team's open tasks are waiting on.
//!
//! Who is woken and how often is decided in [`crate::auto_continue`]; this
//! file reads the team's state, delivers the reminders and tells the page.

use aionui_api_types::{TeamAutoContinuePayload, TeamAutoContinueStatus};
use tracing::{info, warn};

use super::TeamSession;
use crate::auto_continue::{self, Budget, Halt, Member, Nudge, State, Verdict};
use crate::error::TeamError;
use crate::member_runtime::MemberRuntimeSnapshot;
use crate::task_review;
use crate::types::{MailboxMessageType, TeammateRole};
use crate::work_coordinator::{CoordinatorSnapshot, RuntimeConstraint, SlotPhase};
use crate::work_source::WorkSource;

/// Mailbox sender of a reminder, the one every system notice uses.
const REMINDER_SENDER: &str = "team_system";

/// Whether no member is working or about to: nothing running, nothing queued
/// that will run, and no message halfway into a queue.
fn team_is_quiet(snapshot: &CoordinatorSnapshot) -> bool {
    let busy = snapshot.slots.iter().any(|slot| match slot.state {
        SlotPhase::Queued | SlotPhase::Starting | SlotPhase::Running => true,
        // Queued work behind a process that is still starting will run.
        SlotPhase::Blocked => {
            matches!(slot.runtime_constraint, RuntimeConstraint::Starting { .. })
                && slot.queued_foreground_count + slot.queued_background_count > 0
        }
        // A paused member whose turn has not wound down yet.
        SlotPhase::Paused => slot.active_batch.is_some(),
        SlotPhase::Idle => false,
    });
    let enqueueing = snapshot
        .active_run_summary
        .as_ref()
        .is_some_and(|run| run.active_enqueue_lease_count > 0);
    !busy && !enqueueing
}

fn status_of(state: State) -> TeamAutoContinueStatus {
    match state {
        // A stop by the user needs no explaining on the page.
        State::Idle | State::Halted(Halt::UserStop) => TeamAutoContinueStatus::Idle,
        State::Running => TeamAutoContinueStatus::Running,
        State::Halted(Halt::Stalled) => TeamAutoContinueStatus::Stalled,
        State::Halted(Halt::RoundLimit) => TeamAutoContinueStatus::RoundLimit,
    }
}

impl TeamSession {
    /// Called when a member has run out of work. If that leaves the whole team
    /// quiet while the board still holds open tasks, reminds the members those
    /// tasks are waiting on — unless the reminders have run out.
    pub(crate) async fn continue_open_work(&self) {
        // Held to the end: of two members going quiet together, the second
        // finds the first one's reminders already queued.
        let mut budget = self.auto_continue.lock().await;
        let snapshot = self.work_coordinator.snapshot();
        if !team_is_quiet(&snapshot) {
            return;
        }
        let tasks = match self.scheduler.list_tasks().await {
            Ok(tasks) => tasks,
            Err(error) => {
                warn!(team_id = %self.team.id, error = %error, "auto-continue: the task board could not be read");
                return;
            }
        };
        let open_tasks = auto_continue::open_task_count(&tasks);
        let nudges = auto_continue::plan(&tasks, &self.reminder_members(&snapshot).await);
        if nudges.is_empty() {
            // Nothing open, or nothing a member can act on: the round is over.
            if budget.settle() {
                self.announce_auto_continue(&budget, open_tasks);
            }
            return;
        }

        match budget.admit(&auto_continue::board_mark(&tasks)) {
            None => {}
            Some(Verdict::Halt(halt)) => {
                info!(
                    team_id = %self.team.id,
                    reason = ?halt,
                    rounds = budget.rounds(),
                    open_tasks,
                    "auto-continue: reminders ended with open work on the board"
                );
                self.announce_auto_continue(&budget, open_tasks);
            }
            Some(Verdict::Remind { round, last_reminder }) => {
                let mut delivered = 0;
                for nudge in &nudges {
                    match self.deliver_reminder(nudge, last_reminder).await {
                        Ok(()) => delivered += 1,
                        Err(error) => warn!(
                            team_id = %self.team.id,
                            slot_id = %nudge.slot_id,
                            error = %error,
                            "auto-continue: a reminder could not be delivered"
                        ),
                    }
                }
                info!(
                    team_id = %self.team.id,
                    round,
                    last_reminder,
                    reminded = delivered,
                    open_tasks,
                    "auto-continue: the team went quiet with open work"
                );
                if delivered == 0 {
                    // Nobody was woken, so nothing is running.
                    budget.settle();
                }
                self.announce_auto_continue(&budget, open_tasks);
            }
        }
    }

    /// The user wrote: the reminders start over.
    pub(super) async fn restart_auto_continue(&self) {
        let mut budget = self.auto_continue.lock().await;
        let shown = status_of(budget.state()) != TeamAutoContinueStatus::Idle;
        budget.reset();
        if shown {
            let open_tasks = self.open_task_count().await;
            self.announce_auto_continue(&budget, open_tasks);
        }
    }

    /// The user stopped something: no reminders until they write again.
    pub(super) async fn halt_auto_continue(&self) {
        let mut budget = self.auto_continue.lock().await;
        if matches!(budget.state(), State::Halted(_)) {
            return;
        }
        let was_running = budget.state() == State::Running;
        budget.stop();
        if was_running {
            let open_tasks = self.open_task_count().await;
            self.announce_auto_continue(&budget, open_tasks);
        }
    }

    /// The stop button beside "continuing by itself": ends the reminders and
    /// whatever the team is doing right now.
    pub(crate) async fn stop_auto_continue(&self) -> Result<(), TeamError> {
        self.halt_auto_continue().await;
        let Some(team_run_id) = self.team_run_manager.current_active_run_id() else {
            return Ok(());
        };
        match self
            .cancel_run(&team_run_id, None, Some("auto_continue_stopped".into()))
            .await
        {
            // The run ended by itself in between: there is nothing left to stop.
            Err(TeamError::InvalidRequest(_))
                if self.team_run_manager.current_active_run_id().as_deref() != Some(team_run_id.as_str()) =>
            {
                Ok(())
            }
            result => result,
        }
    }

    /// Where the reminders stand, for a page that just opened.
    pub(crate) async fn auto_continue_payload(&self) -> TeamAutoContinuePayload {
        let open_tasks = self.open_task_count().await;
        let budget = self.auto_continue.lock().await;
        self.auto_continue_snapshot(&budget, open_tasks)
    }

    fn auto_continue_snapshot(&self, budget: &Budget, open_tasks: usize) -> TeamAutoContinuePayload {
        TeamAutoContinuePayload {
            team_id: self.team.id.clone(),
            status: status_of(budget.state()),
            round: budget.rounds(),
            open_tasks,
        }
    }

    fn announce_auto_continue(&self, budget: &Budget, open_tasks: usize) {
        self.team_event_emitter()
            .broadcast_auto_continue(self.auto_continue_snapshot(budget, open_tasks));
    }

    async fn open_task_count(&self) -> usize {
        self.scheduler
            .list_tasks()
            .await
            .map(|tasks| auto_continue::open_task_count(&tasks))
            .unwrap_or(0)
    }

    /// Every member as the reminder plan sees it.
    async fn reminder_members(&self, snapshot: &CoordinatorSnapshot) -> Vec<Member> {
        let agents = self.scheduler.list_agents().await;
        // A role that cannot be read leaves no reviewer to remind; the lead is
        // told about the waiting task instead.
        let reviewers = match self.service.upgrade() {
            Some(service) => {
                let roles = service.role_port();
                task_review::reviewers_among(agents.clone(), roles.as_ref())
                    .await
                    .unwrap_or_else(|error| {
                        warn!(team_id = %self.team.id, error = %error, "auto-continue: reviewers could not be read");
                        Vec::new()
                    })
            }
            None => Vec::new(),
        };
        agents
            .iter()
            .map(|agent| {
                let paused = snapshot
                    .slots
                    .iter()
                    .any(|slot| slot.slot_id == agent.slot_id && slot.state == SlotPhase::Paused);
                let can_run = matches!(
                    self.member_runtimes.snapshot(&agent.slot_id),
                    MemberRuntimeSnapshot::Absent | MemberRuntimeSnapshot::Attaching { .. } | MemberRuntimeSnapshot::Ready
                );
                Member {
                    slot_id: agent.slot_id.clone(),
                    lead: agent.role == TeammateRole::Lead,
                    reviews: reviewers.iter().any(|reviewer| reviewer.slot_id == agent.slot_id),
                    reachable: can_run && !paused,
                }
            })
            .collect()
    }

    /// Puts the reminder in the member's mailbox and conversation, then wakes
    /// the member, starting its process first if it is dormant.
    async fn deliver_reminder(&self, nudge: &Nudge, last_reminder: bool) -> Result<(), TeamError> {
        let agent = self.scheduler.get_agent(&nudge.slot_id).await?;
        self.ensure_member_runtime_lazy(&nudge.slot_id, true).await?;
        let notice = self
            .mailbox
            .write(
                &self.team.id,
                &nudge.slot_id,
                REMINDER_SENDER,
                MailboxMessageType::Message,
                &auto_continue::render_reminder(nudge, last_reminder),
                None,
            )
            .await?;
        self.project_team_system_message(&nudge.slot_id, &agent.conversation_id, &notice.id, &notice.content)
            .await;
        self.enqueue_system_work(&nudge.slot_id, WorkSource::AutoContinue, Some(notice.id), None)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_coordinator::SlotWorkSnapshot;
    use aionui_api_types::TeamRunTargetRole;

    fn slot(state: SlotPhase, runtime_constraint: RuntimeConstraint, queued: usize) -> SlotWorkSnapshot {
        SlotWorkSnapshot {
            slot_id: "worker-1".into(),
            role: TeamRunTargetRole::Teammate,
            state,
            queued_foreground_count: 0,
            queued_background_count: queued,
            active_batch: None,
            active_turn_id: None,
            active_turn_started_at_ms: None,
            runtime_constraint,
            team_run_id: None,
        }
    }

    fn snapshot_of(slots: Vec<SlotWorkSnapshot>) -> CoordinatorSnapshot {
        CoordinatorSnapshot {
            session_generation: "gen-1".into(),
            slots,
            active_run_summary: None,
        }
    }

    #[test]
    fn a_team_with_a_member_at_work_or_about_to_be_is_not_quiet() {
        for state in [SlotPhase::Queued, SlotPhase::Starting, SlotPhase::Running] {
            assert!(
                !team_is_quiet(&snapshot_of(vec![
                    slot(SlotPhase::Idle, RuntimeConstraint::Ready, 0),
                    slot(state, RuntimeConstraint::Ready, 0),
                ])),
                "{state:?} must count as busy"
            );
        }
    }

    #[test]
    fn idle_members_and_dormant_ones_with_nothing_queued_leave_the_team_quiet() {
        assert!(team_is_quiet(&snapshot_of(Vec::new())));
        assert!(team_is_quiet(&snapshot_of(vec![
            slot(SlotPhase::Idle, RuntimeConstraint::Ready, 0),
            // Never started: the coordinator shows it blocked, but it has nothing to run.
            slot(SlotPhase::Blocked, RuntimeConstraint::Starting { operation_id: 0 }, 0),
            slot(SlotPhase::Paused, RuntimeConstraint::Ready, 1),
        ])));
    }

    #[test]
    fn mail_queued_behind_a_starting_process_keeps_the_team_busy() {
        assert!(!team_is_quiet(&snapshot_of(vec![slot(
            SlotPhase::Blocked,
            RuntimeConstraint::Starting { operation_id: 3 },
            1
        )])));
    }

    #[test]
    fn mail_queued_behind_a_failed_process_does_not_keep_the_team_busy() {
        assert!(team_is_quiet(&snapshot_of(vec![slot(
            SlotPhase::Blocked,
            RuntimeConstraint::Failed {
                operation_id: 3,
                classification: "spawn_failed",
            },
            1
        )])));
    }

    #[test]
    fn a_stop_by_the_user_is_shown_as_idle_and_the_other_endings_by_name() {
        assert_eq!(status_of(State::Idle), TeamAutoContinueStatus::Idle);
        assert_eq!(status_of(State::Running), TeamAutoContinueStatus::Running);
        assert_eq!(status_of(State::Halted(Halt::UserStop)), TeamAutoContinueStatus::Idle);
        assert_eq!(status_of(State::Halted(Halt::Stalled)), TeamAutoContinueStatus::Stalled);
        assert_eq!(
            status_of(State::Halted(Halt::RoundLimit)),
            TeamAutoContinueStatus::RoundLimit
        );
    }
}
