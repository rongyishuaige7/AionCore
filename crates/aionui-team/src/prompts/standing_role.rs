//! The section a member spawned from a resident role receives with its role
//! prompt: the charter, the role's memory, and the team tools it may not call.

use std::path::Path;

use crate::ports::TeamRoleBrief;

/// Cap on the memory text placed in the prompt. The file itself may be longer;
/// the member is pointed at it for the rest.
const MAX_INJECTED_MEMORY_CHARS: usize = 8_000;

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
    } else if memory.chars().count() > MAX_INJECTED_MEMORY_CHARS {
        out.extend(memory.chars().take(MAX_INJECTED_MEMORY_CHARS));
        out.push_str("\n\n[Memory cut here; the rest is in the file below.]\n\n");
    } else {
        out.push_str(memory);
        out.push_str("\n\n");
    }
    let memory_file = Path::new(&brief.memory_dir).join("MEMORY.md");
    out.push_str(&format!(
        "Your memory file is `{}`. When you learn something that should outlast this team (a preference of the \
         user, a mistake not to repeat, a decision and its reason), add it there. If you cannot write to that \
         path, say in your reply what should be remembered.",
        memory_file.display()
    ));

    if !brief.denied_tools.is_empty() {
        out.push_str("\n\n### Team tools you may not call\n\n");
        for tool in &brief.denied_tools {
            out.push_str(&format!("- `{tool}`\n"));
        }
        out.push_str("\nThe backend refuses these for your role. If the work needs one, ask the Lead.");
    }
    out
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
        }
    }

    #[test]
    fn renders_charter_memory_path_and_denied_tools() {
        let section = render_standing_role(&brief());

        assert!(section.starts_with("## Your Standing Role"));
        assert!(section.contains("### Charter\n\nChecks work before it is called done."));
        assert!(section.contains("### Memory\n\n- the user wants tests run, not described"));
        assert!(section.contains("MEMORY.md"));
        assert!(section.contains("- `team_task_create`"));
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
    fn long_memory_is_cut_on_a_character_boundary() {
        let section = render_standing_role(&TeamRoleBrief {
            memory: "记".repeat(MAX_INJECTED_MEMORY_CHARS + 50),
            ..brief()
        });

        assert_eq!(section.matches('记').count(), MAX_INJECTED_MEMORY_CHARS);
        assert!(section.contains("[Memory cut here"));
    }
}
