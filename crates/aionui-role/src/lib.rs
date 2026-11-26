#![warn(clippy::disallowed_types)]

//! Resident roles: an assistant promoted to a standing team member.
//!
//! A role adds three things to the assistant it is built on: a charter (what it
//! owns and what it refuses), a memory that outlives any single team, and a
//! table of team tools it may not call. Everything lives as plain files under
//! `<data_dir>/roles/<assistant_id>/`, so the memory stays readable and
//! editable by the user and by agents without going through the database:
//!
//! ```text
//! roles/<assistant_id>/role.json                    charter + permission table
//! roles/<assistant_id>/memory/MEMORY.md             the role's memory
//! roles/<assistant_id>/memory/archive/MEMORY-*.md   memories the role replaced when tidying
//! roles/<assistant_id>/memory/progress/<task>.md    where one task under way stands
//! roles/<assistant_id>/thread.json                  the one conversation the user keeps with it
//! roles/<assistant_id>/thread-leaves/<conv>/*.md    summaries of that conversation, oldest first
//! roles/<assistant_id>/projects/<project>.json     where work in a kept project last stopped
//! roles/<assistant_id>/debriefs.jsonl               automatic saves of kept projects' conversations
//! roles/recall.db                                   rebuildable FTS5 index over threads, runs & leaves
//! ```
//!
//! The team runtime consumes roles through `aionui_team::TeamRolePort`; the
//! composition layer adapts [`RoleService`] to that port.

mod brief;
mod error;
mod git;
mod playbook;
mod proposals;
mod recall;
mod service;
mod state;
mod store;

pub mod routes;

pub use brief::render_keeper_brief;
pub use error::RoleError;
pub use playbook::{
    MAX_BOSS_ITEMS, MAX_PLAYBOOK_ITEM_CHARS, MAX_PLAYBOOK_ITEMS, PLAYBOOK_ARCHIVE_IDLE_DAYS, PLAYBOOK_GRACE_DAYS,
    PROGRESS_ARCHIVE_IDLE_DAYS, playbook_decay_score,
};
pub use recall::{
    DEFAULT_EXPAND_WINDOW, DEFAULT_RECALL_LIMIT, MAX_EXPAND_WINDOW, MAX_RECALL_LIMIT, MAX_RECALL_QUERY_CHARS,
    RecallConversationSync, RecallMessage, RecallSync, RecallWatermark, leaf_ref_id, message_ref_id,
};
pub use routes::role_routes;
pub use service::{
    CuratedMemory, DebriefRecord, MAX_CHARTER_CHARS, MAX_KEPT_DEBRIEFS, MAX_MEMORY_ARCHIVES, MAX_MEMORY_BYTES,
    MAX_NOTE_CHARS, MAX_PROGRESS_STATE_CHARS, MAX_PROGRESS_TASKS, MAX_TASK_NAME_CHARS, ProgressUpdate, RememberedNote,
    RoleService, RunDeclaration, ThreadMark, ThreadTurn, ThreadUpkeep,
};
pub use state::{RoleExternalPort, RoleRouterState};
