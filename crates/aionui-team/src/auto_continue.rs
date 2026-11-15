//! Keeps a team working while its task board still holds open work.
//!
//! A team stops once every member has ended its turn. That used to be final: a
//! board with tasks nobody had finished stayed that way until the user wrote
//! again. Now, when the team goes quiet, the members the open tasks are
//! waiting on are woken with a reminder.
//!
//! How often is decided here, not by the prompt: reminders stop after
//! [`MAX_STALLED_ROUNDS`] in a row that left the board unchanged, after
//! [`MAX_ROUNDS_PER_REQUEST`] in total since the user last wrote, and as soon
//! as the user stops the team.

use crate::task_review::{self, ReviewState};
use crate::types::{TaskStatus, TeamTask};

/// Reminder rounds allowed in a row without the board changing.
pub(crate) const MAX_STALLED_ROUNDS: u32 = 2;

/// Reminder rounds allowed between two messages from the user, however much
/// the board moves in between.
pub(crate) const MAX_ROUNDS_PER_REQUEST: u32 = 20;

/// Owner id of a task that waits on the user rather than on a member.
const USER_OWNER: &str = "user";

/// Owner id of a task anyone may take.
const ANYONE_OWNER: &str = "*";

/// What a reminder needs to know about one team member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub(crate) slot_id: String,
    pub(crate) lead: bool,
    /// Spawned from a role that reviews finished tasks.
    pub(crate) reviews: bool,
    /// Able to take a turn: not paused, not failed, not being removed.
    pub(crate) reachable: bool,
}

/// Why a task is put in front of the member being reminded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    /// The member owns it.
    Yours,
    /// The member owns it and a reviewer sent it back.
    Returned,
    /// It was handed in and waits for this reviewer.
    YourReview,
    /// Nobody owns it; the lead has to place it.
    Unowned,
    /// Its owner cannot take a turn; the lead has to place it.
    OwnerGone,
    /// It waits for review and no reviewer can take a turn.
    NoReviewer,
}

/// One open task in a reminder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenTask {
    pub(crate) id: String,
    pub(crate) subject: String,
    pub(crate) status: TaskStatus,
    pub(crate) why: Why,
}

/// The open tasks one member is reminded of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Nudge {
    pub(crate) slot_id: String,
    pub(crate) tasks: Vec<OpenTask>,
}

fn is_open(task: &TeamTask) -> bool {
    matches!(task.status, TaskStatus::Pending | TaskStatus::InProgress)
}

/// How many tasks on the board are neither completed nor deleted.
pub(crate) fn open_task_count(tasks: &[TeamTask]) -> usize {
    tasks.iter().filter(|task| is_open(task)).count()
}

/// Who to wake about which open tasks, in board order.
///
/// Left out: a task that waits on the user, and a task blocked by another open
/// task — its owner is told when the blocker completes, and the blocker itself
/// is in the plan. A task whose member cannot take a turn goes to the lead; if
/// the lead cannot either, nobody is woken about it.
pub(crate) fn plan(tasks: &[TeamTask], members: &[Member]) -> Vec<Nudge> {
    let lead = members.iter().find(|member| member.lead && member.reachable);
    let mut nudges: Vec<Nudge> = Vec::new();
    for task in tasks.iter().filter(|task| is_open(task)) {
        let owner = task.owner.as_deref().filter(|owner| *owner != ANYONE_OWNER);
        if owner == Some(USER_OWNER) {
            continue;
        }
        let blocked = task
            .blocked_by
            .iter()
            .any(|blocker| tasks.iter().any(|other| other.id == *blocker && is_open(other)));
        if blocked {
            continue;
        }

        let owning_member = owner.and_then(|owner| members.iter().find(|member| member.slot_id == owner));
        let review = task_review::review_state(task);
        let (target, why) = if review == Some(ReviewState::Waiting) {
            let reviewer = members
                .iter()
                .find(|member| member.reviews && member.reachable && Some(member.slot_id.as_str()) != owner);
            match reviewer {
                Some(reviewer) => (Some(reviewer), Why::YourReview),
                None => (lead, Why::NoReviewer),
            }
        } else {
            match owning_member {
                Some(member) if member.reachable && review == Some(ReviewState::Returned) => {
                    (Some(member), Why::Returned)
                }
                Some(member) if member.reachable => (Some(member), Why::Yours),
                Some(_) => (lead, Why::OwnerGone),
                None if owner.is_some() => (lead, Why::OwnerGone),
                None => (lead, Why::Unowned),
            }
        };
        let Some(target) = target else {
            continue;
        };

        let open_task = OpenTask {
            id: task.id.clone(),
            subject: task.subject.clone(),
            status: task.status,
            why,
        };
        match nudges.iter_mut().find(|nudge| nudge.slot_id == target.slot_id) {
            Some(nudge) => nudge.tasks.push(open_task),
            None => nudges.push(Nudge {
                slot_id: target.slot_id.clone(),
                tasks: vec![open_task],
            }),
        }
    }
    nudges
}

