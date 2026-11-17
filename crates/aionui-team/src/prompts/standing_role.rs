//! The section a resident role receives when it is put to work: the charter,
//! the role's memory, the work it left under way and, in a team, the team
//! tools it may not call.

use std::path::Path;

use crate::ports::{TeamRoleBrief, TeamRoleProgress};

/// Cap on the memory text placed in the prompt. The file itself may be longer;
/// the member gets its newest part and is pointed at the file for the rest.
const MAX_INJECTED_MEMORY_CHARS: usize = 8_000;

/// The team tool a role member writes its memory with.
const REMEMBER_TOOL: &str = "team_remember";

/// The team tool a role member records its ongoing work with.
const PROGRESS_TOOL: &str = "team_progress";

/// The helper a role run outside a team reads and writes its memory with. The
/// stdin JSON arrives through a quoted here-document, so a note may hold any
/// quote character.
const ROLE_HELPER: &str = "\"$AIONUI_HELPER_BIN\" role";

/// Cap on the tasks in progress listed in the prompt; the rest stay in the directory.
const MAX_LISTED_TASKS: usize = 20;

/// Cap on the earlier reports listed in the prompt.
const MAX_LISTED_REPORTS: usize = 10;

/// Cap on the summary shown for one task, in characters.
const MAX_TASK_SUMMARY_CHARS: usize = 160;

/// Where a role is put to work. Team tools exist only in a team, so the place
/// decides what the role is told about writing to its own memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setting {
    /// A member of a team, with the team tools at hand.
    Team,
    /// Run on its own by a scheduled task, with no team around it.
    Scheduled,
    /// In a conversation the user started with the role, the user at hand.
    Direct,
}

/// Render the standing-role section for a member of a team. No trailing newline.
pub fn render_standing_role(brief: &TeamRoleBrief) -> String {
    render(brief, Setting::Team)
}

/// Render the standing-role section for a role run on its own by a scheduled
/// task. It has no team tools there, so it is shown the `role` helper commands
/// that write its memory, and nothing is said about reviewing or the team
/// tools it is denied. No trailing newline.
pub fn render_scheduled_standing_role(brief: &TeamRoleBrief) -> String {
    render(brief, Setting::Scheduled)
}

/// Render the standing-role section for a conversation the user started with
/// the role. Like a scheduled run it has no team tools and writes its memory
/// with the `role` helper; unlike one, the user is there to read the reply, so
/// it declares no outcome. No trailing newline.
pub fn render_direct_standing_role(brief: &TeamRoleBrief) -> String {
    render(brief, Setting::Direct)
}

