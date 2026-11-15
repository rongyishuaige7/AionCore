//! The section a member spawned from a resident role receives with its role
//! prompt: the charter, the role's memory, and the team tools it may not call.

use std::path::Path;

use crate::ports::TeamRoleBrief;

/// Cap on the memory text placed in the prompt. The file itself may be longer;
/// the member gets its newest part and is pointed at the file for the rest.
const MAX_INJECTED_MEMORY_CHARS: usize = 8_000;

/// The team tool a role member writes its memory with.
const REMEMBER_TOOL: &str = "team_remember";

/// Render the standing-role section. No trailing newline.
pub fn render_standing_role(brief: &TeamRoleBrief) -> String {
    let mut out = String::from(
        "## Your Standing Role\n\n\
         You are a resident role. The charter and memory below stay with you across every team you join.\n\n",
    );

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
            "The user keeps this memory (`{}`). When you learn something that should outlast this team, say in \
             your reply what should be remembered.",
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

    if brief.verifies {
        out.push_str(
            "\n\n### Reviewing\n\n\
             You review work in every team you join. When another member marks a task completed, the backend holds \
             it and hands it to you. Check the work itself (run it, read it), not the account of it. To pass a \
             task, call `team_task_update` with status `completed`. To return it, call `team_task_update` with \
             status `in_progress` and tell the owner what is missing with `team_send_message`.",
        );
    }

    if !brief.denied_tools.is_empty() {
        out.push_str("\n\n### Team tools you may not call\n\n");
        for tool in &brief.denied_tools {
            out.push_str(&format!("- `{tool}`\n"));
        }
        out.push_str("\nThe backend refuses these for your role. If the work needs one, ask the Lead.");
    }
    out
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
        }
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