/// What makes two boards the same for the purpose of "did anything move":
/// which tasks exist, their status, their review mark and their owner.
pub(crate) fn board_mark(tasks: &[TeamTask]) -> String {
    let mut marks = tasks
        .iter()
        .filter(|task| task.status != TaskStatus::Deleted)
        .map(|task| {
            format!(
                "{}:{}:{}:{}",
                task.id,
                task.status,
                task_review::review_state(task).map_or("", ReviewState::as_str),
                task.owner.as_deref().unwrap_or("")
            )
        })
        .collect::<Vec<_>>();
    marks.sort();
    marks.join("|")
}

/// The reminder a member receives in its mailbox.
pub(crate) fn render_reminder(nudge: &Nudge, last_reminder: bool) -> String {
    let mut text = String::from(
        "⏭️ The team went quiet while the board still has open work. This is an automatic reminder, not a message \
         from the user.\n",
    );
    for task in &nudge.tasks {
        let why = match task.why {
            Why::Yours => "yours",
            Why::Returned => "yours, returned from review",
            Why::YourReview => "waiting for your review",
            Why::Unowned => "nobody owns it",
            Why::OwnerGone => "its owner cannot take a turn",
            Why::NoReviewer => "waiting for review, and no reviewer can take a turn",
        };
        text.push_str(&format!("\n- #{} {} — {}, {why}", task.id, task.subject, task.status));
    }

    let placing = nudge
        .tasks
        .iter()
        .any(|task| matches!(task.why, Why::Unowned | Why::OwnerGone | Why::NoReviewer));
    let reviewing = nudge.tasks.iter().any(|task| task.why == Why::YourReview);
    let working = nudge
        .tasks
        .iter()
        .any(|task| matches!(task.why, Why::Yours | Why::Returned));
    text.push('\n');
    if working {
        text.push_str(
            "\nFor a task of yours: carry on with it; if it is done, mark it completed with team_task_update; if it \
             is no longer needed, set it to deleted.",
        );
    }
    if reviewing {
        text.push_str(
            "\nFor a task waiting for your review: check the work, then pass it (status \"completed\") or return it \
             (status \"in_progress\") and tell the owner what is missing.",
        );
    }
    if placing {
        text.push_str(
            "\nFor a task nobody is on: give it an owner with team_task_update, do it yourself, or set it to deleted.",
        );
    }
    text.push_str(
        "\nIf the work cannot go on without the user, tell the user exactly what you need and end your turn: the \
         reminders stop by themselves when the board stops moving.",
    );
    if last_reminder {
        text.push_str(
            "\n\nThis is the last automatic reminder. If the board does not change, the team stops here and waits \
             for the user.",
        );
    }
    text
}

/// Why the reminders ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Halt {
    /// The user stopped the team.
    UserStop,
    /// [`MAX_STALLED_ROUNDS`] reminders in a row changed nothing on the board.
    Stalled,
    /// [`MAX_ROUNDS_PER_REQUEST`] reminders were sent since the user last wrote.
    RoundLimit,
}