fn render(brief: &TeamRoleBrief, setting: Setting) -> String {
    let in_team = setting == Setting::Team;
    // What a lesson or a piece of work has to outlast to be worth keeping.
    let here = match setting {
        Setting::Team => "this team",
        Setting::Scheduled => "this run",
        Setting::Direct => "this conversation",
    };
    let mut out = String::from("## Your Standing Role\n\n");
    out.push_str(match setting {
        Setting::Team => {
            "You are a resident role. The charter and memory below stay with you across every team you join.\n\n"
        }
        Setting::Scheduled => {
            "You are a resident role. The charter and memory below stay with you across every scheduled run and \
             every team you join.\n\n"
        }
        Setting::Direct => {
            "You are a resident role, and the user is talking with you here. The charter and memory below stay \
             with you across every conversation, every scheduled run and every team you join.\n\n"
        }
    });
    // A codex session carries the helper's commands as MCP tools as well, which
    // reach the backend from where codex's sandbox keeps the shell offline.
    out.push_str(&format!(
        "Where a `{ROLE_HELPER} <command>` call is shown below and your tools include `role_<command>` (a dash \
         becomes an underscore), call that tool with the same fields instead: it reaches the backend even where \
         your shell cannot.\n\n"
    ));

    let charter = brief.charter.trim();
    if !charter.is_empty() {
        out.push_str("### Charter\n\n");
        out.push_str(charter);
        out.push_str("\n\n");
    }

    out.push_str("### Memory\n\n");
    let memory = brief.memory.trim();
    if memory.is_empty() {
        out.push_str("Nothing recorded yet.\n\n");
    } else if let Some(newest) = newest_memory(memory, MAX_INJECTED_MEMORY_CHARS) {
        out.push_str("[Older notes are left out here; the whole memory is in the file below.]\n\n");
        out.push_str(newest);
        out.push_str("\n\n");
    } else {
        out.push_str(memory);
        out.push_str("\n\n");
    }
    let memory_file = Path::new(&brief.memory_dir).join("MEMORY.md");
    if brief.denied_tools.iter().any(|tool| tool == REMEMBER_TOOL) {
        out.push_str(&format!(
            "The user keeps this memory (`{}`). When you learn something that should outlast {here}, say in \
             your reply what should be remembered.",
            memory_file.display()
        ));
    } else if !in_team {
        out.push_str(&format!(
            "When you learn something that should outlast {here} (a preference of the user, a mistake not to \
             repeat, a decision and its reason), record one note:\n\n\
             ```\n{ROLE_HELPER} remember <<'JSON'\n{{\"note\": \"<one line>\"}}\nJSON\n```\n\n\
             The whole memory is kept in `{}`, and `{ROLE_HELPER} show` prints it. To tidy the memory itself \
             (merge what repeats, drop what no longer holds), replace it whole with `{ROLE_HELPER} curate`, which \
             takes `{{\"memory\": \"<the whole new memory>\"}}` the same way and archives the version it replaces.",
            memory_file.display()
        ));
    } else {
        out.push_str(&format!(
            "When you learn something that should outlast this team (a preference of the user, a mistake not to \
             repeat, a decision and its reason), call `{REMEMBER_TOOL}` with one note. The whole memory is kept \
             in `{}`.",
            memory_file.display()
        ));
    }

    let may_record_progress = !brief.denied_tools.iter().any(|tool| tool == PROGRESS_TOOL);
    // How a run outside a team writes where a task stands.
    let progress_command = format!(
        "```\n{ROLE_HELPER} progress <<'JSON'\n{{\"task\": \"<short name>\", \"state\": \"<what is done and what is \
         left>\"}}\nJSON\n```"
    );
    if !brief.progress.is_empty() {
        let progress_dir = Path::new(&brief.memory_dir).join("progress");
        out.push_str("\n\n### Work in progress\n\n");
        for progress in brief.progress.iter().take(MAX_LISTED_TASKS) {
            out.push_str(&task_line(progress));
        }
        if brief.progress.len() > MAX_LISTED_TASKS {
            out.push_str(&format!(
                "- and {} more in the directory below\n",
                brief.progress.len() - MAX_LISTED_TASKS
            ));
        }
        out.push_str(&format!(
            "\nThese are tasks you left under way, each with its state in a file in `{}`. Read a task's file \
             before you pick it up.",
            progress_dir.display()
        ));
        if may_record_progress && in_team {
            out.push_str(&format!(
                " Keep it current with `{PROGRESS_TOOL}`, and send an empty state when the task is finished."
            ));
        } else if may_record_progress {
            out.push_str(&format!(
                " Keep it current, and send an empty state when the task is finished:\n\n{progress_command}"
            ));
        } else if !in_team {
            out.push_str(" When you move one forward, say in your reply where it now stands.");
        }
    } else if may_record_progress && in_team {
        out.push_str(&format!(
            "\n\nFor work that will outlast this team, record where it stands with `{PROGRESS_TOOL}`, one task \
             per call, so that you can pick it up in another team."
        ));
    } else if may_record_progress {
        let later = if setting == Setting::Direct {
            "you can pick it up in a later run or conversation"
        } else {
            "the next run can pick it up"
        };
        out.push_str(&format!(
            "\n\nFor work that will outlast {here}, record where it stands, one task per call, so that {later}:\
             \n\n{progress_command}"
        ));
    }

    // Open to every role: it writes nothing the user keeps.
    out.push_str(&format!(
        "\n\n### Coming back to something\n\n\
         When a matter cannot be finished now because it has to wait (a build, a reply, a date), set yourself a \
         follow-up instead of waiting for it or letting it drop:\n\n\
         ```\n{ROLE_HELPER} follow-up <<'JSON'\n{{\"note\": \"<what to pick up, written for a reader who \
         remembers nothing of this moment>\", \"after_minutes\": 30}}\nJSON\n```\n\n\
         {}",
        if in_team {
            "It reaches you once, as a message in your mailbox, after that many minutes (a week at most), with the \
             note."
        } else {
            "It wakes you once, in this conversation, after that many minutes (a week at most), with the note."
        }
    ));

    // A team member answers to its Lead, who speaks to the user, and in a
    // conversation the user reads the reply. A run on its own has nobody
    // between it and the user, so it says itself whether the user is to be
    // disturbed.
    if setting == Setting::Scheduled {
        out.push_str(&format!(
            "\n\n### Ending this run\n\n\
             Nobody is watching this run. Before your final reply, declare how it ended:\n\n\
             ```\n{ROLE_HELPER} report <<'JSON'\n{{\"outcome\": \"silent\"}}\nJSON\n```\n\n\
             - `silent`: nothing the user needs to hear, and the run asks for no attention. Use it whenever a \
             check found nothing new.\n\
             - `say`: there is something the user should know. Add `\"summary\": \"<the one line they see \
             first>\"`.\n\
             - `ask`: you cannot go on without the user's answer. The summary is the question.\n\n\
             Your reply stays in this conversation either way. A run that ends without declaring is shown to the \
             user as if it had something to say."
        ));
        if !brief.reported.is_empty() {
            out.push_str("\n\n### Already told to the user\n\n");
            for line in brief.reported.iter().take(MAX_LISTED_REPORTS) {
                out.push_str(&format!("- {line}\n"));
            }
            out.push_str("\nDo not say these again unless something about them has changed.");
        }
    }

    if in_team && brief.verifies {
        out.push_str(
            "\n\n### Reviewing\n\n\
             You review work in every team you join. When another member marks a task completed, the backend holds \
             it and hands it to you. Check the work itself (run it, read it), not the account of it. To pass a \
             task, call `team_task_update` with status `completed`. To return it, call `team_task_update` with \
             status `in_progress` and tell the owner what is missing with `team_send_message`.",
        );
    }

    if in_team && !brief.denied_tools.is_empty() {
        out.push_str("\n\n### Team tools you may not call\n\n");
        for tool in &brief.denied_tools {
            out.push_str(&format!("- `{tool}`\n"));
        }
        out.push_str("\nThe backend refuses these for your role. If the work needs one, ask the Lead.");
    }
    out
}

