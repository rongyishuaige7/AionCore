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
//! roles/recall.db                                   rebuildable FTS5 index over threads, runs & leaves
//! ```
//!
//! The team runtime consumes roles through `aionui_team::TeamRolePort`; the
//! composition layer adapts [`RoleService`] to that port.

mod error;
mod recall;
mod service;
mod state;
mod store;

pub mod routes;

pub use error::RoleError;
pub use recall::{
    DEFAULT_EXPAND_WINDOW, DEFAULT_RECALL_LIMIT, MAX_EXPAND_WINDOW, MAX_RECALL_LIMIT, MAX_RECALL_QUERY_CHARS,
    RecallConversationSync, RecallMessage, RecallSync, RecallWatermark, leaf_ref_id, message_ref_id,
};
pub use routes::role_routes;
pub use service::{
    CuratedMemory, MAX_CHARTER_CHARS, MAX_MEMORY_ARCHIVES, MAX_MEMORY_BYTES, MAX_NOTE_CHARS, MAX_PROGRESS_STATE_CHARS,
    MAX_PROGRESS_TASKS, MAX_TASK_NAME_CHARS, ProgressUpdate, RememberedNote, RoleService, ThreadMark, ThreadTurn,
    ThreadUpkeep,
};
pub use state::RoleRouterState;