/// Where a team stands with its reminders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum State {
    /// No reminder is out.
    #[default]
    Idle,
    /// Members were reminded and have not all gone quiet again.
    Running,
    /// No more reminders until the user writes.
    Halted(Halt),
}

/// The answer to "may the team be reminded once more".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Send round number `round`; `last_reminder` when one more unchanged
    /// board ends them.
    Remind { round: u32, last_reminder: bool },
    /// This attempt is the one that ended them.
    Halt(Halt),
}

/// Counts a team's reminders between two messages from the user.
#[derive(Debug, Default)]
pub(crate) struct Budget {
    state: State,
    /// Reminders sent since the user last wrote.
    rounds: u32,
    /// Reminders sent since the board last changed.
    stalled: u32,
    /// The board as it stood when the last reminder went out.
    last_board: Option<String>,
}

impl Budget {
    pub(crate) fn state(&self) -> State {
        self.state
    }

    pub(crate) fn rounds(&self) -> u32 {
        self.rounds
    }

    /// The user wrote: everything starts over.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// The user stopped the team: no reminders until they write again.
    pub(crate) fn stop(&mut self) {
        self.state = State::Halted(Halt::UserStop);
    }

    /// The team went quiet with nothing left to remind anyone of. Returns
    /// whether that ended a running round.
    pub(crate) fn settle(&mut self) -> bool {
        let was_running = self.state == State::Running;
        if was_running {
            self.state = State::Idle;
        }
        was_running
    }