/// One line of the task list: the task, the day its state was last written and
/// the first line of that state.
fn task_line(progress: &TeamRoleProgress) -> String {
    let summary: String = progress
        .state
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(MAX_TASK_SUMMARY_CHARS)
        .collect();
    let day = if progress.updated_day.is_empty() {
        String::new()
    } else {
        format!(" ({})", progress.updated_day)
    };
    if summary.is_empty() {
        format!("- `{}`{day}\n", progress.task)
    } else {
        format!("- `{}`{day}: {summary}\n", progress.task)
    }
}

/// The newest part of a memory longer than `max_chars`, or `None` when the
/// whole memory fits. Notes are appended, so the newest are at the end and
/// those are the ones kept. The kept part starts on a line boundary where
/// there is one, so it never opens in the middle of a note.
fn newest_memory(memory: &str, max_chars: usize) -> Option<&str> {
    let total = memory.chars().count();
    if total <= max_chars {
        return None;
    }
    let start = memory
        .char_indices()
        .nth(total - max_chars)
        .map_or(memory.len(), |(index, _)| index);
    let tail = &memory[start..];
    if memory[..start].ends_with('\n') {
        return Some(tail);
    }
    // The cut landed inside a note: drop what is left of it.
    match tail.split_once('\n') {
        Some((_, rest)) if !rest.trim().is_empty() => Some(rest),
        _ => Some(tail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief() -> TeamRoleBrief {
        TeamRoleBrief {
            charter: "Checks work before it is called done.".into(),
            memory: "- the user wants tests run, not described".into(),
            memory_dir: "/data/roles/reviewer/memory".into(),
            denied_tools: vec!["team_task_create".into()],
            verifies: false,
            progress: Vec::new(),
            reported: Vec::new(),
        }
    }

    fn task(name: &str, state: &str, day: &str) -> TeamRoleProgress {
        TeamRoleProgress {
            task: name.into(),
            state: state.into(),
            updated_day: day.into(),
        }
    }

    #[test]
    fn a_role_with_nothing_under_way_is_told_how_to_record_ongoing_work() {
        let section = render_standing_role(&brief());

        assert!(!section.contains("### Work in progress"));
        assert!(section.contains("record where it stands with `team_progress`"));
    }

    #[test]
    fn tasks_in_progress_are_listed_by_name_day_and_first_line_only() {
        let section = render_standing_role(&TeamRoleBrief {
            progress: vec![
                task(
                    "weekly-report",
                    "\nDraft sent, figures for week 46 missing.\nDetails: ask finance.",
                    "2026-11-14",
                ),
                task("migration", "", ""),
            ],
            ..brief()
        });

        assert!(section.contains(
            "### Work in progress\n\n\
             - `weekly-report` (2026-11-14): Draft sent, figures for week 46 missing.\n\
             - `migration`\n\n\
             These are tasks you left under way"
        ));
        // The rest of a state stays in its file, which the member is pointed at.
        assert!(!section.contains("ask finance"));
        assert!(section.contains("/data/roles/reviewer/memory/progress"));
        assert!(section.contains("Keep it current with `team_progress`"));
        assert!(!section.contains("record where it stands with"));
        assert!(!section.ends_with('\n'));
    }

    #[test]
    fn a_role_denied_the_progress_tool_sees_its_tasks_but_is_not_told_to_call_it() {
        let denied = TeamRoleBrief {
            denied_tools: vec!["team_progress".into()],
            ..brief()
        };
        assert!(!render_standing_role(&denied).contains("call `team_progress`"));
        assert!(!render_standing_role(&denied).contains("with `team_progress`"));

        let section = render_standing_role(&TeamRoleBrief {
            progress: vec![task("migration", "half done", "2026-11-15")],
            ..denied
        });
        assert!(section.contains("- `migration` (2026-11-15): half done"));
        assert!(!section.contains("Keep it current"));
    }

    #[test]
    fn a_scheduled_run_gets_charter_memory_and_tasks_but_no_word_of_team_tools() {
        let section = render_scheduled_standing_role(&TeamRoleBrief {
            verifies: true,
            progress: vec![task("weekly-report", "Draft sent.", "2026-11-14")],
            ..brief()
        });

        assert!(section.starts_with("## Your Standing Role"));
        assert!(section.contains("across every scheduled run and every team you join"));
        assert!(section.contains("### Charter\n\nChecks work before it is called done."));
        assert!(section.contains("### Memory\n\n- the user wants tests run, not described"));
        assert!(section.contains("- `weekly-report` (2026-11-14): Draft sent."));
        // It writes its memory with the helper every conversation carries.
        assert!(section.contains("\"$AIONUI_HELPER_BIN\" role remember <<'JSON'\n{\"note\": \"<one line>\"}\nJSON"));
        assert!(section.contains("\"$AIONUI_HELPER_BIN\" role progress <<'JSON'\n{\"task\": "));
        assert!(section.contains("`\"$AIONUI_HELPER_BIN\" role show` prints it"));
        assert!(section.contains("replace it whole with `\"$AIONUI_HELPER_BIN\" role curate`"));
        // Nothing it is told to call exists only in a team.
        assert!(!section.contains("team_"));
        assert!(!section.contains("### Reviewing"));
        assert!(!section.contains("may not call"));
        assert!(!section.ends_with('\n'));
    }

    #[test]
    fn a_conversation_with_the_role_carries_its_charter_and_memory_but_asks_for_no_report() {
        let section = render_direct_standing_role(&brief());

        assert!(section.contains("the user is talking with you here"), "{section}");
        assert!(section.contains("### Charter\n\nChecks work before it is called done."));
        assert!(section.contains("### Memory\n\n- the user wants tests run, not described"));
        assert!(section.contains("should outlast this conversation (a preference of the user"), "{section}");
        assert!(section.contains("\"$AIONUI_HELPER_BIN\" role remember <<'JSON'"));
        assert!(section.contains("so that you can pick it up in a later run or conversation"), "{section}");
        assert!(section.contains("wakes you once, in this conversation"), "{section}");
        assert!(!section.contains("### Ending this run"));
        assert!(!section.contains("role report"));
        assert!(!section.contains("team_"));
        assert!(!section.ends_with('\n'));
    }

    #[test]
    fn a_scheduled_run_with_no_task_yet_is_shown_how_to_leave_one_for_the_next_run() {
        let section = render_scheduled_standing_role(&brief());

        assert!(!section.contains("### Work in progress"));
        assert!(section.contains("so that the next run can pick it up"));
        assert!(section.contains("\"$AIONUI_HELPER_BIN\" role progress <<'JSON'"));
    }

    #[test]
    fn a_role_is_shown_how_to_come_back_to_a_matter_wherever_it_works() {
        let scheduled = render_scheduled_standing_role(&brief());
        assert!(scheduled.contains("### Coming back to something"), "{scheduled}");
        assert!(
            scheduled.contains("\"$AIONUI_HELPER_BIN\" role follow-up <<'JSON'"),
            "{scheduled}"
        );
        assert!(scheduled.contains("\"after_minutes\": 30"), "{scheduled}");

        assert!(
            scheduled.contains("wakes you once, in this conversation"),
            "{scheduled}"
        );

        let in_team = render_standing_role(&brief());
        assert!(
            in_team.contains("\"$AIONUI_HELPER_BIN\" role follow-up <<'JSON'"),
            "{in_team}"
        );
        assert!(in_team.contains("as a message in your mailbox"), "{in_team}");
    }

    #[test]
    fn a_role_is_told_to_use_its_tools_instead_of_the_helper_where_it_has_them() {
        for section in [render_scheduled_standing_role(&brief()), render_standing_role(&brief())] {
            assert!(
                section.contains("your tools include `role_<command>` (a dash becomes an underscore)"),
                "{section}"
            );
        }
    }

    #[test]
    fn a_run_on_its_own_is_asked_to_declare_whether_the_user_is_to_be_disturbed() {
        let scheduled = render_scheduled_standing_role(&brief());
        assert!(scheduled.contains("### Ending this run"));
        assert!(scheduled.contains("\"$AIONUI_HELPER_BIN\" role report <<'JSON'"));
        for outcome in ["`silent`", "`say`", "`ask`"] {
            assert!(scheduled.contains(outcome), "{outcome} is explained");
        }
        // Nothing reported yet: no empty list.
        assert!(!scheduled.contains("### Already told to the user"));

        // In a team the Lead speaks to the user.
        let in_team = render_standing_role(&brief());
        assert!(!in_team.contains("role report"));
        assert!(!in_team.contains("### Ending this run"));
    }

    #[test]
    fn a_run_on_its_own_is_shown_the_newest_things_it_already_told_the_user() {
        let reported: Vec<String> = (0..MAX_LISTED_REPORTS + 2)
            .map(|n| format!("2026-11-14 09:{n:02} [say] report {n}"))
            .collect();
        let section = render_scheduled_standing_role(&TeamRoleBrief {
            reported: reported.clone(),
            ..brief()
        });
        assert!(section.contains("### Already told to the user"));
        assert!(section.contains("- 2026-11-14 09:00 [say] report 0\n"));
        assert!(!section.contains(&reported[MAX_LISTED_REPORTS]));
        assert!(section.contains("Do not say these again"));

        let in_team = render_standing_role(&TeamRoleBrief { reported, ..brief() });
        assert!(!in_team.contains("### Already told to the user"));
    }

    #[test]
    fn a_scheduled_run_kept_from_its_memory_is_asked_to_say_it_in_the_reply() {
        let section = render_scheduled_standing_role(&TeamRoleBrief {
            denied_tools: vec![REMEMBER_TOOL.into(), PROGRESS_TOOL.into()],
            progress: vec![task("weekly-report", "Draft sent.", "2026-11-14")],
            ..brief()
        });

        assert!(section.contains("should outlast this run, say in your reply what should be remembered"));
        assert!(section.contains("say in your reply where it now stands"));
        for kept_from in ["role remember", "role curate", "role progress", "role show"] {
            assert!(!section.contains(kept_from), "{kept_from}: {section}");
        }
        // Coming back to a matter writes nothing the user keeps.
        assert!(section.contains("role follow-up"), "{section}");
        assert!(!section.contains("team_"));
    }

    #[test]
    fn a_long_task_list_names_the_newest_and_counts_the_rest() {
        let section = render_standing_role(&TeamRoleBrief {
            progress: (0..MAX_LISTED_TASKS + 3)
                .map(|index| task(&format!("task-{index}"), "x", ""))
                .collect(),
            ..brief()
        });

        assert!(section.contains(&format!("- `task-{}`: x\n", MAX_LISTED_TASKS - 1)));
        assert!(!section.contains(&format!("- `task-{MAX_LISTED_TASKS}`")));
        assert!(section.contains("- and 3 more in the directory below\n"));
    }

    #[test]
    fn renders_charter_memory_path_and_denied_tools() {
        let section = render_standing_role(&brief());

        assert!(section.starts_with("## Your Standing Role"));
        assert!(section.contains("### Charter\n\nChecks work before it is called done."));
        assert!(section.contains("### Memory\n\n- the user wants tests run, not described"));
        assert!(section.contains("MEMORY.md"));
        assert!(section.contains("call `team_remember` with one note"));
        assert!(section.contains("- `team_task_create`"));
        assert!(!section.contains("### Reviewing"));
        assert!(!section.ends_with('\n'));
    }

    #[test]
    fn empty_role_still_names_the_memory_file_and_omits_empty_sections() {
        let section = render_standing_role(&TeamRoleBrief {
            memory_dir: "/data/roles/blank/memory".into(),
            ..TeamRoleBrief::default()
        });

        assert!(!section.contains("### Charter"));
        assert!(section.contains("Nothing recorded yet."));
        assert!(section.contains("MEMORY.md"));
        assert!(!section.contains("may not call"));
    }

    #[test]
    fn a_role_denied_the_remember_tool_is_not_told_to_call_it() {
        let section = render_standing_role(&TeamRoleBrief {
            denied_tools: vec!["team_remember".into()],
            ..brief()
        });

        assert!(!section.contains("call `team_remember`"));
        assert!(section.contains("say in your reply what should be remembered"));
        assert!(section.contains("- `team_remember`"));
    }

    #[test]
    fn a_reviewing_role_is_told_how_to_pass_and_return_a_task() {
        let section = render_standing_role(&TeamRoleBrief {
            verifies: true,
            ..brief()
        });

        assert!(section.contains("### Reviewing"));
        assert!(section.contains("status `completed`"));
        assert!(section.contains("status `in_progress`"));
        // The permission table still comes last.
        assert!(section.find("### Reviewing").unwrap() < section.find("### Team tools you may not call").unwrap());
    }

    #[test]
    fn long_memory_is_cut_on_a_character_boundary() {
        let section = render_standing_role(&TeamRoleBrief {
            memory: "记".repeat(MAX_INJECTED_MEMORY_CHARS + 50),
            ..brief()
        });

        assert_eq!(section.matches('记').count(), MAX_INJECTED_MEMORY_CHARS);
        assert!(section.contains("[Older notes are left out here"));
    }

    #[test]
    fn long_memory_keeps_its_newest_notes_whole() {
        let old_note = format!("- 2026-01-01 {}", "o".repeat(MAX_INJECTED_MEMORY_CHARS));
        let memory = format!("{old_note}\n- 2026-11-14 run the tests\n- 2026-11-15 ask before pushing");

        let section = render_standing_role(&TeamRoleBrief { memory, ..brief() });

        // The cut lands inside the oldest note, which is dropped rather than shown in part.
        assert!(!section.contains("ooo"));
        assert!(section.contains(
            "[Older notes are left out here; the whole memory is in the file below.]\n\n\
             - 2026-11-14 run the tests\n- 2026-11-15 ask before pushing\n\n"
        ));
    }

    #[test]
    fn newest_memory_is_none_for_a_memory_that_fits() {
        assert_eq!(newest_memory("- a\n- b", 7), None);
    }

    #[test]
    fn newest_memory_keeps_a_note_the_cut_lands_in_front_of() {
        // Four characters fit: exactly the last note, which starts a line.
        assert_eq!(newest_memory("- a\n- b\n- cd", 4), Some("- cd"));
        // Five do not reach the start of "- b", so that note is dropped.
        assert_eq!(newest_memory("- a\n- b\n- cd", 5), Some("- cd"));
    }
}