    /// The team went quiet with open work and `board` on the wall. `None` once
    /// the reminders have already ended.
    pub(crate) fn admit(&mut self, board: &str) -> Option<Verdict> {
        if matches!(self.state, State::Halted(_)) {
            return None;
        }
        if self.last_board.as_deref() != Some(board) {
            self.stalled = 0;
        }
        let halt = if self.stalled >= MAX_STALLED_ROUNDS {
            Some(Halt::Stalled)
        } else if self.rounds >= MAX_ROUNDS_PER_REQUEST {
            Some(Halt::RoundLimit)
        } else {
            None
        };
        if let Some(halt) = halt {
            self.state = State::Halted(halt);
            return Some(Verdict::Halt(halt));
        }
        self.rounds += 1;
        self.stalled += 1;
        self.last_board = Some(board.to_owned());
        self.state = State::Running;
        Some(Verdict::Remind {
            round: self.rounds,
            last_reminder: self.stalled >= MAX_STALLED_ROUNDS,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn task(id: &str, status: TaskStatus, owner: Option<&str>) -> TeamTask {
        TeamTask {
            id: id.to_owned(),
            team_id: "team-1".to_owned(),
            subject: format!("subject {id}"),
            description: None,
            status,
            owner: owner.map(str::to_owned),
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            metadata: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn reviewed(mut task: TeamTask, state: &str) -> TeamTask {
        task.metadata = Some(json!({ "review": { "state": state, "by": "dev", "at": 1 } }));
        task
    }

    fn member(slot_id: &str) -> Member {
        Member {
            slot_id: slot_id.to_owned(),
            lead: false,
            reviews: false,
            reachable: true,
        }
    }

    fn team() -> Vec<Member> {
        vec![
            Member {
                lead: true,
                ..member("lead")
            },
            member("dev"),
            Member {
                reviews: true,
                ..member("qa")
            },
        ]
    }

    fn targets(nudges: &[Nudge]) -> Vec<(&str, Vec<(&str, Why)>)> {
        nudges
            .iter()
            .map(|nudge| {
                (
                    nudge.slot_id.as_str(),
                    nudge.tasks.iter().map(|task| (task.id.as_str(), task.why)).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_finished_board_wakes_nobody() {
        let tasks = [
            task("1", TaskStatus::Completed, Some("dev")),
            task("2", TaskStatus::Deleted, None),
        ];
        assert!(plan(&tasks, &team()).is_empty());
        assert_eq!(open_task_count(&tasks), 0);
    }

    #[test]
    fn an_open_task_wakes_its_owner() {
        let tasks = [
            task("1", TaskStatus::InProgress, Some("dev")),
            task("2", TaskStatus::Pending, Some("dev")),
            task("3", TaskStatus::Completed, Some("dev")),
        ];
        assert_eq!(
            targets(&plan(&tasks, &team())),
            vec![("dev", vec![("1", Why::Yours), ("2", Why::Yours)])]
        );
    }

    #[test]
    fn a_task_waiting_for_review_wakes_the_reviewer_not_its_owner() {
        let tasks = [reviewed(task("1", TaskStatus::InProgress, Some("dev")), "waiting")];
        assert_eq!(
            targets(&plan(&tasks, &team())),
            vec![("qa", vec![("1", Why::YourReview)])]
        );
    }

    #[test]
    fn a_returned_task_wakes_its_owner_again() {
        let tasks = [reviewed(task("1", TaskStatus::InProgress, Some("dev")), "returned")];
        assert_eq!(
            targets(&plan(&tasks, &team())),
            vec![("dev", vec![("1", Why::Returned)])]
        );
    }

    #[test]
    fn a_task_nobody_can_work_on_goes_to_the_lead() {
        let mut members = team();
        members[1].reachable = false;
        let tasks = [
            task("1", TaskStatus::Pending, None),
            task("2", TaskStatus::Pending, Some("*")),
            task("3", TaskStatus::InProgress, Some("dev")),
            task("4", TaskStatus::InProgress, Some("left-the-team")),
        ];
        assert_eq!(
            targets(&plan(&tasks, &members)),
            vec![(
                "lead",
                vec![
                    ("1", Why::Unowned),
                    ("2", Why::Unowned),
                    ("3", Why::OwnerGone),
                    ("4", Why::OwnerGone)
                ]
            )]
        );
    }

    #[test]
    fn a_review_with_no_reviewer_left_goes_to_the_lead() {
        let mut members = team();
        members[2].reachable = false;
        let tasks = [reviewed(task("1", TaskStatus::InProgress, Some("dev")), "waiting")];
        assert_eq!(
            targets(&plan(&tasks, &members)),
            vec![("lead", vec![("1", Why::NoReviewer)])]
        );
    }

    #[test]
    fn a_task_waiting_on_the_user_wakes_nobody() {
        let tasks = [task("1", TaskStatus::Pending, Some("user"))];
        assert!(plan(&tasks, &team()).is_empty());
        assert_eq!(open_task_count(&tasks), 1);
    }

    #[test]
    fn a_blocked_task_waits_for_its_blocker() {
        let mut blocked = task("2", TaskStatus::Pending, Some("qa"));
        blocked.blocked_by = vec!["1".to_owned()];
        let tasks = [task("1", TaskStatus::InProgress, Some("dev")), blocked.clone()];
        assert_eq!(targets(&plan(&tasks, &team())), vec![("dev", vec![("1", Why::Yours)])]);

        // Once the blocker is done the task is open work like any other.
        let tasks = [task("1", TaskStatus::Completed, Some("dev")), blocked];
        assert_eq!(targets(&plan(&tasks, &team())), vec![("qa", vec![("2", Why::Yours)])]);
    }

    #[test]
    fn nobody_is_woken_when_neither_the_owner_nor_the_lead_can_take_a_turn() {
        let mut members = team();
        members[0].reachable = false;
        members[1].reachable = false;
        let tasks = [task("1", TaskStatus::InProgress, Some("dev"))];
        assert!(plan(&tasks, &members).is_empty());
    }

    #[test]
    fn the_reminders_end_after_two_rounds_that_changed_nothing() {
        let mut budget = Budget::default();
        assert_eq!(
            budget.admit("a"),
            Some(Verdict::Remind {
                round: 1,
                last_reminder: false
            })
        );
        assert_eq!(
            budget.admit("a"),
            Some(Verdict::Remind {
                round: 2,
                last_reminder: true
            })
        );
        assert_eq!(budget.admit("a"), Some(Verdict::Halt(Halt::Stalled)));
        assert_eq!(budget.state(), State::Halted(Halt::Stalled));
        // Ended is ended, even if the board moves afterwards.
        assert_eq!(budget.admit("b"), None);
    }

    #[test]
    fn a_board_that_moves_keeps_the_reminders_coming() {
        let mut budget = Budget::default();
        for round in 1..=6 {
            assert_eq!(
                budget.admit(&format!("board {round}")),
                Some(Verdict::Remind {
                    round,
                    last_reminder: false
                })
            );
        }
    }

    #[test]
    fn a_board_that_keeps_moving_still_runs_into_the_round_limit() {
        let mut budget = Budget::default();
        for round in 1..=MAX_ROUNDS_PER_REQUEST {
            assert!(matches!(
                budget.admit(&format!("board {round}")),
                Some(Verdict::Remind { .. })
            ));
        }
        assert_eq!(budget.admit("one more"), Some(Verdict::Halt(Halt::RoundLimit)));
    }

    #[test]
    fn a_user_stop_ends_the_reminders_until_the_user_writes_again() {
        let mut budget = Budget::default();
        budget.admit("a");
        budget.stop();
        assert_eq!(budget.admit("b"), None);

        budget.reset();
        assert_eq!(budget.state(), State::Idle);
        assert_eq!(
            budget.admit("b"),
            Some(Verdict::Remind {
                round: 1,
                last_reminder: false
            })
        );
    }

    #[test]
    fn a_cleared_board_ends_a_running_round_once() {
        let mut budget = Budget::default();
        assert!(!budget.settle(), "nothing was running");
        budget.admit("a");
        assert_eq!(budget.state(), State::Running);
        assert!(budget.settle());
        assert_eq!(budget.state(), State::Idle);
        assert!(!budget.settle());
    }

    #[test]
    fn settling_does_not_reopen_reminders_that_ended() {
        let mut budget = Budget::default();
        budget.stop();
        assert!(!budget.settle());
        assert_eq!(budget.state(), State::Halted(Halt::UserStop));
    }

    #[test]
    fn the_board_mark_follows_status_review_and_owner_but_not_wording() {
        let base = [task("1", TaskStatus::InProgress, Some("dev"))];
        let mut reworded = base.clone();
        reworded[0].subject = "something else".to_owned();
        reworded[0].description = Some("more detail".to_owned());
        assert_eq!(board_mark(&base), board_mark(&reworded));

        let done = [task("1", TaskStatus::Completed, Some("dev"))];
        let handed_in = [reviewed(task("1", TaskStatus::InProgress, Some("dev")), "waiting")];
        let reassigned = [task("1", TaskStatus::InProgress, Some("qa"))];
        let grown = [base[0].clone(), task("2", TaskStatus::Pending, None)];
        for changed in [&done[..], &handed_in[..], &reassigned[..], &grown[..]] {
            assert_ne!(board_mark(&base), board_mark(changed));
        }

        // A deleted task is gone from the board, wherever it sits in the list.
        let with_deleted = [task("0", TaskStatus::Deleted, None), base[0].clone()];
        assert_eq!(board_mark(&base), board_mark(&with_deleted));
    }

    #[test]
    fn a_reminder_names_each_task_and_what_is_expected() {
        let nudge = Nudge {
            slot_id: "lead".to_owned(),
            tasks: vec![
                OpenTask {
                    id: "7".to_owned(),
                    subject: "Ship the fix".to_owned(),
                    status: TaskStatus::InProgress,
                    why: Why::Yours,
                },
                OpenTask {
                    id: "8".to_owned(),
                    subject: "Write the notes".to_owned(),
                    status: TaskStatus::Pending,
                    why: Why::Unowned,
                },
            ],
        };

        let text = render_reminder(&nudge, false);
        assert!(text.contains("- #7 Ship the fix — in_progress, yours"));
        assert!(text.contains("- #8 Write the notes — pending, nobody owns it"));
        assert!(text.contains("mark it completed with team_task_update"));
        assert!(text.contains("give it an owner"));
        assert!(!text.contains("waiting for your review"));
        assert!(!text.contains("last automatic reminder"));

        assert!(render_reminder(&nudge, true).contains("last automatic reminder"));
    }
}
